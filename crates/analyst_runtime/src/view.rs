use serde::{Deserialize, Serialize};

use crate::Generation;

pub const DEFAULT_KM_PER_POINT: f32 = 0.35;
pub const MIN_KM_PER_POINT: f32 = 0.01;
pub const MAX_KM_PER_POINT: f32 = 50.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScreenPoint {
    pub x: f32,
    pub y: f32,
}

impl ScreenPoint {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WorldPoint {
    pub east_km: f64,
    pub north_km: f64,
}

impl WorldPoint {
    pub const ORIGIN: Self = Self {
        east_km: 0.0,
        north_km: 0.0,
    };

    pub const fn new(east_km: f64, north_km: f64) -> Self {
        Self { east_km, north_km }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportMetrics {
    pub width_points: f32,
    pub height_points: f32,
    pub pixels_per_point: f32,
}

impl ViewportMetrics {
    pub fn sanitized(self) -> Self {
        Self {
            width_points: finite_positive(self.width_points, 1.0),
            height_points: finite_positive(self.height_points, 1.0),
            pixels_per_point: finite_positive(self.pixels_per_point, 1.0),
        }
    }

    pub fn center(self) -> ScreenPoint {
        let metrics = self.sanitized();
        ScreenPoint::new(metrics.width_points * 0.5, metrics.height_points * 0.5)
    }
}

/// Serializable camera intent in radar-local world kilometres.
///
/// `rotation_rad` is clockwise screen rotation. At zero rotation, east is
/// right and north is up.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Camera2D {
    pub center_east_km: f64,
    pub center_north_km: f64,
    pub km_per_point: f32,
    pub rotation_rad: f32,
}

impl Default for Camera2D {
    fn default() -> Self {
        Self {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: DEFAULT_KM_PER_POINT,
            rotation_rad: 0.0,
        }
    }
}

impl Camera2D {
    pub fn sanitized(self) -> Self {
        Self {
            center_east_km: finite_f64(self.center_east_km, 0.0),
            center_north_km: finite_f64(self.center_north_km, 0.0),
            km_per_point: finite_positive(self.km_per_point, DEFAULT_KM_PER_POINT)
                .clamp(MIN_KM_PER_POINT, MAX_KM_PER_POINT),
            rotation_rad: if self.rotation_rad.is_finite() {
                normalize_angle(self.rotation_rad)
            } else {
                0.0
            },
        }
    }

    pub fn world_to_screen(self, world: WorldPoint, viewport: ViewportMetrics) -> ScreenPoint {
        let camera = self.sanitized();
        let viewport = viewport.sanitized();
        let center = viewport.center();
        let dx = (world.east_km - camera.center_east_km) as f32;
        let dy = (world.north_km - camera.center_north_km) as f32;
        let (sin, cos) = camera.rotation_rad.sin_cos();
        let screen_x_km = cos * dx + sin * dy;
        let screen_y_km = sin * dx - cos * dy;
        ScreenPoint {
            x: center.x + screen_x_km / camera.km_per_point,
            y: center.y + screen_y_km / camera.km_per_point,
        }
    }

    pub fn screen_to_world(self, screen: ScreenPoint, viewport: ViewportMetrics) -> WorldPoint {
        let camera = self.sanitized();
        let viewport = viewport.sanitized();
        let center = viewport.center();
        let screen_x_km = (screen.x - center.x) * camera.km_per_point;
        let screen_y_km = (screen.y - center.y) * camera.km_per_point;
        let (sin, cos) = camera.rotation_rad.sin_cos();
        // The forward 2x2 matrix is its own inverse.
        let dx = cos * screen_x_km + sin * screen_y_km;
        let dy = sin * screen_x_km - cos * screen_y_km;
        WorldPoint {
            east_km: camera.center_east_km + f64::from(dx),
            north_km: camera.center_north_km + f64::from(dy),
        }
    }

    /// Move map content by a screen-space drag delta.
    pub fn pan_by_screen_delta(&mut self, delta_x_points: f32, delta_y_points: f32) {
        let camera = self.sanitized();
        let (sin, cos) = camera.rotation_rad.sin_cos();
        let screen_x_km = delta_x_points * camera.km_per_point;
        let screen_y_km = delta_y_points * camera.km_per_point;
        let world_dx = cos * screen_x_km + sin * screen_y_km;
        let world_dy = sin * screen_x_km - cos * screen_y_km;
        self.center_east_km = camera.center_east_km - f64::from(world_dx);
        self.center_north_km = camera.center_north_km - f64::from(world_dy);
        self.km_per_point = camera.km_per_point;
        self.rotation_rad = camera.rotation_rad;
    }

    /// Zoom about a screen point while preserving the world coordinate under
    /// that point. `factor > 1` zooms in.
    pub fn zoom_about(&mut self, factor: f32, anchor: ScreenPoint, viewport: ViewportMetrics) {
        let before = self.screen_to_world(anchor, viewport);
        let current = self.sanitized();
        let factor = finite_positive(factor, 1.0);
        self.km_per_point =
            (current.km_per_point / factor).clamp(MIN_KM_PER_POINT, MAX_KM_PER_POINT);
        self.center_east_km = current.center_east_km;
        self.center_north_km = current.center_north_km;
        self.rotation_rad = current.rotation_rad;
        let after = self.screen_to_world(anchor, viewport);
        self.center_east_km += before.east_km - after.east_km;
        self.center_north_km += before.north_km - after.north_km;
    }

    /// Rotate about a screen point while preserving its world coordinate.
    pub fn rotate_about(
        &mut self,
        rotation_rad: f32,
        anchor: ScreenPoint,
        viewport: ViewportMetrics,
    ) {
        let before = self.screen_to_world(anchor, viewport);
        let current = self.sanitized();
        self.center_east_km = current.center_east_km;
        self.center_north_km = current.center_north_km;
        self.km_per_point = current.km_per_point;
        self.rotation_rad = normalize_angle(rotation_rad);
        let after = self.screen_to_world(anchor, viewport);
        self.center_east_km += before.east_km - after.east_km;
        self.center_north_km += before.north_km - after.north_km;
    }

    pub fn radar_raster_view(self, viewport: ViewportMetrics) -> RasterView {
        let camera = self.sanitized();
        let viewport = viewport.sanitized();
        let radar = camera.world_to_screen(WorldPoint::ORIGIN, viewport);
        let width_px = (viewport.width_points * viewport.pixels_per_point)
            .round()
            .max(1.0) as u32;
        let height_px = (viewport.height_points * viewport.pixels_per_point)
            .round()
            .max(1.0) as u32;
        let km_per_px = camera.km_per_point / viewport.pixels_per_point;
        RasterView {
            width_px,
            height_px,
            radar_x_px: radar.x * viewport.pixels_per_point,
            radar_y_px: radar.y * viewport.pixels_per_point,
            km_per_px,
            rotation_rad: camera.rotation_rad,
        }
    }
}

/// Renderer-neutral viewport contract for the radar raster worker.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterView {
    pub width_px: u32,
    pub height_px: u32,
    pub radar_x_px: f32,
    pub radar_y_px: f32,
    pub km_per_px: f32,
    pub rotation_rad: f32,
}

/// Half-octave geometry LOD bucket. Exact camera scale is intentionally not a
/// geometry cache key.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LodBucket(pub i16);

impl LodBucket {
    pub fn ideal(km_per_point: f32, reference_km_per_point: f32) -> Self {
        let scale = finite_positive(km_per_point, DEFAULT_KM_PER_POINT);
        let reference = finite_positive(reference_km_per_point, DEFAULT_KM_PER_POINT);
        let half_octaves = (scale / reference).log2() * 2.0;
        Self(half_octaves.floor().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
    }

    pub fn center_scale(self, reference_km_per_point: f32) -> f32 {
        let reference = finite_positive(reference_km_per_point, DEFAULT_KM_PER_POINT);
        reference * 2.0_f32.powf(f32::from(self.0) * 0.5)
    }
}

/// Stateful LOD selector with hysteresis so small wheel/trackpad deltas do not
/// repeatedly rebuild scene geometry around a bucket boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LodSelector {
    current: LodBucket,
    reference_km_per_point: f32,
    hysteresis_fraction: f32,
}

impl LodSelector {
    pub fn new(km_per_point: f32, reference_km_per_point: f32) -> Self {
        let reference = finite_positive(reference_km_per_point, DEFAULT_KM_PER_POINT);
        Self {
            current: LodBucket::ideal(km_per_point, reference),
            reference_km_per_point: reference,
            hysteresis_fraction: 0.12,
        }
    }

    pub const fn current(self) -> LodBucket {
        self.current
    }

    pub fn update(&mut self, km_per_point: f32) -> LodBucket {
        let scale = finite_positive(km_per_point, DEFAULT_KM_PER_POINT);
        let hysteresis = self.hysteresis_fraction.clamp(0.0, 0.45);
        loop {
            let center = self.current.center_scale(self.reference_km_per_point);
            let upper = center * 2.0_f32.sqrt() * (1.0 + hysteresis);
            let lower = center / 2.0_f32.sqrt() * (1.0 - hysteresis);
            if scale > upper && self.current.0 < i16::MAX {
                self.current.0 += 1;
            } else if scale < lower && self.current.0 > i16::MIN {
                self.current.0 -= 1;
            } else {
                break;
            }
        }
        self.current
    }
}

/// Retained geometry identity. Camera translation and exact camera scale do
/// not appear here by design.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GeometryCacheKey {
    pub dataset: Generation,
    pub projection: Generation,
    pub style: Generation,
    pub lod: LodBucket,
}

fn finite_positive(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

fn finite_f64(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
}

fn normalize_angle(angle: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    (angle + std::f32::consts::PI).rem_euclid(tau) - std::f32::consts::PI
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: ViewportMetrics = ViewportMetrics {
        width_points: 1000.0,
        height_points: 800.0,
        pixels_per_point: 2.0,
    };

    fn close(left: f64, right: f64) {
        assert!((left - right).abs() < 1.0e-5, "{left} != {right}");
    }

    #[test]
    fn world_screen_round_trip_survives_rotation() {
        let camera = Camera2D {
            center_east_km: 12.0,
            center_north_km: -8.0,
            km_per_point: 0.2,
            rotation_rad: 0.73,
        };
        let world = WorldPoint::new(45.0, 19.0);
        let restored = camera.screen_to_world(camera.world_to_screen(world, VIEW), VIEW);
        close(restored.east_km, world.east_km);
        close(restored.north_km, world.north_km);
    }

    #[test]
    fn zoom_keeps_anchor_world_coordinate_fixed() {
        let mut camera = Camera2D::default();
        let anchor = ScreenPoint::new(810.0, 190.0);
        let before = camera.screen_to_world(anchor, VIEW);
        camera.zoom_about(2.5, anchor, VIEW);
        let after = camera.screen_to_world(anchor, VIEW);
        close(before.east_km, after.east_km);
        close(before.north_km, after.north_km);
    }

    #[test]
    fn pan_moves_world_content_with_pointer() {
        let mut camera = Camera2D::default();
        let world = WorldPoint::new(30.0, 20.0);
        let before = camera.world_to_screen(world, VIEW);
        camera.pan_by_screen_delta(50.0, -25.0);
        let after = camera.world_to_screen(world, VIEW);
        assert!((after.x - before.x - 50.0).abs() < 1.0e-4);
        assert!((after.y - before.y + 25.0).abs() < 1.0e-4);
    }

    #[test]
    fn raster_view_places_radar_from_camera_transform() {
        let camera = Camera2D {
            center_east_km: 35.0,
            center_north_km: -14.0,
            ..Camera2D::default()
        };
        let view = camera.radar_raster_view(VIEW);
        assert_eq!(view.width_px, 2000);
        assert_eq!(view.height_px, 1600);
        let radar_points = camera.world_to_screen(WorldPoint::ORIGIN, VIEW);
        assert!((view.radar_x_px - radar_points.x * 2.0).abs() < 1.0e-4);
        assert!((view.radar_y_px - radar_points.y * 2.0).abs() < 1.0e-4);
    }

    #[test]
    fn geometry_identity_is_camera_independent() {
        let key = GeometryCacheKey {
            dataset: Generation::new(4),
            projection: Generation::new(2),
            style: Generation::new(7),
            lod: LodBucket(3),
        };
        let mut camera = Camera2D::default();
        camera.pan_by_screen_delta(900.0, -400.0);
        camera.zoom_about(1.4, VIEW.center(), VIEW);
        assert_eq!(
            key,
            GeometryCacheKey {
                dataset: Generation::new(4),
                projection: Generation::new(2),
                style: Generation::new(7),
                lod: LodBucket(3),
            }
        );
    }

    #[test]
    fn lod_hysteresis_holds_near_boundary() {
        let mut selector = LodSelector::new(1.0, 1.0);
        let original = selector.current();
        assert_eq!(selector.update(1.05), original);
        assert_eq!(selector.update(0.96), original);
    }
}
