use std::sync::Arc;

use analyst_runtime::{Camera2D, PaneId, PaneLayout, ScreenPoint, ViewportMetrics, WorldPoint};
use eframe::egui;
use map_scene::gpu::MapPaintCallback;
use map_scene::{MapGeometry, RadarProjection};

const PANE_GAP: f32 = 3.0;
const HEADER_HEIGHT: f32 = 26.0;
const RANGE_RINGS_KM: &[f64] = &[50.0, 100.0, 150.0, 200.0, 300.0, 400.0];

pub struct PaneTexture<'a> {
    pub handle: &'a egui::TextureHandle,
    pub camera: Camera2D,
    pub viewport: ViewportMetrics,
}

/// The retained map underlay for one pane, if the scene has geometry built for
/// the pane's current LOD. `projection` also drives the cursor's lat/lon, so
/// the readout uses the same transform the map was built with.
#[derive(Clone, Default)]
pub struct PaneMap {
    pub geometry: Option<Arc<MapGeometry>>,
    pub projection: Option<RadarProjection>,
}

pub struct PaneInteraction {
    pub clicked: bool,
    pub camera: Camera2D,
    pub camera_changed: bool,
    pub viewport: ViewportMetrics,
}

pub fn pane_rects(canvas: egui::Rect, layout: PaneLayout) -> Vec<(PaneId, egui::Rect)> {
    let pane = |index| PaneId::new(index).expect("pane index is within workstation limit");
    match layout {
        PaneLayout::One => vec![(pane(0), canvas)],
        PaneLayout::TwoHorizontal => {
            let half = (canvas.height() - PANE_GAP) * 0.5;
            let top = egui::Rect::from_min_size(canvas.min, egui::vec2(canvas.width(), half));
            let bottom = egui::Rect::from_min_size(
                egui::pos2(canvas.left(), top.bottom() + PANE_GAP),
                egui::vec2(canvas.width(), half),
            );
            vec![(pane(0), top), (pane(1), bottom)]
        }
        PaneLayout::TwoVertical => {
            let half = (canvas.width() - PANE_GAP) * 0.5;
            let left = egui::Rect::from_min_size(canvas.min, egui::vec2(half, canvas.height()));
            let right = egui::Rect::from_min_size(
                egui::pos2(left.right() + PANE_GAP, canvas.top()),
                egui::vec2(half, canvas.height()),
            );
            vec![(pane(0), left), (pane(1), right)]
        }
        PaneLayout::Four => {
            let width = (canvas.width() - PANE_GAP) * 0.5;
            let height = (canvas.height() - PANE_GAP) * 0.5;
            let top_left = egui::Rect::from_min_size(canvas.min, egui::vec2(width, height));
            let top_right = egui::Rect::from_min_size(
                egui::pos2(top_left.right() + PANE_GAP, canvas.top()),
                egui::vec2(width, height),
            );
            let bottom_left = egui::Rect::from_min_size(
                egui::pos2(canvas.left(), top_left.bottom() + PANE_GAP),
                egui::vec2(width, height),
            );
            let bottom_right = egui::Rect::from_min_size(
                egui::pos2(top_right.left(), top_right.bottom() + PANE_GAP),
                egui::vec2(width, height),
            );
            vec![
                (pane(0), top_left),
                (pane(1), top_right),
                (pane(2), bottom_left),
                (pane(3), bottom_right),
            ]
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn draw_pane(
    ui: &mut egui::Ui,
    pane: PaneId,
    rect: egui::Rect,
    active: bool,
    camera: Camera2D,
    texture: Option<PaneTexture<'_>>,
    map: &PaneMap,
    title: &str,
    status: &str,
) -> PaneInteraction {
    let response = ui.interact(
        rect,
        ui.id().with(("radar-pane", pane.get())),
        egui::Sense::click_and_drag(),
    );
    let viewport = ViewportMetrics {
        width_points: rect.width().max(1.0),
        height_points: rect.height().max(1.0),
        pixels_per_point: ui.ctx().pixels_per_point().max(1.0),
    };
    let mut updated_camera = camera;
    let mut camera_changed = false;

    if response.dragged() {
        let delta = ui.input(|input| input.pointer.delta());
        if delta.length_sq() > 0.0 {
            updated_camera.pan_by_screen_delta(delta.x, delta.y);
            camera_changed = true;
        }
    }

    if response.hovered() {
        let scroll = ui.input(|input| input.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let pointer = ui
                .input(|input| input.pointer.hover_pos())
                .unwrap_or(rect.center());
            let local = ScreenPoint::new(pointer.x - rect.left(), pointer.y - rect.top());
            let factor = (1.0 + scroll / 600.0).clamp(0.72, 1.4);
            updated_camera.zoom_about(factor, local, viewport);
            camera_changed = true;
        }
    }

    if response.double_clicked() {
        updated_camera = Camera2D::default();
        camera_changed = true;
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(6, 9, 13));

    // Map underlay first: the radar draws over it.
    paint_map(&painter, rect, pane, updated_camera, viewport, map);

    if let Some(texture) = texture {
        paint_transformed_texture(&painter, rect, updated_camera, viewport, texture);
    }
    draw_map_labels(&painter, rect, updated_camera, viewport, map);
    draw_range_rings(&painter, rect, updated_camera, viewport);
    draw_cursor_readout(
        ui,
        &painter,
        rect,
        updated_camera,
        viewport,
        response.hovered(),
        map.projection.as_ref(),
    );
    draw_header(&painter, rect, title, status);
    draw_border(&painter, rect, active);

    PaneInteraction {
        clicked: response.clicked(),
        camera: updated_camera,
        camera_changed,
        viewport,
    }
}

/// Queue the retained map for this pane.
///
/// The callback carries only a geometry handle and the camera; the vertex and
/// index buffers behind it are already on the GPU and are not touched here.
fn paint_map(
    painter: &egui::Painter,
    rect: egui::Rect,
    pane: PaneId,
    camera: Camera2D,
    viewport: ViewportMetrics,
    map: &PaneMap,
) {
    let Some(geometry) = map.geometry.clone() else {
        return;
    };
    if geometry.is_empty() {
        return;
    }
    let pixels_per_point = viewport.sanitized().pixels_per_point;
    let callback = MapPaintCallback {
        pane_index: pane.index(),
        geometry,
        camera,
        viewport,
        rect_px: [
            rect.left() * pixels_per_point,
            rect.top() * pixels_per_point,
            rect.right() * pixels_per_point,
            rect.bottom() * pixels_per_point,
        ],
    };
    painter.add(eframe::egui_wgpu::Callback::new_paint_callback(
        rect, callback,
    ));
}

fn paint_transformed_texture(
    painter: &egui::Painter,
    rect: egui::Rect,
    current_camera: Camera2D,
    current_viewport: ViewportMetrics,
    texture: PaneTexture<'_>,
) {
    let rendered_viewport = texture.viewport.sanitized();
    let rendered_corners = [
        ScreenPoint::new(0.0, 0.0),
        ScreenPoint::new(rendered_viewport.width_points, 0.0),
        ScreenPoint::new(
            rendered_viewport.width_points,
            rendered_viewport.height_points,
        ),
        ScreenPoint::new(0.0, rendered_viewport.height_points),
    ];
    let uv = [
        egui::pos2(0.0, 0.0),
        egui::pos2(1.0, 0.0),
        egui::pos2(1.0, 1.0),
        egui::pos2(0.0, 1.0),
    ];
    let mut mesh = egui::Mesh::with_texture(texture.handle.id());
    for (corner, uv) in rendered_corners.into_iter().zip(uv) {
        let world = texture.camera.screen_to_world(corner, rendered_viewport);
        let current = current_camera.world_to_screen(world, current_viewport);
        mesh.vertices.push(egui::epaint::Vertex {
            pos: egui::pos2(rect.left() + current.x, rect.top() + current.y),
            uv,
            color: egui::Color32::WHITE,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// Draw the labels that survived bounded placement.
///
/// Text is egui's, drawn after the retained geometry. The expensive part —
/// projecting every candidate — already happened in the build; this only
/// transforms the survivors and rejects overlaps.
fn draw_map_labels(
    painter: &egui::Painter,
    rect: egui::Rect,
    camera: Camera2D,
    viewport: ViewportMetrics,
    map: &PaneMap,
) {
    let Some(geometry) = map.geometry.as_ref() else {
        return;
    };
    let (placed, _metrics) =
        map_scene::place_labels(geometry, camera, viewport, map_scene::MAX_LABELS_PLACED);
    for label in placed {
        let position = egui::pos2(
            rect.left() + label.position.x,
            rect.top() + label.position.y,
        );
        // A dark halo keeps the name readable over bright reflectivity.
        for offset in [
            egui::vec2(-1.0, 0.0),
            egui::vec2(1.0, 0.0),
            egui::vec2(0.0, -1.0),
            egui::vec2(0.0, 1.0),
        ] {
            painter.text(
                position + offset,
                egui::Align2::CENTER_CENTER,
                label.name,
                egui::FontId::proportional(10.0),
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 190),
            );
        }
        painter.text(
            position,
            egui::Align2::CENTER_CENTER,
            label.name,
            egui::FontId::proportional(10.0),
            egui::Color32::from_rgb(214, 222, 232),
        );
    }
}

fn draw_range_rings(
    painter: &egui::Painter,
    rect: egui::Rect,
    camera: Camera2D,
    viewport: ViewportMetrics,
) {
    let radar = camera.world_to_screen(WorldPoint::ORIGIN, viewport);
    let center = egui::pos2(rect.left() + radar.x, rect.top() + radar.y);
    let stroke = egui::Stroke::new(
        0.8_f32,
        egui::Color32::from_rgba_unmultiplied(170, 190, 205, 88),
    );
    for range_km in RANGE_RINGS_KM {
        let radius = (*range_km as f32 / camera.sanitized().km_per_point).abs();
        if radius > 4.0 && radius < rect.width().max(rect.height()) * 2.0 {
            painter.circle_stroke(center, radius, stroke);
        }
    }
    painter.circle_filled(center, 3.2, egui::Color32::from_rgb(230, 236, 240));
}

fn draw_cursor_readout(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    camera: Camera2D,
    viewport: ViewportMetrics,
    hovered: bool,
    projection: Option<&RadarProjection>,
) {
    if !hovered {
        return;
    }
    let Some(pointer) = ui.input(|input| input.pointer.hover_pos()) else {
        return;
    };
    if !rect.contains(pointer) {
        return;
    }
    let local = ScreenPoint::new(pointer.x - rect.left(), pointer.y - rect.top());
    let world = camera.screen_to_world(local, viewport);
    let range_km = world.east_km.hypot(world.north_km);
    let azimuth_deg = world
        .east_km
        .atan2(world.north_km)
        .to_degrees()
        .rem_euclid(360.0);
    // Same inverse transform the map was built with, so the readout and the
    // basemap can never disagree.
    let text = match projection.map(|projection| projection.world_to_lon_lat(world)) {
        Some((lon_deg, lat_deg)) => format!(
            "{range_km:.1} km  {azimuth_deg:05.1}°   {:.4}°{}  {:.4}°{}",
            lat_deg.abs(),
            if lat_deg >= 0.0 { "N" } else { "S" },
            lon_deg.abs(),
            if lon_deg >= 0.0 { "E" } else { "W" },
        ),
        None => format!("{range_km:.1} km  {azimuth_deg:05.1}°"),
    };
    painter.text(
        egui::pos2(rect.left() + 8.0, rect.bottom() - 8.0),
        egui::Align2::LEFT_BOTTOM,
        text,
        egui::FontId::monospace(11.0),
        egui::Color32::from_rgb(220, 228, 234),
    );
}

fn draw_header(painter: &egui::Painter, rect: egui::Rect, title: &str, status: &str) {
    let header = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(
            rect.right(),
            (rect.top() + HEADER_HEIGHT).min(rect.bottom()),
        ),
    );
    painter.rect_filled(
        header,
        0.0,
        egui::Color32::from_rgba_unmultiplied(4, 7, 10, 218),
    );
    painter.text(
        egui::pos2(header.left() + 8.0, header.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(239, 243, 246),
    );
    painter.text(
        egui::pos2(header.right() - 8.0, header.center().y),
        egui::Align2::RIGHT_CENTER,
        status,
        egui::FontId::monospace(10.0),
        egui::Color32::from_rgb(166, 184, 196),
    );
}

fn draw_border(painter: &egui::Painter, rect: egui::Rect, active: bool) {
    let color = if active {
        egui::Color32::from_rgb(78, 180, 244)
    } else {
        egui::Color32::from_rgb(45, 57, 67)
    };
    let width = if active { 2.0_f32 } else { 1.0_f32 };
    let stroke = egui::Stroke::new(width, color);
    painter.line_segment([rect.left_top(), rect.right_top()], stroke);
    painter.line_segment([rect.right_top(), rect.right_bottom()], stroke);
    painter.line_segment([rect.right_bottom(), rect.left_bottom()], stroke);
    painter.line_segment([rect.left_bottom(), rect.left_top()], stroke);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_pane_layout_covers_each_quadrant() {
        let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0));
        let panes = pane_rects(canvas, PaneLayout::Four);
        assert_eq!(panes.len(), 4);
        assert!(panes[0].1.center().x < canvas.center().x);
        assert!(panes[1].1.center().x > canvas.center().x);
        assert!(panes[2].1.center().y > canvas.center().y);
        assert!(panes[3].1.center().y > canvas.center().y);
    }
}
