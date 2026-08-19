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
use map_scene::MapSceneController;
use radar_core::RadarVolume;

use data_source::warnings::{WarningRecord, WarningsSource, WarningsState};

use crate::hazards::{PlacedHazard, place_hazards};
use crate::live_service::{LiveService, LiveUpdate, default_live_cache_dir};
use crate::load_service::{LoadRequest, LoadService, LoadUpdate, LoadedVolume};
use crate::pane_canvas::{PaneMap, PaneTexture, PlacedSite, draw_pane, pane_rects};
use crate::product::DisplayProduct;

use crate::app_support::{color_image_from_rgba, layout_label, pane_title, viewport_changed};
use crate::product_availability::ProductAvailabilityIndex;
use crate::product_picker::{ProductPickerInput, ProductPickerState, draw_product_picker};
use crate::render_service::{
    RenderRequest, RenderService, RenderUpdate, RenderedPane, SweepBlendRequest,
};
use crate::sites_service::{LocatedSite, SitesService};
use crate::sweep::{SweepAnimator, SweepState, catch_up_factor};
use crate::warnings_service::WarningsService;

/// How often placed hazards are rebuilt so expiries take effect.
///
/// Placement filters by "now", so it goes stale on its own even when nothing
/// arrives. A warning ends on a whole minute, so checking twice a minute is
/// enough to never leave an expired polygon on screen for long.
const HAZARD_REPLACEMENT_INTERVAL: Duration = Duration::from_secs(30);

enum LiveAction {
    Start(String),
    Stop,
}

/// Opening overview scale: wide enough to show the country before a radar
/// volume says where to look.
const PLACEHOLDER_KM_PER_POINT: f32 = 4.0;

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
    /// Where the pointer was over this pane last frame, in radar-local
    /// kilometres, and the readout built from it.
    hovered_world_km: Option<(f64, f64)>,
    probe_text: Option<String>,
    /// Turns bursty radial arrivals into a clockwise wipe. One per pane,
    /// because two panes can be following different tilts of the same volume.
    sweep: SweepAnimator,
    /// The reveal handed to the last render request.
    sweep_state: Option<SweepState>,
    /// What that reveal is a reveal OF. The animator recognises a sweep by its
    /// elevation and start azimuth, which a product change leaves untouched
    /// while replacing every pixel, so the pane tracks that separately.
    sweep_key: Option<SweepKey>,
    /// When the reveal was last stepped, for the wall-clock ease.
    sweep_stepped_at: Option<Instant>,
}

impl PaneRuntime {
    fn reset_sweep(&mut self) {
        self.sweep.reset();
        self.sweep_state = None;
        self.sweep_key = None;
        self.sweep_stepped_at = None;
    }
}

/// What a pane's sweep reveal refers to.
///
/// Compared for equality to decide whether the eased position still means
/// anything. The cut index is in here because switching tilts inside one volume
/// changes everything about the sweep while leaving the frame identity alone.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SweepKey {
    identity: analyst_runtime::FrameIdentity,
    product: &'static str,
    cut_index: usize,
}

struct InstalledTexture {
    handle: egui::TextureHandle,
    stamp: RenderStamp,
    camera: analyst_runtime::Camera2D,
    viewport: ViewportMetrics,
    width: u32,
    height: u32,
}

/// What a measurement of the current volume was taken from.
///
/// Compared for equality to decide whether the measurement is still good.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CapabilitiesKey {
    identity: analyst_runtime::FrameIdentity,
    stage: FrameStage,
    cuts: usize,
    radials: usize,
}

pub struct WorkstationApp {
    workspace: WorkspaceState,
    history: VolumeHistory,
    /// What the current volume can do, measured once per frame off the paint
    /// path. Cut selection needs median elevations and per-sweep scan times,
    /// and walking every radial while painting is not affordable.
    /// Whether a pane click takes a Vrot endpoint instead of selecting a pane.
    /// Thermal levels the hail products are computed against. Starts as the
    /// documented fallback, which badges itself ASSUMED so nobody mistakes it
    /// for a sounding.
    hail_environment: product_engine::HailEnvironment,
    /// The 3D volume explorer. Its own window, so opening it does not disturb
    /// the pane layout an analyst has set up.
    vol3d: crate::vol3d::Vol3d,
    vrot_active: bool,
    vrot_state: crate::vrot::VrotState,
    vrot_pane: Option<PaneId>,
    capabilities: Option<Arc<product_engine::VolumeCapabilities>>,
    capabilities_for: Option<CapabilitiesKey>,
    /// How hard the raster worker is asked to work. Not part of `RenderStamp`:
    /// a change bumps every pane's view clock instead, which is the existing
    /// way of saying "same data, different picture".
    quality: render2d::DisplayQuality,
    /// Which products the current volume can actually show, rebuilt with the
    /// capabilities.
    product_availability: ProductAvailabilityIndex,
    product_picker: ProductPickerState,
    product_picker_open: bool,
    load_service: LoadService,
    render_service: RenderService,
    session_clock: GenerationClock,
    frame_clock: GenerationClock,
    pane_clocks: [GenerationClock; analyst_runtime::MAX_PANES],
    view_clocks: [GenerationClock; analyst_runtime::MAX_PANES],
    sweep_clocks: [GenerationClock; analyst_runtime::MAX_PANES],
    palette_clock: GenerationClock,
    panes: [PaneRuntime; analyst_runtime::MAX_PANES],
    color_tables: Arc<ColorTableSet>,
    source_path_text: String,
    status: String,
    load_ms: Option<f32>,
    last_playback_step: Instant,
    map_scene: MapSceneController,
    sites_service: SitesService,
    sites: Vec<LocatedSite>,
    placed_sites: Arc<[PlacedSite]>,
    placed_sites_projection: Option<map_scene::ProjectionId>,
    live_service: LiveService,
    live_cache_dir: PathBuf,
    site_text: String,
    live_site: Option<String>,
    live_status: String,
    warnings_service: WarningsService,
    warnings: Vec<WarningRecord>,
    warnings_state: WarningsState,
    show_warnings: bool,
    placed_hazards: Arc<[PlacedHazard]>,
    placed_hazards_projection: Option<map_scene::ProjectionId>,
    placed_hazards_at: Option<Instant>,
}

impl WorkstationApp {
    pub fn new(
        creation_context: &eframe::CreationContext<'_>,
        input_path: Option<PathBuf>,
        live_site: Option<String>,
        warnings_source: WarningsSource,
    ) -> Self {
        let context = creation_context.egui_ctx.clone();
        let source_path_text = input_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let mut app = Self {
            workspace: WorkspaceState::default(),
            history: VolumeHistory::default(),
            hail_environment: product_engine::HailEnvironment::climatological_fallback(),
            vol3d: crate::vol3d::Vol3d::default(),
            vrot_active: false,
            vrot_state: crate::vrot::VrotState::Idle,
            vrot_pane: None,
            capabilities: None,
            capabilities_for: None,
            quality: render2d::DisplayQuality::default(),
            product_availability: ProductAvailabilityIndex::unrestricted(),
            product_picker: ProductPickerState::default(),
            product_picker_open: false,
            load_service: LoadService::new(context.clone()),
            render_service: RenderService::new(context.clone()),
            live_service: LiveService::new(context.clone()),
            map_scene: {
                let repaint_context = context.clone();
                MapSceneController::new(move || repaint_context.request_repaint())
            },
            session_clock: GenerationClock::default(),
            frame_clock: GenerationClock::default(),
            pane_clocks: [GenerationClock::default(); analyst_runtime::MAX_PANES],
            view_clocks: [GenerationClock::default(); analyst_runtime::MAX_PANES],
            sweep_clocks: [GenerationClock::default(); analyst_runtime::MAX_PANES],
            palette_clock: GenerationClock::default(),
            panes: array::from_fn(|_| PaneRuntime::default()),
            color_tables: Arc::new(ColorTableSet::default()),
            source_path_text,
            status: "Drop a Level II file here or enter a path above".to_owned(),
            load_ms: None,
            last_playback_step: Instant::now(),
            sites_service: SitesService::new(context.clone()),
            sites: Vec::new(),
            placed_sites: Vec::new().into(),
            placed_sites_projection: None,
            live_cache_dir: default_live_cache_dir(),
            site_text: String::new(),
            live_site: None,
            live_status: String::new(),
            warnings_service: WarningsService::new(context, warnings_source),
            warnings: Vec::new(),
            warnings_state: WarningsState::Unknown,
            show_warnings: true,
            placed_hazards: Vec::new().into(),
            placed_hazards_projection: None,
            placed_hazards_at: None,
        };
        if let Some(path) = input_path {
            app.begin_load(path);
        }
        if let Some(site) = live_site {
            app.site_text = site.trim().to_uppercase();
            app.start_live(site);
        }
        // Open on a map instead of an empty pane. The placeholder anchor, and
        // this overview scale, are replaced by the first real volume.
        if app.history.is_empty() {
            app.map_scene.set_default_anchor();
            let panes = app.workspace.centre_on_anchor(PLACEHOLDER_KM_PER_POINT);
            app.invalidate_view_panes(&panes);
        }
        app
    }

    /// Apply a camera stated at startup to every pane, so a particular pan or
    /// zoom can be reproduced without driving the window by hand.
    pub fn set_initial_camera(
        &mut self,
        zoom_km_per_point: Option<f32>,
        center_km: Option<(f64, f64)>,
    ) {
        if zoom_km_per_point.is_none() && center_km.is_none() {
            return;
        }
        let mut panes = Vec::with_capacity(analyst_runtime::MAX_PANES);
        for index in 0..analyst_runtime::MAX_PANES {
            let Some(pane) = PaneId::new(index as u8) else {
                continue;
            };
            let camera = &mut self.workspace.pane_mut(pane).camera;
            if let Some(km_per_point) = zoom_km_per_point {
                camera.km_per_point = km_per_point;
            }
            if let Some((east_km, north_km)) = center_km {
                camera.center_east_km = east_km;
                camera.center_north_km = north_km;
            }
            *camera = camera.sanitized();
            panes.push(pane);
        }
        self.invalidate_view_panes(&panes);
    }

    /// Open every pane on a product stated at startup.
    ///
    /// This exists for the same reason the camera options do. Windows refuses a
    /// foreground change from a background process, so synthetic clicks land in
    /// whatever window happens to be focused; a product cannot be selected by
    /// hand in a captured session. Without this flag the only product that
    /// could ever be photographed on real data is the default one.
    pub fn set_initial_product(&mut self, product: Option<DisplayProduct>) {
        let Some(product) = product else {
            return;
        };
        let id = product.product_id();
        let mut panes = Vec::with_capacity(analyst_runtime::MAX_PANES);
        for index in 0..analyst_runtime::MAX_PANES {
            let Some(pane) = PaneId::new(index as u8) else {
                continue;
            };
            self.workspace.pane_mut(pane).product = id.clone();
            panes.push(pane);
        }
        self.invalidate_semantic_panes(&panes);
    }

    /// Open the 3D explorer at startup, so a particular view can be captured
    /// without driving the window by hand.
    pub fn set_vol3d_open(&mut self, open: bool) {
        self.vol3d.open = open;
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

    fn poll_site_directory(&mut self) {
        while let Some(sites) = self.sites_service.try_recv() {
            self.sites = sites;
            // Force a reprojection against the current anchor.
            self.placed_sites_projection = None;
        }
    }

    /// Project the site directory into world kilometres.
    ///
    /// Done once per anchor change rather than per frame: the positions are
    /// fixed relative to the projection, so the paint pass only has to apply
    /// the camera transform.
    fn refresh_placed_sites(&mut self) {
        let Some(projection) = self.map_scene.projection() else {
            return;
        };
        if self.placed_sites_projection == Some(projection.id()) {
            return;
        }
        self.placed_sites = self
            .sites
            .iter()
            .filter_map(|site| {
                let world =
                    projection.try_lon_lat_to_world(site.longitude_deg, site.latitude_deg)?;
                Some(PlacedSite {
                    id: site.id.clone(),
                    world,
                })
            })
            .collect::<Vec<_>>()
            .into();
        self.placed_sites_projection = Some(projection.id());
    }

    /// Hover text for the warnings chip.
    ///
    /// The chip's own number is every alert in force, and most of those are
    /// county-coded products that carry no polygon at all -- 442 active against
    /// 148 with geometry, measured on 2026-08-17. Saying how many are actually
    /// drawn stops the chip reading as a claim about the picture.
    fn poll_warnings(&mut self) {
        while let Some(update) = self.warnings_service.try_recv() {
            self.warnings_state = update.state;
            // A failed poll leaves the previous records alone: blanking the map
            // on one bad round trip would be a worse lie than a stale polygon,
            // and the chip already says the feed is offline.
            if let Some(records) = update.records {
                self.warnings = records;
                self.placed_hazards_at = None;
            }
        }
    }

    /// Project the warnings in force into world kilometres.
    ///
    /// Rebuilt when the anchor changes, when new records arrive, and on a slow
    /// timer so an expiry takes effect without waiting for the next poll.
    fn refresh_placed_hazards(&mut self) {
        if !self.show_warnings {
            if !self.placed_hazards.is_empty() {
                self.placed_hazards = Vec::new().into();
                self.placed_hazards_at = None;
            }
            return;
        }
        let Some(projection) = self.map_scene.projection() else {
            return;
        };
        let stale = self
            .placed_hazards_at
            .is_none_or(|at| at.elapsed() >= HAZARD_REPLACEMENT_INTERVAL);
        if !stale && self.placed_hazards_projection == Some(projection.id()) {
            return;
        }
        let now = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        self.placed_hazards = place_hazards(&self.warnings, &now, &projection).into();
        self.placed_hazards_projection = Some(projection.id());
        self.placed_hazards_at = Some(Instant::now());
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
        // Anchor the map at the radar this volume came from. Re-anchoring is a
        // no-op when the site is unchanged; a genuine site change moves the
        // ground out from under every camera, so each one is re-derived against
        // the new antenna instead of being left on kilometres that now name a
        // different place. See `WorkspaceState::apply_site_change`.
        let opening = self.map_scene.is_default_anchor();
        let previous_anchor = self.map_scene.projection();
        if let (Some(latitude), Some(longitude)) = (
            loaded.volume.site.latitude_deg,
            loaded.volume.site.longitude_deg,
        ) && self
            .map_scene
            .set_radar_anchor(f64::from(latitude), f64::from(longitude))
        {
            let new_anchor = self.map_scene.projection();
            let changed = if opening {
                // Nothing on screen is the analyst's unless they said so, and
                // `--zoom`/`--center` are stated in radar-local kilometres, so
                // the hand-over changes a scale and reprojects nothing.
                self.workspace.leave_overview(
                    PLACEHOLDER_KM_PER_POINT,
                    analyst_runtime::DEFAULT_KM_PER_POINT,
                )
            } else {
                let viewports = array::from_fn(|index| self.panes[index].viewport);
                self.workspace.apply_site_change(&viewports, |world| {
                    let (lon, lat) = previous_anchor?.world_to_lon_lat(world);
                    new_anchor?.try_lon_lat_to_world(lon, lat)
                })
            };
            self.invalidate_view_panes(&changed);
        }

        let before = self.current_frame_signature();
        let before_extent = self.current_frame_extent();
        let stage = loaded.stage;
        let report = self.history.install(VolumeFrame::new(
            loaded.volume,
            loaded.origin,
            stage,
            loaded.source_label,
        ));
        self.load_ms = Some(loaded.elapsed_ms);
        let after = self.current_frame_signature();
        let after_extent = self.current_frame_extent();

        if before != after {
            // A genuinely different frame: the old pixels describe another
            // volume, so they go.
            self.frame_clock.bump();
            self.clear_all_panes();
        } else if before_extent != after_extent {
            // The same frame, grown. Radials were appended under one site,
            // volume time and stage, so the signature above cannot see it and
            // without this the new data never reaches the screen at all.
            //
            // The clock is bumped but the panes are NOT cleared: the installed
            // texture still shows the part of the sweep that had already
            // arrived, and clearing it would blink the pane to empty on every
            // chunk. The texture is replaced when the new render lands.
            self.frame_clock.bump();
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
        let mut quality_changed = false;
        let mut palette_changed = false;
        let mut tilt_delta = 0_isize;
        let visible = self.workspace.visible_panes();
        let cameras_linked = visible
            .iter()
            .all(|pane| self.workspace.pane(*pane).links.camera == Some(0));
        let mut toggle_camera_links = false;
        let mut toggle_warnings = false;

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
            // Its own chip, so an analyst can tell "no warnings out" from "we
            // are not receiving warnings".
            let chip = match self.warnings_state.active() {
                Some(active) => format!("{} · {active}", self.warnings_state.label()),
                None => self.warnings_state.label().to_owned(),
            };
            let response = ui
                .selectable_label(self.show_warnings, chip)
                .on_hover_text(crate::app_support::warnings_hover(
                    &self.warnings_state.detail(),
                    self.show_warnings,
                    self.placed_hazards.len(),
                ));
            if response.clicked() {
                toggle_warnings = true;
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

            let picker_button = ui
                .selectable_label(self.product_picker_open, current_product.label())
                .on_hover_text("Choose a product and its colour table");
            let mut opened_this_frame = false;
            if picker_button.clicked() {
                self.product_picker_open = !self.product_picker_open;
                if self.product_picker_open {
                    self.product_picker.opened(current_product);
                    opened_this_frame = true;
                }
            }
            if self.product_picker_open {
                // Only while open: the picker takes the arrow keys, Enter and
                // Escape off the global event queue every frame it runs, so
                // drawing it unconditionally would eat them from the toolbar.
                let outcome = egui::Area::new(egui::Id::new("workstation-product-picker"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(picker_button.rect.left_bottom() + egui::vec2(0.0, 4.0))
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            draw_product_picker(
                                ui,
                                ProductPickerInput {
                                    state: &mut self.product_picker,
                                    current: current_product,
                                    availability: &self.product_availability,
                                    tables: &self.color_tables,
                                    show_experimental: false,
                                },
                            )
                        })
                    });
                let popup_rect = outcome.response.rect;
                let outcome = outcome.inner.inner;
                if let Some(product) = outcome.product {
                    selected_product = product;
                    self.product_picker_open = false;
                }
                if let Some(selection) = outcome.palette {
                    // Family-wide on purpose: installing a velocity table moves
                    // VEL, DVEL, SRV and DSRV together, because they are the
                    // same measurement drawn four ways.
                    Arc::make_mut(&mut self.color_tables)
                        .set_family(selection.family, selection.table);
                    self.palette_clock.bump();
                    palette_changed = true;
                }
                // `crate::popup` rather than `clicked_elsewhere()`. That method
                // answered yes for the click that OPENED this popup - the click
                // was on the button, which is outside the popup - so the popup
                // opened and closed inside one frame and the product button was
                // dead. The rule now knows about that click.
                let dismissal = crate::popup::dismissal_from_input(
                    ui.ctx(),
                    popup_rect,
                    picker_button.rect,
                    opened_this_frame,
                    outcome.dismissed,
                );
                if dismissal.should_close() {
                    self.product_picker_open = false;
                }
            }

            // A colour table has to be reachable without the popup. It is the
            // control that tells an analyst whether a strange-looking field is
            // the data or the palette, so burying it one level down inside
            // another menu was wrong.
            let palette_family = crate::product_picker::palette_family(current_product);
            if let Some(family) = palette_family {
                let installed = self.color_tables.for_family(family).clone();
                egui::ComboBox::from_id_salt("workstation-palette")
                    .selected_text(installed.name())
                    .width(210.0)
                    .show_ui(ui, |ui| {
                        for table in color_tables::palette_offers_for_family(family, &installed) {
                            let chosen = table.name() == installed.name();
                            if ui.selectable_label(chosen, table.name()).clicked() && !chosen {
                                Arc::make_mut(&mut self.color_tables).set_family(family, table);
                                self.palette_clock.bump();
                                palette_changed = true;
                            }
                        }
                    })
                    .response
                    .on_hover_text(
                        "Colour table for this product's family. The last row is the \
                         selected palette redrawn the other way: smooth or stepped.",
                    );
            }

            crate::app_support::basemap_picker(ui, &mut self.map_scene);

            let mut selected_quality = self.quality;
            egui::ComboBox::from_id_salt("workstation-quality")
                .selected_text(selected_quality.preset_label().unwrap_or("Custom"))
                .width(92.0)
                .show_ui(ui, |ui| {
                    for (label, preset) in render2d::DisplayQuality::PRESETS {
                        ui.selectable_value(&mut selected_quality, preset, label);
                    }
                })
                .response
                .on_hover_text(
                    "Display quality. Smooth adds sub-beams and sub-gates so a gate stops \
                     being a visible block; High and Ultra also supersample, which is what \
                     removes the speckle of a zoomed-out view. Ultra costs about sixteen \
                     times the native raster per frame.",
                );
            if selected_quality != self.quality {
                self.quality = selected_quality;
                quality_changed = true;
            }

            if ui.button("− Tilt").clicked() {
                tilt_delta = -1;
            }
            ui.label(self.active_tilt_label())
                .on_hover_text(self.active_tilt_hover());
            if ui.button("+ Tilt").clicked() {
                tilt_delta = 1;
            }
            if ui
                .selectable_label(cameras_linked, "Link cameras")
                .clicked()
            {
                toggle_camera_links = true;
            }
            if ui
                .selectable_label(self.vol3d.open, "3D")
                .on_hover_text("Volumetric explorer: every tilt resampled into a box and ray marched")
                .clicked()
            {
                self.vol3d.open = !self.vol3d.open;
            }
            if ui
                .selectable_label(self.vrot_active, "Vrot")
                .on_hover_text(
                    "Click two gates across a velocity couplet.
                     Needs a dealiased product: measuring folded velocity gives                      a number wrong by a multiple of the Nyquist that still                      looks reasonable.",
                )
                .clicked()
            {
                self.vrot_active = !self.vrot_active;
                if !self.vrot_active {
                    self.vrot_state.clear();
                    self.vrot_pane = None;
                }
            }
            if self.vrot_state.measurement().is_some() || self.vrot_state.pending().is_some() {
                if ui.button("Clear Vrot").clicked() {
                    self.vrot_state.clear();
                    self.vrot_pane = None;
                }
                if let Some(measurement) = self.vrot_state.measurement() {
                    ui.label(format!("Vrot {:.0} kt", measurement.vrot_knots()));
                }
            }
            ui.label(format!("Pane {}", active.get() + 1));
        });

        if quality_changed || palette_changed {
            // Same data, different picture: every pane's view generation moves,
            // which discards the in-flight render and asks for a new one
            // without throwing away the texture that is currently on screen.
            self.invalidate_view_panes(self.workspace.visible_panes());
        }

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
        if toggle_warnings {
            self.show_warnings = !self.show_warnings;
            // Force placement now rather than at the next cadence, so the map
            // answers the click on this frame.
            self.placed_hazards_at = None;
            self.placed_hazards_projection = None;
            self.refresh_placed_hazards();
            if self.show_warnings {
                self.warnings_service.refresh();
            }
        }
        if toggle_camera_links {
            let new_group = (!cameras_linked).then_some(0);
            for pane in self.workspace.visible_panes() {
                self.workspace.pane_mut(*pane).links.camera = new_group;
            }
        }
    }

    /// The 3D volume explorer, in its own window.
    ///
    /// Follows the active pane's product, so switching that pane to velocity
    /// rebuilds the box from velocity rather than showing a reflectivity body
    /// under a velocity label.
    fn vol3d_window(&mut self, context: &egui::Context) {
        if !self.vol3d.open {
            return;
        }
        let product = DisplayProduct::from_product_id(&self.workspace.active().product);
        let descriptor = product.descriptor();
        // Volume products are already a vertical reduction; there is nothing
        // left to ray march. Fall back to the moment they are built from.
        let moment = descriptor.computation.source_moment();
        let table = crate::palettes::table_for(descriptor, &self.color_tables);
        let range = descriptor.domain.declared_engine_range;
        let candidates = crate::app_support::vol3d_candidates(&self.history);
        let input = crate::vol3d::pane::Vol3dPaneInput {
            candidates: &candidates,
            moment,
            product_label: descriptor.short_name.to_owned(),
            color_table: &table,
            value_range: (range.min, range.max),
        };
        let mut open = self.vol3d.open;
        egui::Window::new("3D Volume")
            .open(&mut open)
            .default_size([900.0, 620.0])
            .show(context, |ui| {
                crate::vol3d::pane::draw_vol3d_pane(&mut self.vol3d, ui, &input);
            });
        self.vol3d.open = open;
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
            // Ask the scene for this pane's LOD. Once resident this is a cache
            // lookup; it queues a build only when the bucket is new.
            let pane_map = PaneMap {
                geometry: self
                    .map_scene
                    .geometry_for_pane(pane.index(), camera.sanitized().km_per_point),
                tiles: self
                    .map_scene
                    .tiles_for_pane(pane.index(), camera, pane_rect),
                projection: self.map_scene.projection(),
                // Paint-time colours for the chosen basemap look. Read from the
                // style the controller is holding rather than stored beside it,
                // so the picker has exactly one thing to set.
                chrome: map_scene::MapChrome::for_style(self.map_scene.style()),
                sites: Arc::clone(&self.placed_sites),
                active_site: self.live_site.clone(),
                hazards: Arc::clone(&self.placed_hazards),
            };
            // Badges describe what limits the picture. Only what is true right
            // now; an empty list is the common case and draws nothing.
            let mut badges: Vec<String> = Vec::new();
            if let Some(frame) = self.history.current()
                && frame.stage != FrameStage::Complete
            {
                badges.push(format!("{:?}", frame.stage).to_uppercase());
            }
            // A hail product computed from a guessed freezing level and one
            // computed from a sounding are different claims. Without this the
            // two look identical on screen, which is the whole reason the
            // environment carries its provenance around with it.
            if product
                .derived_volume()
                .is_some_and(product_engine::registry::DerivedVolumeId::needs_hail_environment)
            {
                badges.push(self.hail_environment.summary());
            }
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
                // The raster is painted with `palettes::table_for`
                // (render_service.rs), so the legend has to read the same table
                // or the bar explains a picture drawn with a different one. For
                // a derived-volume product whose domain is metres or kilograms,
                // a base-moment dBZ ramp does not even intersect the domain, so
                // the legend vanished instead of being wrong visibly: VIL
                // Density had no legend at all.
                let table = crate::palettes::table_for(product.descriptor(), &self.color_tables);
                let layout = crate::legend::legend_layout(&product.domain(), &table);
                let overlay = crate::pane_canvas::PaneOverlay {
                    legend: layout.as_ref(),
                    table: Some(&table),
                    product_name: product.descriptor().short_name,
                    badges: &badges,
                    probe: self.panes[pane.index()].probe_text.as_deref(),
                };
                draw_pane(
                    ui,
                    pane,
                    pane_rect,
                    pane == self.workspace.active_pane,
                    camera,
                    texture,
                    &pane_map,
                    &title,
                    &status,
                    &overlay,
                )
            };

            if self.vrot_active && interaction.clicked {
                self.take_vrot_sample(pane, volume.as_deref(), cut_index, product);
            } else if let Some(site) = interaction.clicked_site {
                // Clicking a site marker is the quickest way to change radar.
                self.workspace.set_active(pane);
                self.site_text = site.to_uppercase();
                self.start_live(site);
            } else if let Some((lon, lat)) = interaction.ctrl_clicked_lon_lat {
                // Ctrl+click loads the nearest S-band NEXRAD. A TDWR sits
                // closer to most downtowns than the WSR-88D does and must never
                // win; `nearest_site` is where that is decided. Note the
                // argument swap: the projection returns (lon, lat) and
                // `nearest_s_band_site` takes (lat, lon).
                self.workspace.set_active(pane);
                match crate::nearest_site::nearest_s_band_site(lat, lon, &self.sites) {
                    Some(choice) => {
                        let status = choice.status_line();
                        self.site_text = choice.id.to_uppercase();
                        self.start_live(choice.id);
                        // AFTER the load kick: `start_live` writes its own
                        // status, so setting this first would be invisible.
                        self.status = status;
                    }
                    None => self.status = crate::nearest_site::no_site_in_range_status(),
                }
            } else if interaction.clicked {
                self.workspace.set_active(pane);
            }
            self.panes[pane.index()].hovered_world_km = interaction.hovered_world_km;
            self.refresh_probe(pane, volume.as_deref(), cut_index, product);
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

        // A volume product needs the measurement; without it there is nothing
        // to select tilts from, and drawing an empty field would look like a
        // storm-free sky rather than a missing prerequisite.
        let Some(capabilities) = self.capabilities.as_ref().map(Arc::clone) else {
            return;
        };
        // A sweep still filling is drawn over the last complete picture of the
        // same tilt. A complete sweep is not blended at all, so an archive file
        // renders down exactly the path it always did.
        let sweep = self.panes[pane.index()]
            .sweep_state
            .filter(|state| !state.complete)
            .and_then(|state| {
                let moment = product.source_moment();
                let (previous_volume, previous_cut_index) = crate::app_support::previous_sweep_for(
                    &self.history,
                    &volume,
                    cut_index,
                    &moment,
                )?;
                Some(SweepBlendRequest {
                    previous_volume,
                    previous_cut_index,
                    start_deg: state.start_deg,
                    revealed_deg: state.revealed_deg,
                })
            });

        let request = RenderRequest {
            pane,
            stamp,
            volume,
            capabilities,
            environment: self.hail_environment.clone(),
            cut_index,
            product,
            camera: self.workspace.pane(pane).camera,
            viewport,
            storm_motion: self.workspace.pane(pane).storm_motion,
            color_tables: Arc::clone(&self.color_tables),
            quality: self.quality,
            sweep,
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

    /// Step every pane's sweep reveal on by one frame.
    ///
    /// Only panes with no render in flight are stepped, and that restriction is
    /// load-bearing rather than an optimisation. The reveal is part of the
    /// render stamp, so moving it while a render is running would make that
    /// render stale the instant it landed, `install_render` would drop it, and
    /// the pane would never install anything at all. Tying each step to the
    /// completion of the last one also makes the animation self-pacing: a
    /// slower render takes fewer, larger steps instead of falling behind.
    fn advance_sweeps(&mut self) {
        let Some((identity, volume)) = self
            .history
            .current()
            .map(|frame| (frame.identity.clone(), Arc::clone(&frame.volume)))
        else {
            for runtime in &mut self.panes {
                runtime.reset_sweep();
            }
            return;
        };

        // Only the live edge animates. A frame the analyst has scrubbed back to
        // is finished data, and revealing it a spoke at a time would animate
        // history rather than report on an arriving sweep.
        if !self.history.at_live_edge() {
            for runtime in &mut self.panes {
                runtime.reset_sweep();
            }
            return;
        }

        let now = Instant::now();
        for index in 0..analyst_runtime::MAX_PANES {
            let Some(pane) = PaneId::new(index as u8) else {
                continue;
            };
            // Resolved before the mutable borrow below: both read `self`.
            let product = DisplayProduct::from_product_id(&self.workspace.pane(pane).product);
            let cut_index = self.resolve_cut_index(pane, &volume);
            let key = cut_index.map(|cut_index| SweepKey {
                identity: identity.clone(),
                product: product.id(),
                cut_index,
            });
            let runtime = &mut self.panes[index];

            let (Some(cut_index), Some(key)) = (cut_index, key) else {
                runtime.reset_sweep();
                continue;
            };
            if runtime.sweep_key.as_ref() != Some(&key) {
                runtime.reset_sweep();
                runtime.sweep_key = Some(key);
            }
            if runtime.pending_stamp.is_some() {
                continue;
            }
            let Some(cut) = volume.cuts.get(cut_index) else {
                runtime.reset_sweep();
                continue;
            };

            let elapsed = runtime
                .sweep_stepped_at
                .map(|stepped_at| now.saturating_duration_since(stepped_at))
                .unwrap_or_default();
            let catch_up = runtime
                .sweep_state
                .map(|state| catch_up_factor(state.pending_deg()))
                .unwrap_or(1.0);
            let before = runtime.sweep_state;
            let after = runtime.sweep.observe(cut, elapsed.mul_f32(catch_up));
            runtime.sweep_state = after;
            runtime.sweep_stepped_at = Some(now);

            // Only a reveal that actually moved is worth a render. Without this
            // a settled pane would re-render every frame forever, because the
            // stamp would change on every step whether or not the picture did.
            if before != after {
                self.sweep_clocks[index].bump();
            }
        }
    }

    /// The tilt as it was in the previous frame, for a sweep still arriving.
    ///
    /// `None` is not a failure: the first volume after a site change genuinely
    /// has nothing older to underpaint with, and the blend then draws the
    /// arrived wedge alone, which is what the pane did before any of this
    /// existed.
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
            runtime.reset_sweep();
        }
    }

    fn clear_all_panes(&mut self) {
        for runtime in &mut self.panes {
            runtime.texture = None;
            runtime.pending_stamp = None;
            runtime.status.clear();
            // The reveal describes a position in a sweep that is no longer on
            // screen. Easing on from it would wipe the new picture in from
            // wherever the old one happened to have got to.
            runtime.reset_sweep();
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
            sweep: self.sweep_clocks[pane.index()].current(),
        }
    }

    /// Re-measure the current volume when anything about it changes.
    ///
    /// The key includes the cut and radial counts, not just the frame identity
    /// and stage. A live volume grows in place: chunks arrive, radials are
    /// appended and whole cuts are added, all under one site and volume time at
    /// stage `Partial`. Keying on identity alone would measure the first
    /// fragment that arrived and then answer every later question from it, so
    /// the pane would keep drawing the tilt that existed a minute ago.
    fn refresh_capabilities(&mut self) {
        let key = self.history.current().map(|frame| CapabilitiesKey {
            identity: frame.identity.clone(),
            stage: frame.stage,
            cuts: frame.volume.cuts.len(),
            radials: frame.volume.cuts.iter().map(|cut| cut.radials.len()).sum(),
        });
        if key == self.capabilities_for && self.capabilities.is_some() {
            return;
        }
        self.capabilities = self
            .history
            .current()
            .map(|frame| Arc::new(product_engine::VolumeCapabilities::analyze(&frame.volume)));
        self.capabilities_for = key;
        // Greying out a product the volume cannot show is a claim about the
        // data, so it is remeasured wherever the measurement is.
        self.product_availability =
            ProductAvailabilityIndex::from_optional_capabilities(self.capabilities.as_deref());
    }

    fn current_frame_signature(&self) -> Option<(analyst_runtime::FrameIdentity, FrameStage)> {
        self.history
            .current()
            .map(|frame| (frame.identity.clone(), frame.stage))
    }

    /// How much data the current frame holds, as (cuts, radials).
    ///
    /// A live volume grows in place: chunks arrive and radials are appended
    /// under one site, one volume time and the stage `Partial`. Its identity
    /// and stage therefore do not change while it fills, which is why growth
    /// needs its own measure - see `install_loaded_volume`.
    fn current_frame_extent(&self) -> Option<(usize, usize)> {
        self.history.current().map(|frame| {
            (
                frame.volume.cuts.len(),
                frame.volume.cuts.iter().map(|cut| cut.radials.len()).sum(),
            )
        })
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

    /// Which sweep this pane should draw.
    ///
    /// Delegates to `product_engine::cut_selection`, which chooses by measured
    /// elevation and scan time rather than by position in the file. The
    /// difference is not cosmetic: on a VCP 212 SAILSx3 volume the lowest tilt
    /// is scanned four times across the volume period, and taking the first one
    /// listed serves velocity that is over four minutes older than a sweep of
    /// the same tilt sitting in the same file.
    /// Take one endpoint of a Vrot measurement from the point just clicked.
    fn take_vrot_sample(
        &mut self,
        pane: PaneId,
        volume: Option<&RadarVolume>,
        cut_index: Option<usize>,
        product: DisplayProduct,
    ) {
        self.workspace.set_active(pane);
        if self.vrot_pane != Some(pane) {
            // Starting in a different pane abandons the half-finished pair
            // rather than pairing gates from two different pictures.
            self.vrot_state.clear();
            self.vrot_pane = Some(pane);
        }
        let Some((east_km, north_km)) = self.panes[pane.index()].hovered_world_km else {
            return;
        };
        let (Some(volume), Some(cut_index)) = (volume, cut_index) else {
            return;
        };
        let descriptor = product.descriptor();
        let elevation_deg = self
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.cut(cut_index))
            .map(|cut| cut.nominal_elevation_deg)
            .or_else(|| volume.cuts.get(cut_index).map(|cut| cut.elevation_deg))
            .unwrap_or_default();
        let reading = crate::probe::probe_polar(
            volume,
            cut_index,
            &descriptor.computation.source_moment(),
            elevation_deg,
            volume.site.elevation_m,
            east_km,
            north_km,
        );
        let crate::probe::ProbeReading::Value(value) = reading else {
            self.status = "Vrot: that point has no velocity".to_owned();
            return;
        };
        let sample = crate::vrot::VrotSample::from_probe(&value);

        match self.vrot_state.pending().cloned() {
            None => {
                self.vrot_state = crate::vrot::VrotState::AwaitingSecond(sample);
                self.status = "Vrot: click the other side of the couplet".to_owned();
            }
            Some(first) => {
                let dealiased = descriptor.computation.uses_dealiased_velocity();
                match crate::vrot::measure(first, sample, dealiased) {
                    Ok(measurement) => {
                        self.status = crate::vrot::report(&measurement);
                        self.vrot_state = crate::vrot::VrotState::Complete(measurement);
                    }
                    Err(refusal) => {
                        self.status = format!("Vrot refused: {}", refusal.label());
                        self.vrot_state = crate::vrot::VrotState::Idle;
                    }
                }
            }
        }
    }

    /// Read the value under this pane's cursor from the sweep it is drawing.
    ///
    /// Uses the pointer position captured during the previous paint, so the
    /// volume is never scanned while laying out a frame.
    fn refresh_probe(
        &mut self,
        pane: PaneId,
        volume: Option<&RadarVolume>,
        cut_index: Option<usize>,
        product: DisplayProduct,
    ) {
        let Some((east_km, north_km)) = self.panes[pane.index()].hovered_world_km else {
            self.panes[pane.index()].probe_text = None;
            return;
        };
        let (Some(volume), Some(cut_index)) = (volume, cut_index) else {
            self.panes[pane.index()].probe_text = None;
            return;
        };
        let descriptor = product.descriptor();
        // The measured elevation, so the beam height is computed from the angle
        // the antenna actually flew rather than from the first radial's.
        let elevation_deg = self
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.cut(cut_index))
            .map(|cut| cut.nominal_elevation_deg)
            .or_else(|| volume.cuts.get(cut_index).map(|cut| cut.elevation_deg))
            .unwrap_or_default();
        let reading = crate::probe::probe_polar(
            volume,
            cut_index,
            &descriptor.computation.source_moment(),
            elevation_deg,
            volume.site.elevation_m,
            east_km,
            north_km,
        );
        self.panes[pane.index()].probe_text = Some(crate::probe::format_reading(
            &reading,
            &descriptor.domain,
            descriptor.short_name,
        ));
    }

    fn resolve_cut_index(&self, pane: PaneId, volume: &RadarVolume) -> Option<usize> {
        let intent = self.workspace.pane(pane);
        let product = DisplayProduct::from_product_id(&intent.product);
        let descriptor = product.descriptor();
        let moment = descriptor.computation.source_moment();
        let policy = descriptor.cut_policy;

        let Some(capabilities) = self.capabilities.as_ref() else {
            // Measurement has not run yet this frame. Draw something rather
            // than nothing; the next frame will have the real answer.
            return product.first_available_cut(volume);
        };

        match intent.tilt {
            TiltSelection::LowestAvailable => {
                product_engine::cut_selection::select_lowest_tilt(capabilities, &moment, policy)
                    .map(|choice| choice.cut_index)
            }
            TiltSelection::CutIndex(index) => {
                let index = usize::from(index);
                product
                    .is_available_in_cut(volume, index)
                    .then_some(index)
                    .or_else(|| {
                        product_engine::cut_selection::select_lowest_tilt(
                            capabilities,
                            &moment,
                            policy,
                        )
                        .map(|choice| choice.cut_index)
                    })
            }
            TiltSelection::NearestElevationTenths(target) => {
                product_engine::cut_selection::select_nearest_elevation(
                    capabilities,
                    f32::from(target) / 10.0,
                    &moment,
                    policy,
                )
                .map(|choice| choice.cut_index)
            }
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
        // Step one commanded tilt, not one cut. On a split-cut volume the next
        // entry in the cut list is the other leg of the same elevation, so
        // stepping by index makes "+ Tilt" stand still.
        let next = match self.capabilities.as_ref() {
            Some(capabilities) => product_engine::cut_selection::step_tilt(
                capabilities,
                current,
                delta,
                &product.descriptor().computation.source_moment(),
                product.descriptor().cut_policy,
            )
            .map(|choice| choice.cut_index),
            None => product.next_available_cut(&volume, current, delta),
        };
        let Some(next) = next else {
            return;
        };
        let changed = self
            .workspace
            .apply_tilt_from(active, TiltSelection::CutIndex(next as u16));
        self.invalidate_semantic_panes(&changed);
    }

    /// Why this sweep and not another. Shown on hover over the tilt readout,
    /// because "the pane jumped from 0.48 to 0.44 degrees" is otherwise an
    /// unexplained change rather than a four-minute-fresher picture.
    fn active_tilt_hover(&self) -> String {
        let Some(capabilities) = self.capabilities.as_ref() else {
            return "No volume measured yet".to_owned();
        };
        let pane = self.workspace.active_pane;
        let product = DisplayProduct::from_product_id(&self.workspace.pane(pane).product);
        let descriptor = product.descriptor();
        let moment = descriptor.computation.source_moment();
        let Some(choice) = product_engine::cut_selection::select_lowest_tilt(
            capabilities,
            &moment,
            descriptor.cut_policy,
        ) else {
            return format!("No sweep in this volume carries {moment}");
        };
        let Some(cut) = capabilities.cut(choice.cut_index) else {
            return "No sweep selected".to_owned();
        };
        let mut lines = vec![
            format!(
                "cut {} of {} - {} leg at {:.2}° (stored {:.2}°)",
                choice.cut_index,
                capabilities.cuts.len(),
                choice.leg.label(),
                cut.nominal_elevation_deg,
                cut.stored_elevation_deg
            ),
            format!(
                "{} radials, {:.0}° of azimuth{}",
                cut.radial_count,
                cut.azimuth_coverage_deg,
                if cut.complete { "" } else { ", still arriving" }
            ),
        ];
        if let Some(nyquist) = cut.representative_nyquist_mps {
            lines.push(format!("Nyquist {nyquist:.1} m/s"));
        }
        if choice.repeats_passed_over > 0 {
            lines.push(format!(
                "{} other sweep(s) of this tilt in the volume",
                choice.repeats_passed_over
            ));
        }
        if choice.older_alternative_ms > 0 {
            lines.push(format!(
                "{:.1} s fresher than the first sweep listed in the file",
                choice.older_alternative_ms as f32 / 1000.0
            ));
        }
        lines.join(
            "
",
        )
    }

    fn active_tilt_label(&self) -> String {
        let Some(frame) = self.history.current() else {
            return "No tilt".to_owned();
        };
        let Some(index) = self.resolve_cut_index(self.workspace.active_pane, &frame.volume) else {
            return "Unavailable".to_owned();
        };
        // The measured elevation, not the stored one. The stored angle is the
        // first radial's, taken while the antenna is still ramping onto the
        // tilt, so real 0.5-degree sweeps label themselves "0.4" and disagree
        // with every other radar viewer.
        self.capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.cut(index))
            // Two decimals, not one. The commanded tilt is 0.5 degrees but the
            // antenna flies 0.44, and rounding that to "0.4" reads as a wrong
            // 0.5 rather than as a right measurement. There is no VCP
            // elevation table here to recover the commanded angle from, so the
            // honest thing is to show what was measured, precisely enough that
            // nobody mistakes it for a label.
            .map(|cut| format!("{:.2}°", cut.nominal_elevation_deg))
            .or_else(|| {
                frame
                    .volume
                    .cuts
                    .get(index)
                    .map(|cut| format!("{:.2}°", cut.elevation_deg))
            })
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
        self.poll_site_directory();
        self.poll_warnings();
        // Before anything asks which sweep to draw.
        self.refresh_capabilities();
        self.map_scene
            .set_pixels_per_point(context.pixels_per_point());
        self.map_scene.poll();
        self.refresh_placed_sites();
        self.refresh_placed_hazards();
        self.poll_render_results(&context);
        // After the results, before the canvas asks for the next render:
        // the reveal only steps for panes whose previous render has landed.
        self.advance_sweeps();
        self.advance_playback(&context);

        ui.visuals_mut().panel_fill = egui::Color32::from_rgb(10, 13, 17);
        self.vol3d_window(&context);
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

        // A reveal that has caught up with the data is not animating: it is
        // waiting for a chunk, and the load service wakes the UI when one
        // lands. Repainting anyway would spin at 60 Hz over a picture that
        // cannot change.
        let animating = self.panes.iter().any(|pane| {
            pane.pending_stamp.is_some()
                || pane
                    .sweep_state
                    .is_some_and(|state| !state.complete && state.pending_deg() > 0.0)
        });
        if animating {
            context.request_repaint_after(Duration::from_millis(16));
        }
    }
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

    fn first_pane() -> PaneId {
        PaneId::new(0).expect("pane 0 always exists")
    }

    #[test]
    fn a_pane_header_names_the_unit_its_readout_will_be_in() {
        assert_eq!(
            pane_title(None, first_pane(), DisplayProduct::Reflectivity, None),
            "1 · REF (dBZ)"
        );
    }

    #[test]
    fn a_pane_header_distinguishes_the_two_velocity_style_units() {
        // Velocity reads in knots and spectrum width in metres per second, and
        // the header is where an analyst finds that out before misreading a
        // threshold quoted in the other one.
        assert_eq!(
            pane_title(None, first_pane(), DisplayProduct::DealiasedVelocity, None),
            "1 · DVEL (kt)"
        );
        assert_eq!(
            pane_title(None, first_pane(), DisplayProduct::SpectrumWidth, None),
            "1 · SW (m/s)"
        );
    }

    #[test]
    fn a_dimensionless_product_header_carries_no_empty_parentheses() {
        assert_eq!(
            pane_title(
                None,
                first_pane(),
                DisplayProduct::CorrelationCoefficient,
                None
            ),
            "1 · RHO"
        );
    }
}
