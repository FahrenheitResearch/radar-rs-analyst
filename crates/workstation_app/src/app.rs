use std::array;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use analyst_runtime::{
    FrameOrigin, FrameStage, GenerationClock, PaneId, PaneLayout, PlaybackState, RenderStamp,
    TiltSelection, ViewportMetrics, VolumeFrame, VolumeHistory, WorkspaceState,
};
use chrono::SecondsFormat;
use color_tables::ColorTableSet;
use eframe::egui;
use radar_core::RadarVolume;

use crate::live_service::{LiveService, LiveUpdate, default_live_cache_dir};
use crate::load_service::{LoadRequest, LoadService, LoadUpdate, LoadedVolume};
use crate::pane_canvas::{PaneTexture, draw_pane, pane_rects};
use crate::product::DisplayProduct;
use crate::render_service::{RenderRequest, RenderService, RenderUpdate, RenderedPane};

enum LiveAction {
    Start(String),
    Stop,
}

const MAX_LOAD_RESULTS_PER_FRAME: usize = 4;
const MAX_RENDER_RESULTS_PER_FRAME: usize = 4;
const TIMELINE_HEIGHT: f32 = 34.0;
const PLAYBACK_FRAME_TIME: Duration = Duration::from_millis(700);

#[derive(Default)]
struct PaneRuntime {
    texture: Option<InstalledTexture>,
    pending_stamp: Option<RenderStamp>,
    viewport: Option<ViewportMetrics>,
    status: String,
}

struct InstalledTexture {
    handle: egui::TextureHandle,
    stamp: RenderStamp,
    camera: analyst_runtime::Camera2D,
    viewport: ViewportMetrics,
    width: u32,
    height: u32,
}

pub struct WorkstationApp {
    workspace: WorkspaceState,
    history: VolumeHistory,
    load_service: LoadService,
    render_service: RenderService,
    session_clock: GenerationClock,
    frame_clock: GenerationClock,
    pane_clocks: [GenerationClock; analyst_runtime::MAX_PANES],
    view_clocks: [GenerationClock; analyst_runtime::MAX_PANES],
    palette_clock: GenerationClock,
    panes: [PaneRuntime; analyst_runtime::MAX_PANES],
    color_tables: Arc<ColorTableSet>,
    source_path_text: String,
    status: String,
    load_ms: Option<f32>,
    last_playback_step: Instant,
    live_service: LiveService,
    live_cache_dir: PathBuf,
    site_text: String,
    live_site: Option<String>,
    live_status: String,
}

impl WorkstationApp {
    pub fn new(
        creation_context: &eframe::CreationContext<'_>,
        input_path: Option<PathBuf>,
        live_site: Option<String>,
    ) -> Self {
        let context = creation_context.egui_ctx.clone();
        let source_path_text = input_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let mut app = Self {
            workspace: WorkspaceState::default(),
            history: VolumeHistory::default(),
            load_service: LoadService::new(context.clone()),
            render_service: RenderService::new(context.clone()),
            live_service: LiveService::new(context),
            session_clock: GenerationClock::default(),
            frame_clock: GenerationClock::default(),
            pane_clocks: [GenerationClock::default(); analyst_runtime::MAX_PANES],
            view_clocks: [GenerationClock::default(); analyst_runtime::MAX_PANES],
            palette_clock: GenerationClock::default(),
            panes: array::from_fn(|_| PaneRuntime::default()),
            color_tables: Arc::new(ColorTableSet::default()),
            source_path_text,
            status: "Drop a Level II file here or enter a path above".to_owned(),
            load_ms: None,
            last_playback_step: Instant::now(),
            live_cache_dir: default_live_cache_dir(),
            site_text: String::new(),
            live_site: None,
            live_status: String::new(),
        };
        if let Some(path) = input_path {
            app.begin_load(path);
        }
        if let Some(site) = live_site {
            app.site_text = site.trim().to_uppercase();
            app.start_live(site);
        }
        app
    }

    fn begin_load(&mut self, path: PathBuf) {
        if self.live_site.is_some() {
            self.live_service.stop();
            self.live_site = None;
            self.live_status.clear();
        }
        let generation = self.session_clock.bump();
        self.frame_clock.bump();
        self.history.clear();
        self.source_path_text = path.display().to_string();
        self.status = format!("Loading {}", path.display());
        self.load_ms = None;
        self.clear_all_panes();
        let source_label = path.display().to_string();
        if let Err(request) = self.load_service.request(LoadRequest {
            generation,
            path,
            origin: FrameOrigin::Local,
            final_stage: FrameStage::Complete,
            source_label,
        }) {
            self.status = format!("load worker is closed: {}", request.path.display());
        }
    }

    /// Start a live session for `site`. The generation bump invalidates every
    /// in-flight local or previous-site result before the new session installs.
    fn start_live(&mut self, site: String) {
        let generation = self.session_clock.bump();
        self.frame_clock.bump();
        self.history.clear();
        self.load_ms = None;
        self.clear_all_panes();
        let label = site.trim().to_uppercase();
        match self
            .live_service
            .start(generation, site, self.live_cache_dir.clone())
        {
            Ok(()) => {
                let site = label;
                self.status = format!("Starting live {site}");
                self.live_status = "connecting".to_owned();
                self.live_site = Some(site);
            }
            Err(message) => {
                self.status = message;
                self.live_status.clear();
                self.live_site = None;
            }
        }
    }

    /// Stop the live session. The generation bump means a download that is
    /// already in flight cannot install after the user has stopped.
    fn stop_live(&mut self) {
        self.live_service.stop();
        self.session_clock.bump();
        self.live_site = None;
        self.live_status.clear();
        self.status = "Live session stopped".to_owned();
    }

    fn poll_live_results(&mut self) {
        for _ in 0..MAX_LOAD_RESULTS_PER_FRAME {
            let Some(update) = self.live_service.try_recv() else {
                break;
            };
            match update {
                LiveUpdate::Started { generation, site } => {
                    if generation == self.session_clock.current() {
                        self.status = format!("Live {site}");
                        self.live_status = "waiting for volume".to_owned();
                    }
                }
                LiveUpdate::VolumeReady {
                    generation,
                    site,
                    path,
                    stage,
                    volume_time,
                    chunk_count,
                    total_size,
                    cache_hit,
                } => {
                    if generation != self.session_clock.current() {
                        continue;
                    }
                    self.live_status = format!(
                        "{} chunk(s) · {:.1} MiB · {}",
                        chunk_count,
                        total_size as f64 / (1_024.0 * 1_024.0),
                        if cache_hit { "cached" } else { "downloaded" }
                    );
                    let source_label = format!(
                        "{site} {}",
                        volume_time.to_rfc3339_opts(SecondsFormat::Secs, true)
                    );
                    if let Err(request) = self.load_service.request(LoadRequest {
                        generation,
                        path,
                        origin: FrameOrigin::Live,
                        final_stage: stage,
                        source_label,
                    }) {
                        self.status = format!("load worker is closed: {}", request.path.display());
                    }
                }
                LiveUpdate::Failed {
                    generation,
                    site,
                    message,
                } => {
                    if generation == self.session_clock.current() {
                        self.status = format!("{site}: {message}");
                        self.live_status = "error".to_owned();
                    }
                }
                LiveUpdate::Stopped => {
                    self.live_status.clear();
                }
            }
        }
    }

    fn poll_load_results(&mut self) {
        for _ in 0..MAX_LOAD_RESULTS_PER_FRAME {
            let Some(update) = self.load_service.try_recv() else {
                break;
            };
            match update {
                LoadUpdate::Started {
                    generation,
                    source_label,
                } => {
                    if generation == self.session_clock.current() {
                        self.status = format!("Decoding {source_label}");
                    }
                }
                LoadUpdate::Volume(loaded) => self.install_loaded_volume(loaded),
                LoadUpdate::Failed {
                    generation,
                    source_label,
                    message,
                } => {
                    if generation == self.session_clock.current() {
                        self.status = format!("{source_label}: {message}");
                        self.clear_all_panes();
                    }
                }
            }
        }
    }

    fn install_loaded_volume(&mut self, loaded: LoadedVolume) {
        if loaded.generation != self.session_clock.current() {
            return;
        }
        let before = self.current_frame_signature();
        let stage = loaded.stage;
        let report = self.history.install(VolumeFrame::new(
            loaded.volume,
            loaded.origin,
            stage,
            loaded.source_label,
        ));
        self.load_ms = Some(loaded.elapsed_ms);
        let after = self.current_frame_signature();
        if before != after {
            self.frame_clock.bump();
            self.clear_all_panes();
        }
        self.status = match stage {
            FrameStage::Preview => format!(
                "Preview ready in {:.1} ms · {} frame(s)",
                loaded.elapsed_ms,
                self.history.len()
            ),
            FrameStage::Partial => format!(
                "Partial volume ready in {:.1} ms · {} frame(s)",
                loaded.elapsed_ms,
                self.history.len()
            ),
            FrameStage::Complete => format!(
                "Complete volume ready in {:.1} ms · {} frame(s) · {:?}",
                loaded.elapsed_ms,
                self.history.len(),
                report.disposition
            ),
        };
    }

    fn poll_render_results(&mut self, context: &egui::Context) {
        for _ in 0..MAX_RENDER_RESULTS_PER_FRAME {
            let Some(update) = self.render_service.try_recv() else {
                break;
            };
            match update {
                RenderUpdate::Completed(rendered) => self.install_render(context, rendered),
                RenderUpdate::Failed {
                    pane,
                    stamp,
                    message,
                } => {
                    if stamp == self.current_stamp(pane) {
                        let runtime = &mut self.panes[pane.index()];
                        runtime.pending_stamp = None;
                        runtime.status = message;
                    }
                }
            }
        }
    }

    fn install_render(&mut self, context: &egui::Context, rendered: RenderedPane) {
        if rendered.stamp != self.current_stamp(rendered.pane) {
            return;
        }
        let image = color_image_from_rgba(rendered.width, rendered.height, &rendered.rgba);
        let runtime = &mut self.panes[rendered.pane.index()];
        let can_update = runtime.texture.as_ref().is_some_and(|texture| {
            texture.width == rendered.width && texture.height == rendered.height
        });
        if can_update {
            if let Some(texture) = &mut runtime.texture {
                texture.handle.set(image, egui::TextureOptions::NEAREST);
                texture.stamp = rendered.stamp;
                texture.camera = rendered.camera;
                texture.viewport = rendered.viewport;
                texture.width = rendered.width;
                texture.height = rendered.height;
            }
        } else {
            let handle = context.load_texture(
                format!(
                    "radar-pane-{}-{}-{}x{}",
                    rendered.pane.get(),
                    rendered.stamp.view.get(),
                    rendered.width,
                    rendered.height
                ),
                image,
                egui::TextureOptions::NEAREST,
            );
            runtime.texture = Some(InstalledTexture {
                handle,
                stamp: rendered.stamp,
                camera: rendered.camera,
                viewport: rendered.viewport,
                width: rendered.width,
                height: rendered.height,
            });
        }
        runtime.pending_stamp = None;
        runtime.status = format!("{:.1} ms", rendered.elapsed_ms);
    }

    fn handle_dropped_files(&mut self, context: &egui::Context) {
        let dropped = context.input(|input| input.raw.dropped_files.clone());
        if let Some(path) = dropped.into_iter().find_map(|file| file.path) {
            self.begin_load(path);
        }
    }

    fn advance_playback(&mut self, context: &egui::Context) {
        if self.history.playback() != PlaybackState::Playing || self.history.len() < 2 {
            return;
        }
        if !self.visible_panes_ready() {
            context.request_repaint_after(Duration::from_millis(16));
            return;
        }
        let elapsed = self.last_playback_step.elapsed();
        if elapsed < PLAYBACK_FRAME_TIME {
            context.request_repaint_after(PLAYBACK_FRAME_TIME - elapsed);
            return;
        }
        let before = self.current_frame_signature();
        self.history.advance_wrapping();
        self.last_playback_step = Instant::now();
        if self.current_frame_signature() != before {
            self.frame_clock.bump();
            self.clear_all_panes();
        }
        context.request_repaint();
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let active = self.workspace.active_pane;
        let current_product = DisplayProduct::from_product_id(&self.workspace.active().product);
        let mut requested_load = None;
        let mut live_action = None;
        let mut selected_layout = self.workspace.layout;
        let mut selected_product = current_product;
        let mut tilt_delta = 0_isize;
        let visible = self.workspace.visible_panes();
        let cameras_linked = visible
            .iter()
            .all(|pane| self.workspace.pane(*pane).links.camera == Some(0));
        let mut toggle_camera_links = false;

        ui.horizontal_wrapped(|ui| {
            ui.strong("Radar Workstation");
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.source_path_text)
                    .desired_width(260.0)
                    .hint_text("Level II file path"),
            );
            if ui.button("Load").clicked() && !self.source_path_text.trim().is_empty() {
                requested_load = Some(PathBuf::from(self.source_path_text.trim()));
            }

            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.site_text)
                    .desired_width(56.0)
                    .char_limit(4)
                    .hint_text("KRTX"),
            );
            if self.live_site.is_some() {
                if ui.button("Stop live").clicked() {
                    live_action = Some(LiveAction::Stop);
                }
            } else if ui.button("Start live").clicked() && !self.site_text.trim().is_empty() {
                live_action = Some(LiveAction::Start(self.site_text.trim().to_owned()));
            }
            if !self.live_status.is_empty() {
                ui.label(&self.live_status);
            }

            ui.separator();
            egui::ComboBox::from_id_salt("workstation-layout")
                .selected_text(layout_label(selected_layout))
                .width(112.0)
                .show_ui(ui, |ui| {
                    for layout in [
                        PaneLayout::One,
                        PaneLayout::TwoVertical,
                        PaneLayout::TwoHorizontal,
                        PaneLayout::Four,
                    ] {
                        ui.selectable_value(&mut selected_layout, layout, layout_label(layout));
                    }
                });

            egui::ComboBox::from_id_salt("workstation-product")
                .selected_text(current_product.label())
                .width(184.0)
                .show_ui(ui, |ui| {
                    for product in DisplayProduct::ALL {
                        ui.selectable_value(&mut selected_product, product, product.label());
                    }
                });

            if ui.button("− Tilt").clicked() {
                tilt_delta = -1;
            }
            ui.label(self.active_tilt_label());
            if ui.button("+ Tilt").clicked() {
                tilt_delta = 1;
            }
            if ui
                .selectable_label(cameras_linked, "Link cameras")
                .clicked()
            {
                toggle_camera_links = true;
            }
            ui.label(format!("Pane {}", active.get() + 1));
        });

        if let Some(path) = requested_load {
            self.begin_load(path);
        }
        match live_action {
            Some(LiveAction::Start(site)) => self.start_live(site),
            Some(LiveAction::Stop) => self.stop_live(),
            None => {}
        }
        if selected_layout != self.workspace.layout {
            self.workspace.set_layout(selected_layout);
        }
        if selected_product != current_product {
            let changed = self
                .workspace
                .apply_product_from(active, selected_product.product_id());
            self.invalidate_semantic_panes(&changed);
        }
        if tilt_delta != 0 {
            self.change_active_tilt(tilt_delta);
        }
        if toggle_camera_links {
            let new_group = (!cameras_linked).then_some(0);
            for pane in self.workspace.visible_panes() {
                self.workspace.pane_mut(*pane).links.camera = new_group;
            }
        }
    }

    fn canvas(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let volume = self
            .history
            .current()
            .map(|frame| Arc::clone(&frame.volume));
        for (pane, pane_rect) in pane_rects(rect, self.workspace.layout) {
            let camera = self.workspace.pane(pane).camera;
            let product = DisplayProduct::from_product_id(&self.workspace.pane(pane).product);
            let cut_index = volume
                .as_deref()
                .and_then(|volume| self.resolve_cut_index(pane, volume));
            let title = pane_title(volume.as_deref(), pane, product, cut_index);
            let status = self.panes[pane.index()].status.clone();
            let interaction = {
                let texture =
                    self.panes[pane.index()]
                        .texture
                        .as_ref()
                        .map(|texture| PaneTexture {
                            handle: &texture.handle,
                            camera: texture.camera,
                            viewport: texture.viewport,
                        });
                draw_pane(
                    ui,
                    pane,
                    pane_rect,
                    pane == self.workspace.active_pane,
                    camera,
                    texture,
                    &title,
                    &status,
                )
            };

            if interaction.clicked {
                self.workspace.set_active(pane);
            }
            self.update_viewport(pane, interaction.viewport);
            if interaction.camera_changed {
                let changed = self.workspace.apply_camera_from(pane, interaction.camera);
                self.invalidate_view_panes(&changed);
            }
            if let Some(volume) = &volume {
                self.ensure_render_requested(pane, Arc::clone(volume), interaction.viewport);
            }
        }
    }

    fn timeline(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        let frame_count = self.history.len();
        let mut selected = self.history.selected_index().unwrap_or(0);
        let mut choose_frame = None;
        let mut go_live = false;
        let mut toggle_playback = false;

        ui.horizontal(|ui| {
            if ui
                .add_enabled(frame_count > 1, egui::Button::new("◀"))
                .clicked()
            {
                choose_frame = selected.checked_sub(1);
            }
            if ui
                .add_enabled(
                    frame_count > 1,
                    egui::Button::new(if self.history.playback() == PlaybackState::Playing {
                        "Pause"
                    } else {
                        "Play"
                    }),
                )
                .clicked()
            {
                toggle_playback = true;
            }
            if ui
                .add_enabled(
                    frame_count > 1 && selected + 1 < frame_count,
                    egui::Button::new("▶"),
                )
                .clicked()
            {
                choose_frame = Some(selected + 1);
            }
            if ui
                .add_enabled(frame_count > 0, egui::Button::new("Go live"))
                .clicked()
            {
                go_live = true;
            }

            if frame_count > 1 {
                let response = ui.add_sized(
                    [220.0, ui.spacing().interact_size.y],
                    egui::Slider::new(&mut selected, 0..=frame_count - 1).show_value(false),
                );
                if response.changed() {
                    choose_frame = Some(selected);
                }
            }

            ui.separator();
            ui.label(self.timeline_status());
            if let Some(load_ms) = self.load_ms {
                ui.label(format!("decode {load_ms:.1} ms"));
            }
            ui.label(format!(
                "history {:.1} MiB",
                self.history.estimated_bytes() as f64 / (1024.0 * 1024.0)
            ));
            let queued = self.render_service.queued_panes();
            if queued > 0 {
                ui.label(format!("{queued} pane(s) queued"));
            }
        });

        if toggle_playback {
            let next = if self.history.playback() == PlaybackState::Playing {
                PlaybackState::Paused
            } else {
                self.last_playback_step = Instant::now();
                PlaybackState::Playing
            };
            self.history.set_playback(next);
            context.request_repaint();
        }
        if go_live {
            let before = self.current_frame_signature();
            self.history.go_live();
            self.history.set_playback(PlaybackState::Paused);
            self.commit_history_selection(before);
        } else if let Some(index) = choose_frame {
            let before = self.current_frame_signature();
            self.history.select(index);
            self.history.set_playback(PlaybackState::Paused);
            self.commit_history_selection(before);
        }
    }

    fn ensure_render_requested(
        &mut self,
        pane: PaneId,
        volume: Arc<RadarVolume>,
        viewport: ViewportMetrics,
    ) {
        let product = DisplayProduct::from_product_id(&self.workspace.pane(pane).product);
        let Some(cut_index) = self.resolve_cut_index(pane, &volume) else {
            let runtime = &mut self.panes[pane.index()];
            runtime.pending_stamp = None;
            runtime.status = format!("{} unavailable", product.id());
            return;
        };
        let stamp = self.current_stamp(pane);
        let runtime = &self.panes[pane.index()];
        let already_current = runtime
            .texture
            .as_ref()
            .is_some_and(|texture| texture.stamp == stamp);
        if already_current || runtime.pending_stamp == Some(stamp) {
            return;
        }

        let request = RenderRequest {
            pane,
            stamp,
            volume,
            cut_index,
            product,
            camera: self.workspace.pane(pane).camera,
            viewport,
            storm_motion: self.workspace.pane(pane).storm_motion,
            color_tables: Arc::clone(&self.color_tables),
        };
        match self.render_service.request(request) {
            Ok(()) => {
                let runtime = &mut self.panes[pane.index()];
                runtime.pending_stamp = Some(stamp);
                runtime.status = "rendering".to_owned();
            }
            Err(_) => {
                self.panes[pane.index()].status = "render worker closed".to_owned();
            }
        }
    }

    fn update_viewport(&mut self, pane: PaneId, viewport: ViewportMetrics) {
        let changed = self.panes[pane.index()]
            .viewport
            .is_none_or(|previous| viewport_changed(previous, viewport));
        if changed {
            self.panes[pane.index()].viewport = Some(viewport);
            self.panes[pane.index()].pending_stamp = None;
            self.view_clocks[pane.index()].bump();
        }
    }

    fn invalidate_view_panes(&mut self, panes: &[PaneId]) {
        for pane in panes {
            self.view_clocks[pane.index()].bump();
            self.panes[pane.index()].pending_stamp = None;
        }
    }

    fn invalidate_semantic_panes(&mut self, panes: &[PaneId]) {
        for pane in panes {
            self.pane_clocks[pane.index()].bump();
            let runtime = &mut self.panes[pane.index()];
            runtime.texture = None;
            runtime.pending_stamp = None;
            runtime.status.clear();
        }
    }

    fn clear_all_panes(&mut self) {
        for runtime in &mut self.panes {
            runtime.texture = None;
            runtime.pending_stamp = None;
            runtime.status.clear();
        }
    }

    fn current_stamp(&self, pane: PaneId) -> RenderStamp {
        RenderStamp {
            pane_id: pane.get(),
            session: self.session_clock.current(),
            frame: self.frame_clock.current(),
            pane: self.pane_clocks[pane.index()].current(),
            view: self.view_clocks[pane.index()].current(),
            palette: self.palette_clock.current(),
        }
    }

    fn current_frame_signature(&self) -> Option<(analyst_runtime::FrameIdentity, FrameStage)> {
        self.history
            .current()
            .map(|frame| (frame.identity.clone(), frame.stage))
    }

    fn commit_history_selection(
        &mut self,
        before: Option<(analyst_runtime::FrameIdentity, FrameStage)>,
    ) {
        if self.current_frame_signature() != before {
            self.frame_clock.bump();
            self.clear_all_panes();
        }
    }

    fn resolve_cut_index(&self, pane: PaneId, volume: &RadarVolume) -> Option<usize> {
        let intent = self.workspace.pane(pane);
        let product = DisplayProduct::from_product_id(&intent.product);
        match intent.tilt {
            TiltSelection::LowestAvailable => product.first_available_cut(volume),
            TiltSelection::CutIndex(index) => {
                let index = usize::from(index);
                product
                    .is_available_in_cut(volume, index)
                    .then_some(index)
                    .or_else(|| product.first_available_cut(volume))
            }
            TiltSelection::NearestElevationTenths(target) => volume
                .cuts
                .iter()
                .enumerate()
                .filter(|(index, _)| product.is_available_in_cut(volume, *index))
                .min_by_key(|(_, cut)| {
                    let elevation = (cut.elevation_deg * 10.0).round() as i16;
                    (elevation - target).abs()
                })
                .map(|(index, _)| index),
        }
    }

    fn change_active_tilt(&mut self, delta: isize) {
        let Some(volume) = self
            .history
            .current()
            .map(|frame| Arc::clone(&frame.volume))
        else {
            return;
        };
        let active = self.workspace.active_pane;
        let product = DisplayProduct::from_product_id(&self.workspace.pane(active).product);
        let Some(current) = self.resolve_cut_index(active, &volume) else {
            return;
        };
        let Some(next) = product.next_available_cut(&volume, current, delta) else {
            return;
        };
        let changed = self
            .workspace
            .apply_tilt_from(active, TiltSelection::CutIndex(next as u16));
        self.invalidate_semantic_panes(&changed);
    }

    fn active_tilt_label(&self) -> String {
        let Some(frame) = self.history.current() else {
            return "No tilt".to_owned();
        };
        let Some(index) = self.resolve_cut_index(self.workspace.active_pane, &frame.volume) else {
            return "Unavailable".to_owned();
        };
        frame
            .volume
            .cuts
            .get(index)
            .map(|cut| format!("{:.1}°", cut.elevation_deg))
            .unwrap_or_else(|| "Unavailable".to_owned())
    }

    fn timeline_status(&self) -> String {
        let Some(frame) = self.history.current() else {
            return self.status.clone();
        };
        let index = self.history.selected_index().unwrap_or(0) + 1;
        format!(
            "{} · {}/{} · {:?} · {}",
            frame.identity.site_id,
            index,
            self.history.len(),
            frame.stage,
            frame
                .identity
                .volume_time
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        )
    }

    fn visible_panes_ready(&self) -> bool {
        self.workspace.visible_panes().iter().all(|pane| {
            let runtime = &self.panes[pane.index()];
            runtime.pending_stamp.is_none()
                && runtime
                    .texture
                    .as_ref()
                    .is_some_and(|texture| texture.stamp == self.current_stamp(*pane))
        })
    }
}

impl eframe::App for WorkstationApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.handle_dropped_files(&context);
        self.poll_live_results();
        self.poll_load_results();
        self.poll_render_results(&context);
        self.advance_playback(&context);

        ui.visuals_mut().panel_fill = egui::Color32::from_rgb(10, 13, 17);
        self.toolbar(ui);
        ui.separator();

        let available = ui.available_size();
        let canvas_height = (available.y - TIMELINE_HEIGHT).max(120.0);
        let (canvas_rect, _) = ui.allocate_exact_size(
            egui::vec2(available.x.max(1.0), canvas_height),
            egui::Sense::hover(),
        );
        self.canvas(ui, canvas_rect);

        ui.separator();
        self.timeline(ui, &context);

        if self.panes.iter().any(|pane| pane.pending_stamp.is_some()) {
            context.request_repaint_after(Duration::from_millis(16));
        }
    }
}

fn layout_label(layout: PaneLayout) -> &'static str {
    match layout {
        PaneLayout::One => "1 pane",
        PaneLayout::TwoHorizontal => "2 horizontal",
        PaneLayout::TwoVertical => "2 vertical",
        PaneLayout::Four => "4 panes",
    }
}

fn pane_title(
    volume: Option<&RadarVolume>,
    pane: PaneId,
    product: DisplayProduct,
    cut_index: Option<usize>,
) -> String {
    let elevation = volume
        .zip(cut_index)
        .and_then(|(volume, index)| volume.cuts.get(index))
        .map(|cut| format!(" · {:.1}°", cut.elevation_deg))
        .unwrap_or_default();
    format!("{} · {}{}", pane.get() + 1, product.id(), elevation)
}

fn viewport_changed(previous: ViewportMetrics, current: ViewportMetrics) -> bool {
    (previous.width_points - current.width_points).abs() >= 0.5
        || (previous.height_points - current.height_points).abs() >= 0.5
        || previous.pixels_per_point.to_bits() != current.pixels_per_point.to_bits()
}

fn color_image_from_rgba(width: u32, height: u32, rgba: &[u8]) -> egui::ColorImage {
    let expected = width as usize * height as usize * 4;
    assert_eq!(rgba.len(), expected, "invalid renderer RGBA buffer length");
    let pixels = rgba
        .chunks_exact(4)
        .map(|pixel| egui::Color32::from_rgba_unmultiplied(pixel[0], pixel[1], pixel[2], pixel[3]))
        .collect();
    egui::ColorImage::new([width as usize, height as usize], pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_change_ignores_subpixel_layout_noise() {
        let original = ViewportMetrics {
            width_points: 800.0,
            height_points: 600.0,
            pixels_per_point: 1.5,
        };
        assert!(!viewport_changed(
            original,
            ViewportMetrics {
                width_points: 800.2,
                height_points: 599.8,
                pixels_per_point: 1.5,
            }
        ));
        assert!(viewport_changed(
            original,
            ViewportMetrics {
                width_points: 801.0,
                ..original
            }
        ));
    }
}
