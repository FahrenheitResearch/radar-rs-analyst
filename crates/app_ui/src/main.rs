use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use data_source::{LEVEL2_ARCHIVE_BUCKET, RadarSite};
use eframe::egui;
use radar_core::{ElevationCut, MomentGrid, MomentStorage, MomentType, RadarVolume};
use render2d::{
    StormMotion, StormRelativePaletteCache, ViewportMomentCache, ViewportRasterOptions,
    ViewportSampleCache, storm_relative_velocity_mps, viewport_rgba_buffer_len,
    viewport_sample_cache_storage_upper_bound,
};

mod basemap_data;

const MIN_DISPLAYABLE_RADIALS: usize = 180;
const DEFAULT_MAP_SCALE: f32 = 115.0;
const MIN_MAP_SCALE: f32 = 2.0;
const MAX_MAP_SCALE: f32 = 3_200.0;
const DEFAULT_RADAR_RANGE_KM: f32 = 460.0;
const DEFAULT_STORM_MOTION_DIRECTION_DEG: f32 = 45.0;
const DEFAULT_STORM_MOTION_SPEED_KT: f32 = 35.0;
const KNOT_TO_MPS: f32 = 0.514_444;
const VROT_ROW_RADIUS: usize = 2;
const VROT_GATE_RADIUS: usize = 4;
const SPECULATIVE_SAMPLE_CACHE_MIN_PIXELS: u64 = 720 * 480;
const LOW_END_SPECULATIVE_SAMPLE_CACHE_MIN_RENDER_MS: f32 = 4.0;
const HIGH_END_SPECULATIVE_SAMPLE_CACHE_MIN_RENDER_MS: f32 = 0.25;
const LOW_END_SAMPLE_CACHE_BYTES: usize = 6 * 1024 * 1024;
const LOW_END_SAMPLE_CACHE_BUILD_BYTES: usize = LOW_END_SAMPLE_CACHE_BYTES * 2;
const MID_RANGE_SAMPLE_CACHE_BYTES: usize = 24 * 1024 * 1024;
const HIGH_END_SAMPLE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const LOW_CORE_PREVIEW_THREADS: usize = 4;
const LOW_CORE_PREVIEW_RENDER_HEAD_START_MS: u64 = 8;
const ACTIVE_LOAD_POLL_MS: u64 = 8;
const PERF_SAMPLE_CAPACITY: usize = 96;
const LATEST_OBJECT_CACHE_TTL: Duration = Duration::from_secs(20);
const BASEMAP_US_DETAIL_BOUNDS: &[[f32; 4]] = &[
    [-125.5, 24.0, -66.0, 50.3],
    [-171.0, 51.0, -129.0, 72.0],
    [-161.5, 18.5, -154.5, 23.0],
    [-68.5, 17.0, -64.0, 19.0],
];
const FORCE_PREVIEW_ENV: &str = "RADAR_RS_FORCE_PREVIEW";
const RAYON_NUM_THREADS_ENV: &str = "RAYON_NUM_THREADS";

fn main() -> eframe::Result {
    let input_path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_sample_path);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1320.0, 900.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Radar RS Analyst",
        native_options,
        Box::new(move |cc| Ok(Box::new(ViewerApp::new(cc, input_path)))),
    )
}

fn default_sample_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("work")
        .join("radar-rs-analyst-samples")
        .join("KTLX20130520_201643_V06.gz")
}

fn cache_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("work")
        .join("radar-rs-analyst-cache")
        .join(name)
}

fn should_preview_loads() -> bool {
    should_preview_loads_for_threads(
        std::env::var_os(FORCE_PREVIEW_ENV).is_some(),
        effective_worker_threads(),
    )
}

fn should_preview_loads_for_threads(force_preview: bool, threads: usize) -> bool {
    force_preview || threads <= LOW_CORE_PREVIEW_THREADS
}

fn effective_worker_threads() -> usize {
    configured_rayon_threads_from(std::env::var(RAYON_NUM_THREADS_ENV).ok().as_deref())
        .or_else(|| thread::available_parallelism().ok().map(usize::from))
        .unwrap_or(1)
}

fn configured_rayon_threads_from(value: Option<&str>) -> Option<usize> {
    value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|threads| *threads > 0)
}

fn preview_render_head_start(threads: usize) -> Duration {
    if threads <= LOW_CORE_PREVIEW_THREADS {
        Duration::from_millis(LOW_CORE_PREVIEW_RENDER_HEAD_START_MS)
    } else {
        Duration::ZERO
    }
}

fn decode_load_path_with_optional_preview(
    path: PathBuf,
    label: &str,
    total_start: Instant,
    mut timings: LoadTimings,
    sender: &mpsc::Sender<AsyncLoadResult>,
    preview_enabled: bool,
) -> Result<DecodedLoad, String> {
    let read_start = Instant::now();
    let raw = std::fs::read(&path)
        .map_err(|err| format!("I/O error reading {}: {err}", path.display()))?;
    timings.read_ms = Some(read_start.elapsed().as_secs_f32() * 1000.0);

    if !preview_enabled {
        let decode_start = Instant::now();
        let mut volume =
            nexrad_io::decode_volume_from_bytes(&raw).map_err(|err| err.to_string())?;
        timings.decode_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
        volume.metadata.source_path = Some(path.display().to_string());
        return Ok(DecodedLoad {
            path,
            volume,
            timings: timings.finish(total_start),
        });
    }

    let preview_head_start = preview_render_head_start(effective_worker_threads());
    let preview_path = path.clone();
    let preview_label = label.to_owned();
    let decode_start = Instant::now();
    let mut first_preview_ms = None;
    let mut send_preview = |mut preview: RadarVolume| {
        let preview_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
        first_preview_ms.get_or_insert(preview_ms);
        let mut preview_timings = timings;
        preview_timings.decode_ms = preview_ms;
        preview_timings.preview_ms = Some(preview_ms);
        preview.metadata.source_path = Some(preview_path.display().to_string());
        let sent = sender.send(AsyncLoadResult {
            label: preview_label.clone(),
            update: AsyncLoadUpdate::Preview(DecodedLoad {
                path: preview_path.clone(),
                volume: preview,
                timings: preview_timings.finish(total_start),
            }),
        });
        if sent.is_ok() && !preview_head_start.is_zero() {
            thread::sleep(preview_head_start);
        }
    };
    let mut volume = if raw.starts_with(&[0x1f, 0x8b]) {
        if let Some(preview) =
            nexrad_io::decode_gzip_preview_from_bytes(&raw, MIN_DISPLAYABLE_RADIALS)
                .map_err(|err| err.to_string())?
        {
            send_preview(preview);
        }
        nexrad_io::decode_volume_from_bytes(&raw).map_err(|err| err.to_string())?
    } else {
        nexrad_io::decode_volume_from_bytes_with_bzip_preview(
            &raw,
            MIN_DISPLAYABLE_RADIALS,
            |preview| {
                send_preview(preview);
            },
        )
        .map_err(|err| err.to_string())?
    };
    timings.decode_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
    timings.preview_ms = first_preview_ms;
    volume.metadata.source_path = Some(path.display().to_string());
    Ok(DecodedLoad {
        path,
        volume,
        timings: timings.finish(total_start),
    })
}

struct ViewerApp {
    source_path: PathBuf,
    volume: Option<Arc<RadarVolume>>,
    selected_cut: usize,
    selected_product: DisplayProduct,
    texture: Option<egui::TextureHandle>,
    texture_key: Option<TextureKey>,
    render_sender: mpsc::Sender<RenderRequest>,
    render_receiver: mpsc::Receiver<AsyncRenderResult>,
    render_recycle_sender: mpsc::Sender<RenderRecycleBuffer>,
    pending_render_key: Option<TextureKey>,
    map_center_lon: f32,
    map_center_lat: f32,
    map_scale: f32,
    radar_range_km: f32,
    load_timing: Option<LoadTimings>,
    render_ms: Option<f32>,
    worker_ms: Option<f32>,
    texture_ms: Option<f32>,
    sample_cache_build_ms: Option<f32>,
    basemap_ms: Option<f32>,
    perf: PerfTelemetry,
    status: String,
    sites: Vec<RadarSite>,
    selected_site_index: usize,
    site_catalog_receiver: Option<mpsc::Receiver<AsyncSiteCatalogResult>>,
    load_receiver: Option<mpsc::Receiver<AsyncLoadResult>>,
    pending_site_id: Option<String>,
    cursor_readout: Option<CursorReadout>,
    storm_motion_direction_deg: f32,
    storm_motion_speed_kt: f32,
}

struct AsyncLoadResult {
    label: String,
    update: AsyncLoadUpdate,
}

enum AsyncLoadUpdate {
    Preview(DecodedLoad),
    Final(Result<DecodedLoad, String>),
}

#[derive(Clone, Copy, Debug, Default)]
struct LoadTimings {
    total_ms: f32,
    lookup_ms: Option<f32>,
    lookup_cache_hit: Option<bool>,
    fetch_ms: Option<f32>,
    fetch_cache_hit: Option<bool>,
    read_ms: Option<f32>,
    decode_ms: f32,
    preview_ms: Option<f32>,
}

impl LoadTimings {
    fn finish(mut self, total_start: Instant) -> Self {
        self.total_ms = total_start.elapsed().as_secs_f32() * 1000.0;
        self
    }
}

struct AsyncSiteCatalogResult {
    result: Result<Vec<RadarSite>, String>,
}

struct DecodedLoad {
    path: PathBuf,
    volume: RadarVolume,
    timings: LoadTimings,
}

struct AsyncRenderResult {
    key: TextureKey,
    result: Result<RenderedTexture, String>,
}

struct RenderRequest {
    key: TextureKey,
    volume: Arc<RadarVolume>,
    cut: usize,
    product: DisplayProduct,
    storm_motion: StormMotion,
    viewport_options: ViewportRasterOptions,
    radar_range_km: f32,
}

struct RenderedTexture {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    buffer_signature: RenderWorkerViewportSignature,
    render_ms: f32,
    worker_ms: f32,
    sample_cache_build_ms: Option<f32>,
    used_sample_cache: bool,
    radar_range_km: f32,
}

struct RenderRecycleBuffer {
    rgba: Vec<u8>,
    signature: Option<RenderWorkerViewportSignature>,
}

#[derive(Clone, Debug)]
struct PerfTelemetry {
    decode: MetricSeries,
    direct_render: MetricSeries,
    cached_render: MetricSeries,
    worker: MetricSeries,
    texture: MetricSeries,
    cache_build: MetricSeries,
}

impl PerfTelemetry {
    fn new() -> Self {
        Self {
            decode: MetricSeries::new(),
            direct_render: MetricSeries::new(),
            cached_render: MetricSeries::new(),
            worker: MetricSeries::new(),
            texture: MetricSeries::new(),
            cache_build: MetricSeries::new(),
        }
    }

    fn record_decode(&mut self, ms: f32) {
        self.decode.push(ms);
    }

    fn record_render(
        &mut self,
        render_ms: f32,
        used_sample_cache: bool,
        worker_ms: f32,
        texture_ms: f32,
        sample_cache_build_ms: Option<f32>,
    ) {
        if used_sample_cache {
            self.cached_render.push(render_ms);
        } else {
            self.direct_render.push(render_ms);
        }
        self.worker.push(worker_ms);
        self.texture.push(texture_ms);
        if let Some(sample_cache_build_ms) = sample_cache_build_ms {
            self.cache_build.push(sample_cache_build_ms);
        }
    }
}

#[derive(Clone, Debug)]
struct MetricSeries {
    samples: [f32; PERF_SAMPLE_CAPACITY],
    start: usize,
    len: usize,
    latest: f32,
}

impl MetricSeries {
    fn new() -> Self {
        Self {
            samples: [0.0; PERF_SAMPLE_CAPACITY],
            start: 0,
            len: 0,
            latest: 0.0,
        }
    }

    fn push(&mut self, ms: f32) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        self.latest = ms;
        if self.len < PERF_SAMPLE_CAPACITY {
            let index = (self.start + self.len) % PERF_SAMPLE_CAPACITY;
            self.samples[index] = ms;
            self.len += 1;
        } else {
            self.samples[self.start] = ms;
            self.start = (self.start + 1) % PERF_SAMPLE_CAPACITY;
        }
    }

    fn summary(&self) -> Option<MetricSummary> {
        if self.len == 0 {
            return None;
        }

        let mut sorted = [0.0; PERF_SAMPLE_CAPACITY];
        for (target, source) in sorted.iter_mut().zip((0..self.len).map(|offset| {
            let index = (self.start + offset) % PERF_SAMPLE_CAPACITY;
            self.samples[index]
        })) {
            *target = source;
        }
        let sorted = &mut sorted[..self.len];
        sorted.sort_by(|a, b| a.total_cmp(b));

        Some(MetricSummary {
            latest: self.latest,
            min: sorted[0],
            p50: sorted[percentile_index(self.len, 50)],
            p95: sorted[percentile_index(self.len, 95)],
            max: sorted[self.len - 1],
            count: self.len,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MetricSummary {
    latest: f32,
    min: f32,
    p50: f32,
    p95: f32,
    max: f32,
    count: usize,
}

fn percentile_index(len: usize, percentile: usize) -> usize {
    if len <= 1 {
        return 0;
    }
    ((len - 1) * percentile + 50) / 100
}

struct RenderWorkerMomentCache {
    volume_ptr: usize,
    cut: usize,
    moment: MomentType,
    cache: ViewportMomentCache,
    storm_palette_cache: Option<RenderWorkerStormPaletteCache>,
}

struct RenderWorkerStormPaletteCache {
    storm_motion_key: (i16, i16),
    cache: Option<StormRelativePaletteCache>,
}

struct RenderWorkerSampleCache {
    signature: RenderWorkerViewportSignature,
    cache: ViewportSampleCache,
}

#[derive(Clone, Copy, Debug)]
struct RenderWorkerCachePolicy {
    threads: usize,
}

impl RenderWorkerCachePolicy {
    fn detect() -> Self {
        Self {
            threads: effective_worker_threads(),
        }
    }

    fn should_speculatively_warm_sample_cache(&self, rendered: &RenderedTexture) -> bool {
        if rendered.used_sample_cache {
            return false;
        }
        let pixels = rendered.width as u64 * rendered.height as u64;
        let min_render_ms = if self.threads <= 7 {
            LOW_END_SPECULATIVE_SAMPLE_CACHE_MIN_RENDER_MS
        } else {
            HIGH_END_SPECULATIVE_SAMPLE_CACHE_MIN_RENDER_MS
        };
        pixels >= SPECULATIVE_SAMPLE_CACHE_MIN_PIXELS
            && rendered.render_ms >= min_render_ms
            && self.can_attempt_sample_cache_build(rendered.buffer_signature.viewport.dimensions())
    }

    #[cfg(test)]
    fn should_build_sample_cache_for_viewport(&self, viewport: ViewportKey) -> bool {
        self.can_store_sample_cache(viewport.dimensions())
    }

    fn should_build_sample_cache_for_moment_cache(
        &self,
        cache: &ViewportMomentCache,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
    ) -> Result<bool, String> {
        let upper_bound = cache
            .sample_cache_storage_upper_bound(volume, options)
            .map_err(|err| err.to_string())?;
        Ok(self.can_store_sample_cache_bytes(upper_bound))
    }

    fn should_prefetch_interaction_cache(&self, dimensions: (u32, u32)) -> bool {
        let (width, height) = dimensions;
        let pixels = width as u64 * height as u64;
        self.threads >= 8
            && pixels >= SPECULATIVE_SAMPLE_CACHE_MIN_PIXELS
            && self.can_store_sample_cache(dimensions)
    }

    fn can_store_sample_cache(&self, dimensions: (u32, u32)) -> bool {
        let (width, height) = dimensions;
        let upper_bound = viewport_sample_cache_storage_upper_bound(ViewportRasterOptions {
            width,
            height,
            radar_x_px: 0.0,
            radar_y_px: 0.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
        });
        self.can_store_sample_cache_bytes(upper_bound)
    }

    fn can_attempt_sample_cache_build(&self, dimensions: (u32, u32)) -> bool {
        let (width, height) = dimensions;
        let upper_bound = viewport_sample_cache_storage_upper_bound(ViewportRasterOptions {
            width,
            height,
            radar_x_px: 0.0,
            radar_y_px: 0.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
        });
        upper_bound <= self.sample_cache_build_bytes()
    }

    fn can_store_sample_cache_bytes(&self, bytes: usize) -> bool {
        bytes <= self.sample_cache_bytes()
    }

    fn sample_cache_capacity(&self) -> usize {
        match self.threads {
            0..=7 => 1,
            8..=15 => 3,
            _ => 6,
        }
    }

    fn sample_cache_bytes(&self) -> usize {
        match self.threads {
            0..=7 => LOW_END_SAMPLE_CACHE_BYTES,
            8..=15 => MID_RANGE_SAMPLE_CACHE_BYTES,
            _ => HIGH_END_SAMPLE_CACHE_BYTES,
        }
    }

    fn sample_cache_build_bytes(&self) -> usize {
        match self.threads {
            0..=7 => LOW_END_SAMPLE_CACHE_BUILD_BYTES,
            _ => self.sample_cache_bytes(),
        }
    }

    fn direct_viewport_capacity(&self) -> usize {
        self.sample_cache_capacity().saturating_mul(2).max(1)
    }

    fn moment_cache_capacity(&self) -> usize {
        self.sample_cache_capacity()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenderWorkerViewportSignature {
    volume_ptr: usize,
    cut: usize,
    moment: MomentType,
    viewport: ViewportKey,
}

impl RenderWorkerViewportSignature {
    fn new(volume_ptr: usize, cut: usize, moment: MomentType, viewport: ViewportKey) -> Self {
        Self {
            volume_ptr,
            cut,
            moment,
            viewport,
        }
    }
}

fn spawn_render_worker() -> (
    mpsc::Sender<RenderRequest>,
    mpsc::Receiver<AsyncRenderResult>,
    mpsc::Sender<RenderRecycleBuffer>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<RenderRequest>();
    let (result_sender, result_receiver) = mpsc::channel::<AsyncRenderResult>();
    let (recycle_sender, recycle_receiver) = mpsc::channel::<RenderRecycleBuffer>();

    thread::spawn(move || {
        let cache_policy = RenderWorkerCachePolicy::detect();
        let mut reusable_pixels = Vec::new();
        let mut reusable_pixels_signature: Option<RenderWorkerViewportSignature> = None;
        let mut moment_caches: Vec<RenderWorkerMomentCache> = Vec::new();
        let mut sample_caches: Vec<RenderWorkerSampleCache> = Vec::new();
        let mut last_direct_viewports: Vec<RenderWorkerViewportSignature> = Vec::new();
        let mut deferred_request: Option<RenderRequest> = None;
        loop {
            let mut request = if let Some(request) = deferred_request.take() {
                request
            } else {
                match request_receiver.recv() {
                    Ok(request) => request,
                    Err(_) => break,
                }
            };
            for newer_request in request_receiver.try_iter() {
                request = newer_request;
            }
            let requested_buffer_signature = RenderWorkerViewportSignature::new(
                Arc::as_ptr(&request.volume) as usize,
                request.cut,
                request.product.base_moment(),
                request.key.viewport,
            );
            while let Ok(recycled) = recycle_receiver.try_recv() {
                let recycled_matches =
                    recycled.signature.as_ref() == Some(&requested_buffer_signature);
                let current_matches =
                    reusable_pixels_signature.as_ref() == Some(&requested_buffer_signature);
                if reusable_pixels.is_empty()
                    || (recycled_matches && !current_matches)
                    || (recycled_matches == current_matches
                        && recycled.rgba.capacity() > reusable_pixels.capacity())
                {
                    reusable_pixels = recycled.rgba;
                    reusable_pixels_signature = recycled.signature;
                }
            }

            let key = request.key.clone();
            let result = ViewerApp::render_viewport_payload(
                &request,
                &mut reusable_pixels,
                &mut reusable_pixels_signature,
                &mut moment_caches,
                &mut sample_caches,
                &mut last_direct_viewports,
                cache_policy,
            );
            let should_warm_sample_cache = result.as_ref().is_ok_and(|rendered| {
                cache_policy.should_speculatively_warm_sample_cache(rendered)
            });
            let should_prefetch_velocity_cache = result.as_ref().is_ok_and(|rendered| {
                ViewerApp::should_prefetch_velocity_interaction_cache(
                    &request,
                    rendered,
                    cache_policy,
                )
            });
            if result_sender
                .send(AsyncRenderResult { key, result })
                .is_err()
            {
                break;
            }
            if should_warm_sample_cache || should_prefetch_velocity_cache {
                match ViewerApp::take_newest_render_request(&request_receiver) {
                    Ok(Some(newest_request)) => {
                        deferred_request = Some(newest_request);
                    }
                    Ok(None) => {
                        if should_prefetch_velocity_cache {
                            match ViewerApp::take_newest_render_request(&request_receiver) {
                                Ok(Some(newest_request)) => {
                                    deferred_request = Some(newest_request);
                                }
                                Ok(None) => {
                                    ViewerApp::warm_velocity_interaction_cache_after_direct_render(
                                        &request,
                                        &mut moment_caches,
                                        &mut sample_caches,
                                        cache_policy,
                                    );
                                }
                                Err(mpsc::TryRecvError::Disconnected) => break,
                                Err(mpsc::TryRecvError::Empty) => {
                                    unreachable!(
                                        "take_newest_render_request maps empty to Ok(None)"
                                    );
                                }
                            }
                        }
                        if deferred_request.is_none() && should_warm_sample_cache {
                            match ViewerApp::take_newest_render_request(&request_receiver) {
                                Ok(Some(newest_request)) => {
                                    deferred_request = Some(newest_request);
                                }
                                Ok(None) => {
                                    ViewerApp::warm_sample_cache_after_direct_render(
                                        &request,
                                        &mut moment_caches,
                                        &mut sample_caches,
                                        &mut last_direct_viewports,
                                        cache_policy,
                                    );
                                }
                                Err(mpsc::TryRecvError::Disconnected) => break,
                                Err(mpsc::TryRecvError::Empty) => {
                                    unreachable!(
                                        "take_newest_render_request maps empty to Ok(None)"
                                    );
                                }
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => break,
                    Err(mpsc::TryRecvError::Empty) => {
                        unreachable!("take_newest_render_request maps empty to Ok(None)");
                    }
                }
            }
        }
    });

    (request_sender, result_receiver, recycle_sender)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DisplayProduct {
    Moment(MomentType),
    StormRelativeVelocity,
}

impl DisplayProduct {
    fn label(&self) -> &str {
        match self {
            Self::Moment(moment) => moment.short_name(),
            Self::StormRelativeVelocity => "SRV",
        }
    }

    fn base_moment(&self) -> MomentType {
        match self {
            Self::Moment(moment) => moment.clone(),
            Self::StormRelativeVelocity => MomentType::Velocity,
        }
    }

    fn is_storm_relative_velocity(&self) -> bool {
        matches!(self, Self::StormRelativeVelocity)
    }
}

#[derive(Clone, Debug)]
struct CursorReadout {
    product: DisplayProduct,
    cut: usize,
    value: f32,
    base_value: Option<f32>,
    vrot: Option<VrotProbe>,
    raw: Option<u16>,
    row: usize,
    gate: usize,
    gate_spacing_m: i32,
    range_km: f32,
    azimuth_deg: f32,
    source_azimuth_deg: f32,
    elevation_deg: f32,
    nyquist_velocity_mps: Option<f32>,
}

#[derive(Clone, Copy, Debug)]
struct VrotProbe {
    delta_v_mps: f32,
    vrot_mps: f32,
    separation_km: f32,
    inbound: VrotGate,
    outbound: VrotGate,
}

#[derive(Clone, Copy, Debug)]
struct VrotGate {
    row: usize,
    gate: usize,
    value_mps: f32,
    azimuth_deg: f32,
}

impl ViewerApp {
    fn new(cc: &eframe::CreationContext<'_>, source_path: PathBuf) -> Self {
        configure_style(&cc.egui_ctx);
        let sites = data_source::fallback_sites();
        let selected_site_index = sites
            .iter()
            .position(|site| site.level2_id == "KTLX")
            .unwrap_or(0);
        let (map_center_lat, map_center_lon) = sites
            .get(selected_site_index)
            .and_then(site_location)
            .unwrap_or((35.33305, -97.27775));
        let (render_sender, render_receiver, render_recycle_sender) = spawn_render_worker();

        let mut app = Self {
            source_path,
            volume: None,
            selected_cut: 0,
            selected_product: DisplayProduct::Moment(MomentType::Reflectivity),
            texture: None,
            texture_key: None,
            render_sender,
            render_receiver,
            render_recycle_sender,
            pending_render_key: None,
            map_center_lon,
            map_center_lat,
            map_scale: DEFAULT_MAP_SCALE,
            radar_range_km: DEFAULT_RADAR_RANGE_KM,
            load_timing: None,
            render_ms: None,
            worker_ms: None,
            texture_ms: None,
            sample_cache_build_ms: None,
            basemap_ms: None,
            perf: PerfTelemetry::new(),
            status: String::new(),
            sites,
            selected_site_index,
            site_catalog_receiver: None,
            load_receiver: None,
            pending_site_id: None,
            cursor_readout: None,
            storm_motion_direction_deg: DEFAULT_STORM_MOTION_DIRECTION_DEG,
            storm_motion_speed_kt: DEFAULT_STORM_MOTION_SPEED_KT,
        };
        app.start_site_catalog_load(&cc.egui_ctx);
        app.load_volume(&cc.egui_ctx);
        app
    }

    fn load_volume(&mut self, ctx: &egui::Context) {
        self.start_local_volume_load(self.source_path.clone(), ctx);
    }

    fn clear_texture(&mut self) {
        self.texture = None;
        self.texture_key = None;
        self.pending_render_key = None;
        self.render_ms = None;
        self.worker_ms = None;
        self.texture_ms = None;
        self.sample_cache_build_ms = None;
    }

    fn install_volume(
        &mut self,
        volume: RadarVolume,
        load_timing: Option<LoadTimings>,
        record_final_decode: bool,
        ctx: &egui::Context,
    ) {
        let (selected_cut, selected_product) = selection_for_installed_volume(
            self.volume.as_deref(),
            self.selected_cut,
            &self.selected_product,
            &volume,
        );
        if let Some(index) = self
            .sites
            .iter()
            .position(|site| site.level2_id == volume.site.id)
        {
            self.selected_site_index = index;
        }
        if record_final_decode && let Some(load_timing) = load_timing {
            self.perf.record_decode(load_timing.decode_ms);
        }
        self.load_timing = load_timing;
        self.volume = Some(Arc::new(volume));
        self.selected_cut = selected_cut;
        self.selected_product = selected_product;
        self.sanitize_selection();
        self.clear_texture();
        self.center_loaded_volume();
        ctx.request_repaint();
    }

    fn start_site_catalog_load(&mut self, ctx: &egui::Context) {
        if self.site_catalog_receiver.is_some() {
            return;
        }

        let (sender, receiver) = mpsc::channel();
        self.site_catalog_receiver = Some(receiver);
        thread::spawn(move || {
            let result = data_source::fetch_level2_radar_sites(7)
                .map(|sites| {
                    if sites.is_empty() {
                        data_source::fallback_sites()
                    } else {
                        sites
                    }
                })
                .map_err(|err| err.to_string());
            let _ = sender.send(AsyncSiteCatalogResult { result });
        });
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn poll_async_site_catalog(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.site_catalog_receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(message) => {
                self.site_catalog_receiver = None;
                if let Ok(sites) = message.result {
                    self.install_site_catalog(sites);
                }
                ctx.request_repaint();
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.site_catalog_receiver = None;
            }
        }
    }

    fn install_site_catalog(&mut self, sites: Vec<RadarSite>) {
        if sites.is_empty() {
            return;
        }
        let current_site_id = self
            .volume
            .as_ref()
            .map(|volume| volume.site.id.clone())
            .or_else(|| self.selected_site().map(|site| site.level2_id.clone()));
        self.sites = sites;
        if let Some(site_id) = current_site_id
            && let Some(index) = self.sites.iter().position(|site| site.level2_id == site_id)
        {
            self.selected_site_index = index;
            return;
        }
        self.selected_site_index = self.selected_site_index.min(self.sites.len() - 1);
    }

    fn poll_async_load(&mut self, ctx: &egui::Context) {
        let mut saw_message = false;
        loop {
            let Some(result) = self.load_receiver.as_ref().map(mpsc::Receiver::try_recv) else {
                return;
            };
            match result {
                Ok(message) => {
                    saw_message = true;
                    match message.update {
                        AsyncLoadUpdate::Preview(decoded) => {
                            self.source_path = decoded.path;
                            self.install_volume(decoded.volume, Some(decoded.timings), false, ctx);
                            self.status = format!("Preview {}", message.label);
                        }
                        AsyncLoadUpdate::Final(result) => {
                            self.load_receiver = None;
                            self.pending_site_id = None;
                            match result {
                                Ok(decoded) => {
                                    self.source_path = decoded.path;
                                    self.install_volume(
                                        decoded.volume,
                                        Some(decoded.timings),
                                        true,
                                        ctx,
                                    );
                                    self.status = format!("Loaded {}", message.label);
                                }
                                Err(err) => {
                                    self.status =
                                        format!("Load failed for {}: {err}", message.label);
                                }
                            }
                            ctx.request_repaint();
                            return;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if saw_message {
                        ctx.request_repaint();
                    } else {
                        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
                    }
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.load_receiver = None;
                    self.pending_site_id = None;
                    self.status = "L2 load worker disconnected".to_owned();
                    return;
                }
            }
        }
    }

    fn sanitize_selection(&mut self) {
        let Some(volume) = &self.volume else {
            return;
        };
        if volume.cuts.is_empty() {
            self.selected_cut = 0;
            return;
        }
        self.selected_cut = self.selected_cut.min(volume.cuts.len() - 1);
        if is_displayable_on_cut(volume, self.selected_cut, &self.selected_product) {
            return;
        }
        let preferred = [
            DisplayProduct::Moment(MomentType::Reflectivity),
            DisplayProduct::Moment(MomentType::Velocity),
            DisplayProduct::StormRelativeVelocity,
            DisplayProduct::Moment(MomentType::SpectrumWidth),
            DisplayProduct::Moment(MomentType::DifferentialReflectivity),
            DisplayProduct::Moment(MomentType::CorrelationCoefficient),
            DisplayProduct::Moment(MomentType::DifferentialPhase),
        ];
        if let Some(product) = preferred
            .iter()
            .find(|product| is_displayable_on_cut(volume, self.selected_cut, product))
            .cloned()
        {
            self.selected_product = product;
        } else if let Some(product) = displayable_products(volume, self.selected_cut)
            .first()
            .cloned()
        {
            self.selected_product = product;
        }
    }

    fn poll_async_render(&mut self, ctx: &egui::Context) {
        let mut saw_message = false;
        loop {
            match self.render_receiver.try_recv() {
                Ok(message) => {
                    saw_message = true;
                    let is_latest = self.pending_render_key.as_ref() == Some(&message.key);
                    match message.result {
                        Ok(rendered) if is_latest => {
                            self.pending_render_key = None;
                            self.install_rendered_texture(ctx, message.key, rendered);
                        }
                        Ok(rendered) => {
                            self.recycle_render_buffer(
                                rendered.rgba,
                                Some(rendered.buffer_signature),
                            );
                        }
                        Err(err) if is_latest => {
                            self.pending_render_key = None;
                            self.texture = None;
                            self.texture_key = None;
                            self.render_ms = None;
                            self.worker_ms = None;
                            self.texture_ms = None;
                            self.sample_cache_build_ms = None;
                            self.status = format!("Render failed: {err}");
                        }
                        Err(_) => {}
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending_render_key = None;
                    self.status = "Render worker disconnected".to_owned();
                    saw_message = true;
                    break;
                }
            }
        }
        if saw_message {
            ctx.request_repaint();
        } else if self.pending_render_key.is_some() {
            ctx.request_repaint_after(Duration::from_millis(8));
        }
    }

    fn recycle_render_buffer(
        &self,
        rgba: Vec<u8>,
        signature: Option<RenderWorkerViewportSignature>,
    ) {
        let _ = self
            .render_recycle_sender
            .send(RenderRecycleBuffer { rgba, signature });
    }

    fn start_render_request(&mut self, request: RenderRequest, ctx: &egui::Context) {
        let key = request.key.clone();
        match self.render_sender.send(request) {
            Ok(()) => {
                self.pending_render_key = Some(key);
                if self.load_receiver.is_none() {
                    self.status = "Rendering".to_owned();
                }
                ctx.request_repaint_after(Duration::from_millis(8));
            }
            Err(_) => {
                self.pending_render_key = None;
                self.status = "Render worker disconnected".to_owned();
            }
        }
    }

    fn request_texture_render(&mut self, ctx: &egui::Context, rect: egui::Rect) {
        let Some(volume) = self.volume.clone() else {
            return;
        };
        let Some((viewport_options, viewport_key)) = self.viewport_raster_options(ctx, rect) else {
            return;
        };
        let key = TextureKey {
            cut: self.selected_cut,
            product: self.selected_product.clone(),
            storm_motion_key: self.storm_motion_key(),
            viewport: viewport_key,
        };
        if self.texture_key.as_ref() == Some(&key) {
            return;
        }
        if self.pending_render_key.as_ref() == Some(&key) {
            ctx.request_repaint_after(Duration::from_millis(8));
            return;
        }

        self.start_render_request(
            RenderRequest {
                key,
                volume,
                cut: self.selected_cut,
                product: self.selected_product.clone(),
                storm_motion: self.current_storm_motion(),
                viewport_options,
                radar_range_km: self
                    .selected_grid_range_km()
                    .unwrap_or(DEFAULT_RADAR_RANGE_KM),
            },
            ctx,
        );
    }

    fn take_newest_render_request(
        receiver: &mpsc::Receiver<RenderRequest>,
    ) -> std::result::Result<Option<RenderRequest>, mpsc::TryRecvError> {
        match receiver.try_recv() {
            Ok(newer_request) => {
                let mut newest_request = newer_request;
                for newer_request in receiver.try_iter() {
                    newest_request = newer_request;
                }
                Ok(Some(newest_request))
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(err @ mpsc::TryRecvError::Disconnected) => Err(err),
        }
    }

    fn render_viewport_payload(
        request: &RenderRequest,
        reusable_pixels: &mut Vec<u8>,
        reusable_pixels_signature: &mut Option<RenderWorkerViewportSignature>,
        moment_caches: &mut Vec<RenderWorkerMomentCache>,
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        last_direct_viewports: &mut Vec<RenderWorkerViewportSignature>,
        cache_policy: RenderWorkerCachePolicy,
    ) -> Result<RenderedTexture, String> {
        let worker_start = Instant::now();
        let required_len = viewport_rgba_buffer_len(request.viewport_options);
        if reusable_pixels.len() != required_len {
            reusable_pixels.resize(required_len, 0);
            *reusable_pixels_signature = None;
        }

        let volume_ptr = Arc::as_ptr(&request.volume) as usize;
        let base_moment = request.product.base_moment();
        let cached_volume_ptr = moment_caches.first().map(|cached| cached.volume_ptr);
        if cached_volume_ptr.is_some_and(|cached_volume_ptr| cached_volume_ptr != volume_ptr) {
            moment_caches.clear();
            sample_caches.clear();
            last_direct_viewports.clear();
        }
        if Self::touch_moment_cache(moment_caches, volume_ptr, request.cut, &base_moment).is_none()
        {
            Self::insert_moment_cache(
                moment_caches,
                cache_policy,
                RenderWorkerMomentCache {
                    volume_ptr,
                    cut: request.cut,
                    moment: base_moment.clone(),
                    cache: ViewportMomentCache::new(
                        request.volume.as_ref(),
                        request.cut,
                        base_moment.clone(),
                    )
                    .map_err(|err| err.to_string())?,
                    storm_palette_cache: None,
                },
            );
        }
        let moment_cache = moment_caches
            .last_mut()
            .expect("render cache is prepared before rendering");
        let cache = &moment_cache.cache;
        let viewport_signature = RenderWorkerViewportSignature::new(
            volume_ptr,
            request.cut,
            base_moment.clone(),
            request.key.viewport,
        );

        let start = Instant::now();
        let mut sample_cache_build_ms = None;
        let sample_cache_matches = Self::touch_sample_cache(sample_caches, &viewport_signature);
        if !sample_cache_matches
            && Self::has_direct_viewport(last_direct_viewports, &viewport_signature)
            && cache_policy.should_build_sample_cache_for_moment_cache(
                cache,
                request.volume.as_ref(),
                request.viewport_options,
            )?
        {
            let cache_build_start = Instant::now();
            let built_sample_cache = cache
                .build_sample_cache(request.volume.as_ref(), request.viewport_options)
                .map_err(|err| err.to_string())?;
            sample_cache_build_ms = Some(cache_build_start.elapsed().as_secs_f32() * 1000.0);
            Self::insert_sample_cache(
                sample_caches,
                cache_policy,
                viewport_signature.clone(),
                built_sample_cache,
            );
            Self::forget_direct_viewport(last_direct_viewports, &viewport_signature);
        }
        let matching_sample_cache = sample_caches
            .last()
            .filter(|cached| cached.signature == viewport_signature);
        let can_reuse_transparency = matching_sample_cache.is_some()
            && reusable_pixels_signature.as_ref() == Some(&viewport_signature);
        *reusable_pixels_signature = None;

        let (width, height, used_sample_cache) = if request.product.is_storm_relative_velocity() {
            let storm_motion_key = request.key.storm_motion_key;
            let palette_matches = moment_cache
                .storm_palette_cache
                .as_ref()
                .is_some_and(|cached| cached.storm_motion_key == storm_motion_key);
            if !palette_matches {
                moment_cache.storm_palette_cache = Some(RenderWorkerStormPaletteCache {
                    storm_motion_key,
                    cache: cache
                        .build_storm_relative_velocity_palette_cache(
                            request.volume.as_ref(),
                            request.storm_motion,
                        )
                        .map_err(|err| err.to_string())?,
                });
            }
            let palette_cache = moment_cache
                .storm_palette_cache
                .as_ref()
                .and_then(|cached| cached.cache.as_ref());
            if let Some(sample_cache) = matching_sample_cache {
                let dimensions = if can_reuse_transparency {
                    if let Some(palette_cache) = palette_cache {
                        cache.render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency_and_palette_cache(
                            request.volume.as_ref(),
                            request.storm_motion,
                            palette_cache,
                            &sample_cache.cache,
                            reusable_pixels,
                        )
                    } else {
                        cache
                            .render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency(
                                request.volume.as_ref(),
                                request.storm_motion,
                                &sample_cache.cache,
                                reusable_pixels,
                            )
                    }
                } else if let Some(palette_cache) = palette_cache {
                    cache.render_storm_relative_velocity_rgba_with_sample_cache_and_palette_cache(
                        request.volume.as_ref(),
                        request.storm_motion,
                        palette_cache,
                        &sample_cache.cache,
                        reusable_pixels,
                    )
                } else {
                    cache.render_storm_relative_velocity_rgba_with_sample_cache(
                        request.volume.as_ref(),
                        request.storm_motion,
                        &sample_cache.cache,
                        reusable_pixels,
                    )
                }
                .map_err(|err| err.to_string())?;
                (dimensions.0, dimensions.1, true)
            } else {
                let dimensions = if let Some(palette_cache) = palette_cache {
                    cache.render_storm_relative_velocity_rgba_into_with_palette_cache(
                        request.volume.as_ref(),
                        request.storm_motion,
                        palette_cache,
                        request.viewport_options,
                        reusable_pixels,
                    )
                } else {
                    cache.render_storm_relative_velocity_rgba_into(
                        request.volume.as_ref(),
                        request.storm_motion,
                        request.viewport_options,
                        reusable_pixels,
                    )
                }
                .map_err(|err| err.to_string())?;
                (dimensions.0, dimensions.1, false)
            }
        } else if let Some(sample_cache) = matching_sample_cache {
            let dimensions = if can_reuse_transparency {
                cache.render_moment_rgba_with_sample_cache_reusing_transparency(
                    request.volume.as_ref(),
                    &sample_cache.cache,
                    reusable_pixels,
                )
            } else {
                cache.render_moment_rgba_with_sample_cache(
                    request.volume.as_ref(),
                    &sample_cache.cache,
                    reusable_pixels,
                )
            }
            .map_err(|err| err.to_string())?;
            (dimensions.0, dimensions.1, true)
        } else {
            let dimensions = cache
                .render_moment_rgba_into(
                    request.volume.as_ref(),
                    request.viewport_options,
                    reusable_pixels,
                )
                .map_err(|err| err.to_string())?;
            (dimensions.0, dimensions.1, false)
        };
        let render_ms = start.elapsed().as_secs_f32() * 1000.0;
        if !used_sample_cache {
            Self::remember_direct_viewport(
                last_direct_viewports,
                cache_policy,
                viewport_signature.clone(),
            );
        }
        let rgba = std::mem::take(reusable_pixels);
        let worker_ms = worker_start.elapsed().as_secs_f32() * 1000.0;

        Ok(RenderedTexture {
            width: width as usize,
            height: height as usize,
            rgba,
            buffer_signature: viewport_signature,
            render_ms,
            worker_ms,
            sample_cache_build_ms,
            used_sample_cache,
            radar_range_km: request.radar_range_km,
        })
    }

    fn should_prefetch_velocity_interaction_cache(
        request: &RenderRequest,
        rendered: &RenderedTexture,
        cache_policy: RenderWorkerCachePolicy,
    ) -> bool {
        request.product.base_moment() != MomentType::Velocity
            && cache_policy
                .should_prefetch_interaction_cache(rendered.buffer_signature.viewport.dimensions())
            && Self::prefetch_velocity_cut(request).is_some()
    }

    fn prefetch_velocity_cut(request: &RenderRequest) -> Option<usize> {
        let product = DisplayProduct::Moment(MomentType::Velocity);
        if is_displayable_on_cut(request.volume.as_ref(), request.cut, &product) {
            Some(request.cut)
        } else {
            best_cut_for_product(request.volume.as_ref(), request.cut, &product)
        }
    }

    fn warm_sample_cache_after_direct_render(
        request: &RenderRequest,
        moment_caches: &mut Vec<RenderWorkerMomentCache>,
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        last_direct_viewports: &mut Vec<RenderWorkerViewportSignature>,
        cache_policy: RenderWorkerCachePolicy,
    ) {
        let signature = RenderWorkerViewportSignature::new(
            Arc::as_ptr(&request.volume) as usize,
            request.cut,
            request.product.base_moment(),
            request.key.viewport,
        );
        if Self::touch_sample_cache(sample_caches, &signature) {
            return;
        }
        let Some(moment_index) = Self::touch_moment_cache(
            moment_caches,
            signature.volume_ptr,
            signature.cut,
            &signature.moment,
        ) else {
            return;
        };
        let moment_cache = &moment_caches[moment_index];
        let Ok(should_build) = cache_policy.should_build_sample_cache_for_moment_cache(
            &moment_cache.cache,
            request.volume.as_ref(),
            request.viewport_options,
        ) else {
            return;
        };
        if !should_build {
            return;
        }
        let Ok(cache) = moment_cache
            .cache
            .build_sample_cache(request.volume.as_ref(), request.viewport_options)
        else {
            return;
        };
        Self::insert_sample_cache(sample_caches, cache_policy, signature.clone(), cache);
        Self::forget_direct_viewport(last_direct_viewports, &signature);
    }

    fn warm_velocity_interaction_cache_after_direct_render(
        request: &RenderRequest,
        moment_caches: &mut Vec<RenderWorkerMomentCache>,
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        cache_policy: RenderWorkerCachePolicy,
    ) {
        let Some(cut) = Self::prefetch_velocity_cut(request) else {
            return;
        };

        if request.product.base_moment() == MomentType::Velocity
            || !cache_policy.should_prefetch_interaction_cache(request.key.viewport.dimensions())
        {
            return;
        }

        let volume_ptr = Arc::as_ptr(&request.volume) as usize;
        let signature = RenderWorkerViewportSignature::new(
            volume_ptr,
            cut,
            MomentType::Velocity,
            request.key.viewport,
        );

        if Self::touch_moment_cache(moment_caches, volume_ptr, cut, &MomentType::Velocity).is_none()
        {
            let Ok(cache) =
                ViewportMomentCache::new(request.volume.as_ref(), cut, MomentType::Velocity)
            else {
                return;
            };
            Self::insert_moment_cache(
                moment_caches,
                cache_policy,
                RenderWorkerMomentCache {
                    volume_ptr,
                    cut,
                    moment: MomentType::Velocity,
                    cache,
                    storm_palette_cache: None,
                },
            );
        }

        if !Self::touch_sample_cache(sample_caches, &signature)
            && let Some(moment_cache) = moment_caches.last()
            && let Ok(true) = cache_policy.should_build_sample_cache_for_moment_cache(
                &moment_cache.cache,
                request.volume.as_ref(),
                request.viewport_options,
            )
            && let Ok(cache) = moment_cache
                .cache
                .build_sample_cache(request.volume.as_ref(), request.viewport_options)
        {
            Self::insert_sample_cache(sample_caches, cache_policy, signature, cache);
        }

        let Some(moment_index) =
            Self::touch_moment_cache(moment_caches, volume_ptr, cut, &MomentType::Velocity)
        else {
            return;
        };
        let moment_cache = &mut moment_caches[moment_index];
        let storm_motion_key = request.key.storm_motion_key;
        let palette_matches = moment_cache
            .storm_palette_cache
            .as_ref()
            .is_some_and(|cached| cached.storm_motion_key == storm_motion_key);
        if palette_matches {
            return;
        }
        if let Ok(cache) = moment_cache
            .cache
            .build_storm_relative_velocity_palette_cache(
                request.volume.as_ref(),
                request.storm_motion,
            )
        {
            moment_cache.storm_palette_cache = Some(RenderWorkerStormPaletteCache {
                storm_motion_key,
                cache,
            });
        }
    }

    fn touch_moment_cache(
        moment_caches: &mut Vec<RenderWorkerMomentCache>,
        volume_ptr: usize,
        cut: usize,
        moment: &MomentType,
    ) -> Option<usize> {
        let index = moment_caches.iter().position(|cached| {
            cached.volume_ptr == volume_ptr && cached.cut == cut && cached.moment == *moment
        })?;
        let cached = moment_caches.remove(index);
        moment_caches.push(cached);
        Some(moment_caches.len() - 1)
    }

    fn insert_moment_cache(
        moment_caches: &mut Vec<RenderWorkerMomentCache>,
        cache_policy: RenderWorkerCachePolicy,
        cache: RenderWorkerMomentCache,
    ) {
        moment_caches.retain(|cached| {
            cached.volume_ptr != cache.volume_ptr
                || cached.cut != cache.cut
                || cached.moment != cache.moment
        });
        moment_caches.push(cache);
        while moment_caches.len() > cache_policy.moment_cache_capacity() {
            moment_caches.remove(0);
        }
    }

    fn touch_sample_cache(
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        signature: &RenderWorkerViewportSignature,
    ) -> bool {
        let Some(index) = sample_caches
            .iter()
            .position(|cached| &cached.signature == signature)
        else {
            return false;
        };
        let cached = sample_caches.remove(index);
        sample_caches.push(cached);
        true
    }

    fn insert_sample_cache(
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        cache_policy: RenderWorkerCachePolicy,
        signature: RenderWorkerViewportSignature,
        cache: ViewportSampleCache,
    ) {
        sample_caches.retain(|cached| cached.signature != signature);
        sample_caches.push(RenderWorkerSampleCache { signature, cache });
        Self::trim_sample_caches(sample_caches, cache_policy);
    }

    fn trim_sample_caches(
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        cache_policy: RenderWorkerCachePolicy,
    ) {
        let capacity = cache_policy.sample_cache_capacity();
        let byte_budget = cache_policy.sample_cache_bytes();
        while sample_caches.len() > capacity
            || Self::sample_cache_storage_bytes(sample_caches) > byte_budget
        {
            if sample_caches.is_empty() {
                break;
            }
            sample_caches.remove(0);
        }
    }

    fn sample_cache_storage_bytes(sample_caches: &[RenderWorkerSampleCache]) -> usize {
        sample_caches
            .iter()
            .map(|cached| cached.cache.storage_bytes())
            .sum()
    }

    fn has_direct_viewport(
        last_direct_viewports: &[RenderWorkerViewportSignature],
        signature: &RenderWorkerViewportSignature,
    ) -> bool {
        last_direct_viewports
            .iter()
            .any(|last_direct| last_direct == signature)
    }

    fn remember_direct_viewport(
        last_direct_viewports: &mut Vec<RenderWorkerViewportSignature>,
        cache_policy: RenderWorkerCachePolicy,
        signature: RenderWorkerViewportSignature,
    ) {
        Self::forget_direct_viewport(last_direct_viewports, &signature);
        last_direct_viewports.push(signature);
        let capacity = cache_policy.direct_viewport_capacity();
        while last_direct_viewports.len() > capacity {
            last_direct_viewports.remove(0);
        }
    }

    fn forget_direct_viewport(
        last_direct_viewports: &mut Vec<RenderWorkerViewportSignature>,
        signature: &RenderWorkerViewportSignature,
    ) {
        last_direct_viewports.retain(|last_direct| last_direct != signature);
    }

    fn install_rendered_texture(
        &mut self,
        ctx: &egui::Context,
        key: TextureKey,
        rendered: RenderedTexture,
    ) {
        let RenderedTexture {
            width,
            height,
            rgba,
            buffer_signature,
            render_ms,
            worker_ms,
            sample_cache_build_ms,
            used_sample_cache,
            radar_range_km,
        } = rendered;
        let texture_start = Instant::now();
        let color_image = radar_color_image_from_rgba([width, height], &rgba);
        let can_update_texture = self
            .texture_key
            .as_ref()
            .is_some_and(|old_key| old_key.viewport.dimensions() == key.viewport.dimensions());
        if can_update_texture && let Some(texture) = &mut self.texture {
            texture.set(color_image, egui::TextureOptions::NEAREST);
        } else {
            self.texture = Some(ctx.load_texture(
                format!(
                    "radar-{}-{}-{}x{}",
                    key.cut,
                    key.product.label(),
                    key.viewport.width,
                    key.viewport.height
                ),
                color_image,
                egui::TextureOptions::NEAREST,
            ));
        }
        let texture_ms = texture_start.elapsed().as_secs_f32() * 1000.0;
        self.texture_key = Some(key);
        self.perf.record_render(
            render_ms,
            used_sample_cache,
            worker_ms,
            texture_ms,
            sample_cache_build_ms,
        );
        self.render_ms = Some(render_ms);
        self.worker_ms = Some(worker_ms);
        self.texture_ms = Some(texture_ms);
        self.sample_cache_build_ms = sample_cache_build_ms;
        self.radar_range_km = radar_range_km;
        self.recycle_render_buffer(rgba, Some(buffer_signature));
        if self.load_receiver.is_none() {
            self.status = "Rendered".to_owned();
        }
    }

    fn viewport_raster_options(
        &self,
        ctx: &egui::Context,
        rect: egui::Rect,
    ) -> Option<(ViewportRasterOptions, ViewportKey)> {
        let (radar_lat, radar_lon) = self.radar_location()?;
        let pixels_per_point = ctx.pixels_per_point().max(1.0);
        let width = (rect.width() * pixels_per_point).round().max(1.0) as u32;
        let height = (rect.height() * pixels_per_point).round().max(1.0) as u32;
        let radar_position = self.lon_lat_to_screen(rect, radar_lon, radar_lat);
        let radar_x_px = (radar_position.x - rect.left()) * pixels_per_point;
        let radar_y_px = (radar_position.y - rect.top()) * pixels_per_point;
        let km_per_px_y = 111.32 / (self.map_scale * pixels_per_point);
        let km_per_px_x = 111.32 * radar_lat.to_radians().cos().abs().max(0.02)
            / (self.map_scale * pixels_per_point);
        let options = ViewportRasterOptions {
            width,
            height,
            radar_x_px,
            radar_y_px,
            km_per_px_x,
            km_per_px_y,
        };
        let key = ViewportKey {
            width,
            height,
            radar_x_px: (radar_x_px * 8.0).round() as i32,
            radar_y_px: (radar_y_px * 8.0).round() as i32,
            km_per_px_x: (km_per_px_x * 1_000_000.0).round() as i32,
            km_per_px_y: (km_per_px_y * 1_000_000.0).round() as i32,
        };
        Some((options, key))
    }

    fn reset_view(&mut self) {
        self.map_scale = DEFAULT_MAP_SCALE;
        self.center_selected_site();
    }

    fn selected_site(&self) -> Option<&RadarSite> {
        self.sites.get(self.selected_site_index)
    }

    fn selected_site_location(&self) -> Option<(f32, f32)> {
        self.selected_site().and_then(site_location)
    }

    fn radar_location(&self) -> Option<(f32, f32)> {
        self.loaded_volume_location()
            .or_else(|| self.selected_site_location())
    }

    fn center_selected_site(&mut self) {
        if let Some((latitude_deg, longitude_deg)) = self.selected_site_location() {
            self.center_map_on(latitude_deg, longitude_deg);
        }
    }

    fn center_loaded_volume(&mut self) {
        if let Some((latitude_deg, longitude_deg)) = self.loaded_volume_location() {
            self.center_map_on(latitude_deg, longitude_deg);
        } else {
            self.center_selected_site();
        }
    }

    fn center_map_on(&mut self, latitude_deg: f32, longitude_deg: f32) {
        if latitude_deg.is_finite() && longitude_deg.is_finite() {
            self.map_center_lat = latitude_deg.clamp(-85.0, 85.0);
            self.map_center_lon = normalize_lon(longitude_deg);
        }
    }

    fn loaded_volume_location(&self) -> Option<(f32, f32)> {
        let site = &self.volume.as_ref()?.site;
        Some((site.latitude_deg?, site.longitude_deg?))
    }

    fn selected_grid_range_km(&self) -> Option<f32> {
        let volume = self.volume.as_ref()?;
        let cut = volume.cuts.get(self.selected_cut)?;
        let grid = cut.moments.get(&self.selected_product.base_moment())?;
        grid_range_km(grid)
    }

    fn current_storm_motion(&self) -> StormMotion {
        StormMotion {
            direction_deg: self.storm_motion_direction_deg.rem_euclid(360.0),
            speed_mps: self.storm_motion_speed_kt.max(0.0) * KNOT_TO_MPS,
        }
    }

    fn storm_motion_key(&self) -> (i16, i16) {
        (
            (self.storm_motion_direction_deg.rem_euclid(360.0) * 10.0).round() as i16,
            (self.storm_motion_speed_kt.max(0.0) * 10.0).round() as i16,
        )
    }

    fn start_local_volume_load(&mut self, path: PathBuf, ctx: &egui::Context) {
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("local L2")
            .to_owned();
        let (sender, receiver) = mpsc::channel();
        self.load_receiver = Some(receiver);
        self.pending_site_id = Some(label.clone());
        self.status = format!("Loading {label}");

        thread::spawn(move || {
            let total_start = Instant::now();
            let result = decode_load_path_with_optional_preview(
                path,
                &label,
                total_start,
                LoadTimings::default(),
                &sender,
                should_preview_loads(),
            );
            let _ = sender.send(AsyncLoadResult {
                label,
                update: AsyncLoadUpdate::Final(result),
            });
        });
        ctx.request_repaint_after(Duration::from_millis(8));
    }

    fn load_latest_level2_for_selected_site(&mut self) {
        let Some(site) = self.selected_site().cloned() else {
            self.status = "No site selected".to_owned();
            return;
        };

        self.start_latest_level2_load(site);
    }

    fn start_latest_level2_load(&mut self, site: RadarSite) {
        let site_id = site.level2_id.clone();
        let (sender, receiver) = mpsc::channel();
        self.load_receiver = Some(receiver);
        self.pending_site_id = Some(site_id.clone());
        self.status = format!("Loading latest L2 {site_id}");

        thread::spawn(move || {
            let total_start = Instant::now();
            let mut timings = LoadTimings::default();
            let result = (|| {
                let lookup_start = Instant::now();
                let latest = data_source::latest_level2_object_cached(
                    &site.level2_id,
                    7,
                    LATEST_OBJECT_CACHE_TTL,
                )
                .map_err(|err| err.to_string())?;
                timings.lookup_ms = Some(lookup_start.elapsed().as_secs_f32() * 1000.0);
                timings.lookup_cache_hit = Some(latest.cache_hit);

                let fetch_start = Instant::now();
                let downloaded = data_source::download_object(
                    LEVEL2_ARCHIVE_BUCKET,
                    latest.object,
                    &cache_dir(&site.level2_id),
                )
                .map_err(|err| err.to_string())?;
                timings.fetch_ms = Some(fetch_start.elapsed().as_secs_f32() * 1000.0);
                timings.fetch_cache_hit = Some(downloaded.cache_hit);

                decode_load_path_with_optional_preview(
                    downloaded.path,
                    &format!("L2 {site_id}"),
                    total_start,
                    timings,
                    &sender,
                    should_preview_loads(),
                )
            })();
            let _ = sender.send(AsyncLoadResult {
                label: format!("L2 {site_id}"),
                update: AsyncLoadUpdate::Final(result),
            });
        });
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_async_site_catalog(&ctx);
        self.poll_async_load(&ctx);
        self.poll_async_render(&ctx);
        self.sanitize_selection();

        egui::Panel::top("top_bar")
            .exact_size(42.0)
            .show_inside(ui, |ui| self.top_bar(ui));

        egui::Panel::right("product_tilt_panel")
            .resizable(false)
            .exact_size(260.0)
            .show_inside(ui, |ui| self.side_panel(ui, &ctx));

        egui::Panel::bottom("status_bar")
            .exact_size(30.0)
            .show_inside(ui, |ui| self.status_bar(ui));

        egui::CentralPanel::default().show_inside(ui, |ui| self.map_canvas(ui));
    }
}

impl ViewerApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.heading("Radar RS Analyst");
            ui.separator();
            if ui.button("Reset View").clicked() {
                self.reset_view();
            }
            if ui.button("Reload").clicked() {
                self.load_volume(ui.ctx());
            }
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Radar");
        ui.add_space(8.0);

        ui.label("Level 2");
        ui.add_space(8.0);
        ui.label("Site");
        let selected_site_label = self
            .selected_site()
            .map(format_site_label)
            .unwrap_or_else(|| "None".to_owned());
        let mut selected_site_index = self.selected_site_index;
        egui::ComboBox::from_id_salt("site_combo")
            .selected_text(selected_site_label)
            .width(220.0)
            .show_ui(ui, |ui| {
                for (index, site) in self.sites.iter().enumerate() {
                    ui.selectable_value(&mut selected_site_index, index, format_site_label(site));
                }
            });
        if selected_site_index != self.selected_site_index {
            self.selected_site_index = selected_site_index;
            self.center_selected_site();
        }

        ui.horizontal(|ui| {
            if ui.button("Load Selected").clicked() {
                self.load_latest_level2_for_selected_site();
            }
            if ui.button("Center").clicked() {
                self.center_selected_site();
            }
        });

        ui.add_space(12.0);
        let Some(volume) = &self.volume else {
            ui.label(&self.status);
            return;
        };

        let site = volume.site.id.clone();
        let volume_time = volume
            .volume_time
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string();
        let vcp = volume
            .vcp
            .as_ref()
            .map(|vcp| vcp.pattern.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let cut_count = volume.cuts.len();
        let decoded_radials = volume.metadata.decoded_radial_count;
        let product_buttons = global_displayable_products(volume)
            .into_iter()
            .map(|product| {
                let target_cut = if is_displayable_on_cut(volume, self.selected_cut, &product) {
                    Some(self.selected_cut)
                } else {
                    best_cut_for_product(volume, self.selected_cut, &product)
                };
                (product, target_cut)
            })
            .collect::<Vec<_>>();
        let cut_rows = volume
            .cuts
            .iter()
            .enumerate()
            .map(|(index, cut)| {
                (
                    index,
                    cut.elevation_deg,
                    cut.radials.len(),
                    index == self.selected_cut,
                    is_displayable_on_cut(volume, index, &self.selected_product),
                )
            })
            .collect::<Vec<_>>();

        ui.label("Level 2 Volume");
        ui.label(format!("Site {site}"));
        ui.label(volume_time);
        ui.label(format!("VCP {vcp}"));
        ui.label(format!("{cut_count} cuts, {decoded_radials} radials"));

        ui.add_space(12.0);
        ui.label("Product");
        ui.horizontal_wrapped(|ui| {
            for (product, target_cut) in &product_buttons {
                let selected = self.selected_product == *product;
                let response = ui.selectable_label(selected, product.label());
                if response.clicked() {
                    self.selected_product = product.clone();
                    if let Some(cut_index) = target_cut {
                        self.selected_cut = *cut_index;
                    }
                    self.clear_texture();
                    ctx.request_repaint();
                }
            }
        });

        if self.selected_product.is_storm_relative_velocity() {
            ui.add_space(8.0);
            ui.label("Storm Motion");
            let direction_changed = ui
                .add(
                    egui::DragValue::new(&mut self.storm_motion_direction_deg)
                        .range(0.0..=359.0)
                        .speed(1.0)
                        .suffix(" deg"),
                )
                .changed();
            let speed_changed = ui
                .add(
                    egui::DragValue::new(&mut self.storm_motion_speed_kt)
                        .range(0.0..=120.0)
                        .speed(1.0)
                        .suffix(" kt"),
                )
                .changed();
            if direction_changed || speed_changed {
                self.storm_motion_direction_deg = self.storm_motion_direction_deg.rem_euclid(360.0);
                self.clear_texture();
                ctx.request_repaint();
            }
        }

        ui.add_space(12.0);
        self.timing_readout(ui);

        ui.add_space(12.0);
        ui.label("Tilt");
        egui::ScrollArea::vertical()
            .id_salt("tilt_list")
            .max_height(390.0)
            .show(ui, |ui| {
                for (index, elevation_deg, radial_count, is_selected, has_selected_product) in
                    &cut_rows
                {
                    let label = format!(
                        "#{:02}  {:>4.2} deg  {:>4} radials",
                        index, elevation_deg, radial_count
                    );
                    let response = ui.add_enabled(
                        *has_selected_product,
                        egui::Button::selectable(*is_selected, label),
                    );
                    if response.clicked() {
                        self.selected_cut = *index;
                        self.sanitize_selection();
                        self.clear_texture();
                        ctx.request_repaint();
                    }
                }
            });
    }

    fn timing_readout(&self, ui: &mut egui::Ui) {
        if let Some(timing) = self.load_timing {
            ui.label(format!("Decode {:.1} ms", timing.decode_ms));
            ui.label(format!("Load {:.1} ms", timing.total_ms));
            if let Some(lookup_ms) = timing.lookup_ms {
                let source = if timing.lookup_cache_hit == Some(true) {
                    "cache"
                } else {
                    "net"
                };
                ui.label(format!("Lookup {:.1} ms {source}", lookup_ms));
            }
            if let Some(fetch_ms) = timing.fetch_ms {
                let source = if timing.fetch_cache_hit == Some(true) {
                    "cache"
                } else {
                    "net"
                };
                ui.label(format!("Fetch {:.1} ms {source}", fetch_ms));
            }
            if let Some(read_ms) = timing.read_ms {
                ui.label(format!("Read {:.1} ms", read_ms));
            }
            if let Some(preview_ms) = timing.preview_ms {
                ui.label(format!("Preview {:.1} ms", preview_ms));
            }
        }
        if let Some(render_ms) = self.render_ms {
            ui.label(format!("Render {:.1} ms", render_ms));
        }
        if let Some(worker_ms) = self.worker_ms {
            ui.label(format!("Worker {:.1} ms", worker_ms));
        }
        if let Some(texture_ms) = self.texture_ms {
            ui.label(format!("Texture {:.1} ms", texture_ms));
        }
        if let Some(sample_cache_build_ms) = self.sample_cache_build_ms {
            ui.label(format!("Cache {:.1} ms", sample_cache_build_ms));
        }
        if let Some(basemap_ms) = self.basemap_ms {
            ui.label(format!("Map {:.1} ms", basemap_ms));
        }

        ui.add_space(6.0);
        self.perf_metric_readout(ui, "Decode", &self.perf.decode);
        self.perf_metric_readout(ui, "Direct", &self.perf.direct_render);
        self.perf_metric_readout(ui, "Cached", &self.perf.cached_render);
        self.perf_metric_readout(ui, "Worker", &self.perf.worker);
        self.perf_metric_readout(ui, "Texture", &self.perf.texture);
        self.perf_metric_readout(ui, "Cache build", &self.perf.cache_build);
    }

    fn perf_metric_readout(&self, ui: &mut egui::Ui, label: &str, series: &MetricSeries) {
        if let Some(summary) = series.summary() {
            ui.label(format!(
                "{} {:.1} p50 {:.1} p95 {:.1} max {:.1} n{}",
                label, summary.latest, summary.p50, summary.p95, summary.max, summary.count
            ));
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(&self.status);
            ui.separator();
            if let Some(readout) = &self.cursor_readout {
                ui.label(format_cursor_readout(readout));
            } else {
                ui.label(format!(
                    "{} cut {}",
                    self.selected_product.label(),
                    self.selected_cut
                ));
            }
            ui.separator();
            ui.label(format!("map {:.0} px/deg", self.map_scale));
            ui.separator();
            ui.label(format!("{:.0} km range", self.radar_range_km));
            ui.separator();
            ui.label(self.source_path.display().to_string());
        });
    }

    fn map_canvas(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(7, 10, 14));

        if response.dragged() {
            let delta = response.drag_delta();
            self.map_center_lon -= delta.x / self.map_scale;
            self.map_center_lat += delta.y / self.map_scale;
            self.clamp_map_center();
        }

        if response.hovered() {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let pointer = ui.input(|input| input.pointer.hover_pos());
                let before = pointer.map(|position| self.screen_to_lon_lat(rect, position));
                let factor = (1.0_f32 + scroll / 600.0).clamp(0.75, 1.35);
                self.map_scale = (self.map_scale * factor).clamp(MIN_MAP_SCALE, MAX_MAP_SCALE);
                if let (Some(position), Some((lon_before, lat_before))) = (pointer, before) {
                    let (lon_after, lat_after) = self.screen_to_lon_lat(rect, position);
                    self.map_center_lon += lon_before - lon_after;
                    self.map_center_lat += lat_before - lat_after;
                }
                self.clamp_map_center();
            }
        }
        self.cursor_readout = response
            .hovered()
            .then(|| ui.input(|input| input.pointer.hover_pos()))
            .flatten()
            .and_then(|position| self.cursor_readout_at(rect, position));

        let basemap_start = Instant::now();
        self.draw_basemap(&painter, rect);
        self.draw_graticule(&painter, rect);
        let underlay_ms = basemap_start.elapsed().as_secs_f32() * 1000.0;
        self.request_texture_render(ui.ctx(), rect);
        self.draw_radar_layer(&painter, rect);
        let overlay_start = Instant::now();
        self.draw_basemap_overlay(&painter, rect);
        self.basemap_ms = Some(underlay_ms + overlay_start.elapsed().as_secs_f32() * 1000.0);

        let site_points = self
            .sites
            .iter()
            .enumerate()
            .filter_map(|(index, site)| {
                let (latitude_deg, longitude_deg) = site_location(site)?;
                let position = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
                rect.expand(18.0)
                    .contains(position)
                    .then_some((index, position))
            })
            .collect::<Vec<_>>();

        if response.clicked()
            && let Some(pointer) = response.interact_pointer_pos()
            && let Some((index, _)) = site_points
                .iter()
                .filter_map(|(index, position)| {
                    let distance = position.distance(pointer);
                    (distance <= 12.0).then_some((*index, distance))
                })
                .min_by(|left, right| left.1.total_cmp(&right.1))
        {
            self.selected_site_index = index;
            self.center_selected_site();
        }

        if response.secondary_clicked()
            && let Some(pointer) = response.interact_pointer_pos()
            && let Some(index) = self.nearest_site_to_position(rect, pointer)
            && let Some(site) = self.sites.get(index).cloned()
        {
            self.selected_site_index = index;
            self.center_selected_site();
            self.start_latest_level2_load(site);
        }

        self.draw_site_markers(&painter, &site_points);
        self.draw_loaded_volume_marker(&painter, rect);

        if self.texture.is_none() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                &self.status,
                egui::FontId::proportional(18.0),
                egui::Color32::from_rgb(210, 218, 230),
            );
        }
    }

    fn draw_radar_layer(&self, painter: &egui::Painter, rect: egui::Rect) {
        let Some(texture) = &self.texture else {
            return;
        };
        let Some((latitude_deg, longitude_deg)) = self.radar_location() else {
            return;
        };

        painter.image(
            texture.id(),
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        self.draw_range_ring(painter, rect, latitude_deg, longitude_deg);
    }

    fn draw_basemap(&self, painter: &egui::Painter, rect: egui::Rect) {
        let bounds = self.visible_geo_bounds(rect).expand(0.25);
        let us_detail_visible = us_detail_visible(bounds);
        self.draw_basemap_lines(
            painter,
            rect,
            bounds,
            basemap_data::BASEMAP_WORLD_COUNTRY_LINES,
            egui::Stroke::new(0.75, egui::Color32::from_rgb(31, 45, 57)),
        );

        if us_detail_visible && self.map_scale >= 38.0 {
            self.draw_basemap_lines(
                painter,
                rect,
                bounds,
                basemap_data::BASEMAP_US_COUNTY_LINES,
                egui::Stroke::new(0.65, egui::Color32::from_rgb(24, 35, 46)),
            );
        }
        if us_detail_visible {
            self.draw_basemap_lines(
                painter,
                rect,
                bounds,
                basemap_data::BASEMAP_US_STATE_LINES,
                egui::Stroke::new(1.05, egui::Color32::from_rgb(41, 58, 73)),
            );
        }

        if self.map_scale >= 36.0 {
            for layer in REGIONAL_BASEMAP_LAYERS {
                if bounds.intersects_bbox(layer.bounds) {
                    self.draw_basemap_lines(
                        painter,
                        rect,
                        bounds,
                        layer.admin_lines,
                        egui::Stroke::new(0.85, egui::Color32::from_rgb(36, 52, 65)),
                    );
                }
            }
        }
    }

    fn draw_basemap_overlay(&self, painter: &egui::Painter, rect: egui::Rect) {
        let bounds = self.visible_geo_bounds(rect).expand(0.15);
        let us_detail_visible = us_detail_visible(bounds);
        if self.map_scale >= 18.0 {
            self.draw_basemap_lines(
                painter,
                rect,
                bounds,
                basemap_data::BASEMAP_WORLD_COUNTRY_LINES,
                egui::Stroke::new(
                    0.85,
                    egui::Color32::from_rgba_unmultiplied(102, 126, 145, 84),
                ),
            );
        }

        if us_detail_visible && self.map_scale >= 76.0 {
            self.draw_basemap_lines(
                painter,
                rect,
                bounds,
                basemap_data::BASEMAP_US_COUNTY_LINES,
                egui::Stroke::new(
                    0.55,
                    egui::Color32::from_rgba_unmultiplied(92, 112, 128, 92),
                ),
            );
        }
        if us_detail_visible {
            self.draw_basemap_lines(
                painter,
                rect,
                bounds,
                basemap_data::BASEMAP_US_STATE_LINES,
                egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(126, 150, 170, 116),
                ),
            );
        }

        if self.map_scale >= 74.0 {
            for layer in REGIONAL_BASEMAP_LAYERS {
                if bounds.intersects_bbox(layer.bounds) {
                    self.draw_basemap_lines(
                        painter,
                        rect,
                        bounds,
                        layer.admin_lines,
                        egui::Stroke::new(
                            0.75,
                            egui::Color32::from_rgba_unmultiplied(112, 136, 154, 96),
                        ),
                    );
                }
            }
        }

        let mut occupied = Vec::with_capacity(128);
        self.draw_world_place_labels(painter, rect, bounds, &mut occupied);
        self.draw_regional_place_labels(painter, rect, bounds, &mut occupied);
        self.draw_admin_labels(painter, rect, bounds, &mut occupied);
    }

    fn draw_basemap_lines(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        lines: &[basemap_data::BasemapLine],
        stroke: egui::Stroke,
    ) {
        for line in lines {
            if bounds.intersects_bbox(line.bbox) {
                self.draw_geo_line(painter, rect, line.points, stroke);
            }
        }
    }

    fn draw_geo_line(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        coordinates: &[(f32, f32)],
        stroke: egui::Stroke,
    ) {
        if coordinates.len() < 2 {
            return;
        }
        let points = coordinates
            .iter()
            .map(|(longitude_deg, latitude_deg)| {
                self.lon_lat_to_screen(rect, *longitude_deg, *latitude_deg)
            })
            .collect::<Vec<_>>();
        painter.add(egui::Shape::line(points, stroke));
    }

    fn draw_world_place_labels(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let Some(max_rank) = world_place_label_rank(self.map_scale) else {
            return;
        };
        self.draw_place_label_set(
            painter,
            rect,
            bounds,
            PlaceLabelSet {
                labels: basemap_data::BASEMAP_WORLD_PLACE_LABELS,
                max_rank,
                max_labels: world_label_budget(self.map_scale),
            },
            occupied,
        );
    }

    fn draw_regional_place_labels(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let Some(max_rank) = place_label_rank(self.map_scale) else {
            return;
        };
        let max_labels = label_budget(self.map_scale);
        if us_detail_visible(bounds) {
            self.draw_place_label_set(
                painter,
                rect,
                bounds,
                PlaceLabelSet {
                    labels: basemap_data::BASEMAP_US_PLACE_LABELS,
                    max_rank,
                    max_labels,
                },
                occupied,
            );
        }
        for layer in REGIONAL_BASEMAP_LAYERS {
            if bounds.intersects_bbox(layer.bounds) {
                self.draw_place_label_set(
                    painter,
                    rect,
                    bounds,
                    PlaceLabelSet {
                        labels: layer.place_labels,
                        max_rank,
                        max_labels,
                    },
                    occupied,
                );
            }
        }
    }

    fn draw_place_label_set(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        place_labels: PlaceLabelSet,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let font = egui::FontId::proportional(if self.map_scale >= 190.0 { 12.0 } else { 11.0 });
        let text_color = egui::Color32::from_rgb(198, 207, 214);
        let halo_color = egui::Color32::from_rgba_unmultiplied(3, 5, 8, 210);
        let dot_color = egui::Color32::from_rgb(118, 143, 158);
        let mut drawn = 0usize;

        for label in place_labels.labels {
            if label.rank > place_labels.max_rank || !bounds.contains(label.lon, label.lat) {
                continue;
            }
            let position = self.lon_lat_to_screen(rect, label.lon, label.lat);
            if !rect.expand(32.0).contains(position) {
                continue;
            }
            let text_position = egui::pos2(position.x + 4.0, position.y - 1.0);
            let label_rect = left_label_rect(text_position, label.name, font.size).expand(2.0);
            if !rect.expand(80.0).intersects(label_rect) || overlaps_any(occupied, label_rect) {
                continue;
            }
            painter.circle_filled(position, 1.5, dot_color);
            draw_halo_text(
                painter,
                text_position,
                egui::Align2::LEFT_CENTER,
                label.name,
                font.clone(),
                text_color,
                halo_color,
            );
            occupied.push(label_rect);
            drawn += 1;
            if drawn >= place_labels.max_labels {
                break;
            }
        }
    }

    fn draw_admin_labels(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        occupied: &mut Vec<egui::Rect>,
    ) {
        if self.map_scale < 118.0 {
            return;
        }
        let max_labels = if self.map_scale >= 220.0 { 72 } else { 36 };
        if us_detail_visible(bounds) {
            self.draw_admin_label_set(
                painter,
                rect,
                bounds,
                basemap_data::BASEMAP_US_COUNTY_LABELS,
                max_labels,
                occupied,
            );
        }
        for layer in REGIONAL_BASEMAP_LAYERS {
            if bounds.intersects_bbox(layer.bounds) {
                self.draw_admin_label_set(
                    painter,
                    rect,
                    bounds,
                    layer.admin_labels,
                    max_labels,
                    occupied,
                );
            }
        }
    }

    fn draw_admin_label_set(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        labels: &[basemap_data::BasemapLabel],
        max_labels: usize,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let font = egui::FontId::proportional(10.0);
        let text_color = egui::Color32::from_rgba_unmultiplied(150, 164, 176, 184);
        let halo_color = egui::Color32::from_rgba_unmultiplied(2, 4, 7, 180);
        let mut drawn = 0usize;

        for label in labels {
            if !bounds.contains(label.lon, label.lat) {
                continue;
            }
            let position = self.lon_lat_to_screen(rect, label.lon, label.lat);
            if !rect.expand(24.0).contains(position) {
                continue;
            }
            let label_rect = centered_label_rect(position, label.name, font.size).expand(5.0);
            if !rect.expand(80.0).intersects(label_rect) || overlaps_any(occupied, label_rect) {
                continue;
            }
            draw_halo_text(
                painter,
                position,
                egui::Align2::CENTER_CENTER,
                label.name,
                font.clone(),
                text_color,
                halo_color,
            );
            occupied.push(label_rect);
            drawn += 1;
            if drawn >= max_labels {
                break;
            }
        }
    }

    fn draw_graticule(&self, painter: &egui::Painter, rect: egui::Rect) {
        let (west, north) = self.screen_to_lon_lat(rect, rect.left_top());
        let (east, south) = self.screen_to_lon_lat(rect, rect.right_bottom());
        let lon_min = west.min(east);
        let lon_max = west.max(east);
        let lat_min = south.min(north).clamp(-85.0, 85.0);
        let lat_max = south.max(north).clamp(-85.0, 85.0);
        let step = graticule_step(rect.width() / self.map_scale);
        let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(28, 38, 50));
        let label_color = egui::Color32::from_rgb(92, 108, 124);

        let mut lon = (lon_min / step).floor() * step;
        while lon <= lon_max {
            let top = self.lon_lat_to_screen(rect, lon, lat_max);
            let bottom = self.lon_lat_to_screen(rect, lon, lat_min);
            painter.line_segment([top, bottom], stroke);
            painter.text(
                egui::pos2(top.x + 4.0, rect.top() + 6.0),
                egui::Align2::LEFT_TOP,
                format!("{:.0}", normalize_lon(lon)),
                egui::FontId::monospace(10.0),
                label_color,
            );
            lon += step;
        }

        let mut lat = (lat_min / step).floor() * step;
        while lat <= lat_max {
            let left = self.lon_lat_to_screen(rect, lon_min, lat);
            let right = self.lon_lat_to_screen(rect, lon_max, lat);
            painter.line_segment([left, right], stroke);
            painter.text(
                egui::pos2(rect.left() + 6.0, left.y - 2.0),
                egui::Align2::LEFT_CENTER,
                format!("{lat:.0}"),
                egui::FontId::monospace(10.0),
                label_color,
            );
            lat += step;
        }
    }

    fn draw_range_ring(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        latitude_deg: f32,
        longitude_deg: f32,
    ) {
        let (lat_radius, lon_radius) = range_radius_deg(latitude_deg, self.radar_range_km);
        let mut points = Vec::with_capacity(97);
        for index in 0..=96 {
            let angle = index as f32 / 96.0 * std::f32::consts::TAU;
            let latitude = latitude_deg + lat_radius * angle.sin();
            let longitude = longitude_deg + lon_radius * angle.cos();
            points.push(self.lon_lat_to_screen(rect, longitude, latitude));
        }
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(1.5, egui::Color32::from_rgb(104, 128, 148)),
        ));
    }

    fn draw_site_markers(&self, painter: &egui::Painter, site_points: &[(usize, egui::Pos2)]) {
        for (index, position) in site_points {
            let selected = *index == self.selected_site_index;
            let fill = if selected {
                egui::Color32::from_rgb(88, 210, 245)
            } else {
                egui::Color32::from_rgb(106, 132, 154)
            };
            let radius = if selected { 5.5 } else { 3.0 };
            painter.circle_filled(*position, radius, fill);
            if selected {
                painter.circle_stroke(
                    *position,
                    10.0,
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(236, 246, 255)),
                );
                if let Some(site) = self.sites.get(*index) {
                    painter.text(
                        *position + egui::vec2(12.0, -10.0),
                        egui::Align2::LEFT_CENTER,
                        &site.level2_id,
                        egui::FontId::proportional(13.0),
                        egui::Color32::from_rgb(238, 246, 255),
                    );
                }
            }
        }
    }

    fn draw_loaded_volume_marker(&self, painter: &egui::Painter, rect: egui::Rect) {
        let Some(volume) = &self.volume else {
            return;
        };
        let Some((latitude_deg, longitude_deg)) = self.loaded_volume_location() else {
            return;
        };
        let position = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
        if !rect.expand(18.0).contains(position) {
            return;
        }

        painter.circle_filled(position, 6.0, egui::Color32::from_rgb(88, 230, 245));
        painter.circle_stroke(
            position,
            11.0,
            egui::Stroke::new(1.8, egui::Color32::from_rgb(244, 252, 255)),
        );
        painter.text(
            position + egui::vec2(12.0, -10.0),
            egui::Align2::LEFT_CENTER,
            &volume.site.id,
            egui::FontId::proportional(13.0),
            egui::Color32::from_rgb(244, 252, 255),
        );
    }

    fn nearest_site_to_position(&self, rect: egui::Rect, position: egui::Pos2) -> Option<usize> {
        let (target_lon, target_lat) = self.screen_to_lon_lat(rect, position);
        nearest_site_index(&self.sites, target_lat, target_lon)
    }

    fn cursor_readout_at(&self, rect: egui::Rect, position: egui::Pos2) -> Option<CursorReadout> {
        let volume = self.volume.as_ref()?;
        let cut = volume.cuts.get(self.selected_cut)?;
        let base_moment = self.selected_product.base_moment();
        let grid = cut.moments.get(&base_moment)?;
        let (radar_lat, radar_lon) = self.loaded_volume_location()?;
        let (target_lon, target_lat) = self.screen_to_lon_lat(rect, position);
        let lat_km = (target_lat - radar_lat) * 111.32;
        let lon_km = (target_lon - radar_lon) * 111.32 * radar_lat.to_radians().cos();
        let range_km = lat_km.hypot(lon_km);
        let max_range_km = grid_range_km(grid)?;
        if range_km > max_range_km {
            return None;
        }

        let mut azimuth_deg = lon_km.atan2(lat_km).to_degrees();
        if azimuth_deg < 0.0 {
            azimuth_deg += 360.0;
        }
        let (row, radial_index) = nearest_grid_row(cut, grid, azimuth_deg)?;
        let gate = gate_for_range(grid, range_km)?;
        let base_value = grid.scaled_value(row, gate)?;
        let raw = grid_raw_value(grid, row, gate);
        let radial = cut.radials.get(radial_index)?;
        let value = if self.selected_product.is_storm_relative_velocity() {
            storm_relative_velocity_mps(base_value, radial.azimuth_deg, self.current_storm_motion())
        } else {
            base_value
        };
        let storm_motion = self.current_storm_motion();
        let vrot = velocity_vrot_probe(cut, grid, row, gate, &self.selected_product, storm_motion);
        Some(CursorReadout {
            product: self.selected_product.clone(),
            cut: self.selected_cut,
            value,
            base_value: self
                .selected_product
                .is_storm_relative_velocity()
                .then_some(base_value),
            vrot,
            raw,
            row,
            gate,
            gate_spacing_m: grid.gate_range.gate_spacing_m,
            range_km,
            azimuth_deg,
            source_azimuth_deg: radial.azimuth_deg,
            elevation_deg: cut.elevation_deg,
            nyquist_velocity_mps: radial.nyquist_velocity_mps,
        })
    }

    fn lon_lat_to_screen(
        &self,
        rect: egui::Rect,
        longitude_deg: f32,
        latitude_deg: f32,
    ) -> egui::Pos2 {
        egui::pos2(
            rect.center().x + (longitude_deg - self.map_center_lon) * self.map_scale,
            rect.center().y - (latitude_deg - self.map_center_lat) * self.map_scale,
        )
    }

    fn screen_to_lon_lat(&self, rect: egui::Rect, position: egui::Pos2) -> (f32, f32) {
        (
            self.map_center_lon + (position.x - rect.center().x) / self.map_scale,
            self.map_center_lat - (position.y - rect.center().y) / self.map_scale,
        )
    }

    fn visible_geo_bounds(&self, rect: egui::Rect) -> GeoBounds {
        let (west, north) = self.screen_to_lon_lat(rect, rect.left_top());
        let (east, south) = self.screen_to_lon_lat(rect, rect.right_bottom());
        GeoBounds {
            west: west.min(east),
            east: west.max(east),
            south: south.min(north).clamp(-85.0, 85.0),
            north: south.max(north).clamp(-85.0, 85.0),
        }
    }

    fn clamp_map_center(&mut self) {
        self.map_center_lon = normalize_lon(self.map_center_lon);
        self.map_center_lat = self.map_center_lat.clamp(-85.0, 85.0);
    }
}

#[derive(Clone, Copy, Debug)]
struct GeoBounds {
    west: f32,
    south: f32,
    east: f32,
    north: f32,
}

#[derive(Clone, Copy)]
struct RegionalBasemapLayer {
    bounds: [f32; 4],
    admin_lines: &'static [basemap_data::BasemapLine],
    admin_labels: &'static [basemap_data::BasemapLabel],
    place_labels: &'static [basemap_data::BasemapLabel],
}

#[derive(Clone, Copy)]
struct PlaceLabelSet {
    labels: &'static [basemap_data::BasemapLabel],
    max_rank: u8,
    max_labels: usize,
}

const REGIONAL_BASEMAP_LAYERS: &[RegionalBasemapLayer] = &[
    RegionalBasemapLayer {
        bounds: basemap_data::BASEMAP_CANADA_BOUNDS,
        admin_lines: basemap_data::BASEMAP_CANADA_ADMIN_LINES,
        admin_labels: basemap_data::BASEMAP_CANADA_ADMIN_LABELS,
        place_labels: basemap_data::BASEMAP_CANADA_PLACE_LABELS,
    },
    RegionalBasemapLayer {
        bounds: basemap_data::BASEMAP_MEXICO_BOUNDS,
        admin_lines: basemap_data::BASEMAP_MEXICO_ADMIN_LINES,
        admin_labels: basemap_data::BASEMAP_MEXICO_ADMIN_LABELS,
        place_labels: basemap_data::BASEMAP_MEXICO_PLACE_LABELS,
    },
    RegionalBasemapLayer {
        bounds: basemap_data::BASEMAP_JAPAN_BOUNDS,
        admin_lines: basemap_data::BASEMAP_JAPAN_ADMIN_LINES,
        admin_labels: basemap_data::BASEMAP_JAPAN_ADMIN_LABELS,
        place_labels: basemap_data::BASEMAP_JAPAN_PLACE_LABELS,
    },
];

fn us_detail_visible(bounds: GeoBounds) -> bool {
    if !bounds.intersects_bbox(basemap_data::BASEMAP_US_BOUNDS) {
        return false;
    }
    BASEMAP_US_DETAIL_BOUNDS
        .iter()
        .any(|us_bounds| bounds.intersects_bbox(*us_bounds))
}

impl GeoBounds {
    fn expand(self, degrees: f32) -> Self {
        Self {
            west: self.west - degrees,
            south: self.south - degrees,
            east: self.east + degrees,
            north: self.north + degrees,
        }
    }

    fn contains(self, longitude_deg: f32, latitude_deg: f32) -> bool {
        longitude_deg >= self.west
            && longitude_deg <= self.east
            && latitude_deg >= self.south
            && latitude_deg <= self.north
    }

    fn intersects_bbox(self, bbox: [f32; 4]) -> bool {
        bbox[2] >= self.west
            && bbox[0] <= self.east
            && bbox[3] >= self.south
            && bbox[1] <= self.north
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TextureKey {
    cut: usize,
    product: DisplayProduct,
    storm_motion_key: (i16, i16),
    viewport: ViewportKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ViewportKey {
    width: u32,
    height: u32,
    radar_x_px: i32,
    radar_y_px: i32,
    km_per_px_x: i32,
    km_per_px_y: i32,
}

impl ViewportKey {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

fn site_location(site: &RadarSite) -> Option<(f32, f32)> {
    Some((site.latitude_deg?, site.longitude_deg?))
}

fn format_site_label(site: &RadarSite) -> String {
    match &site.name {
        Some(name) if !name.is_empty() => format!("{} {}", site.level2_id, name),
        _ => site.level2_id.clone(),
    }
}

fn range_radius_deg(latitude_deg: f32, range_km: f32) -> (f32, f32) {
    let lat_radius = range_km / 111.32;
    let lon_scale = (111.32 * latitude_deg.to_radians().cos().abs()).max(22.0);
    (lat_radius, range_km / lon_scale)
}

fn grid_range_km(grid: &MomentGrid) -> Option<f32> {
    let first_gate_m = grid.gate_range.first_gate_m.max(0) as f32;
    let gate_spacing_m = grid.gate_range.gate_spacing_m.max(0) as f32;
    let range_km = (first_gate_m + gate_spacing_m * grid.gate_range.gate_count as f32) / 1000.0;
    (range_km > 0.0).then_some(range_km)
}

fn gate_for_range(grid: &MomentGrid, range_km: f32) -> Option<usize> {
    let spacing_m = grid.gate_range.gate_spacing_m.max(1) as f32;
    let gate = ((range_km * 1000.0 - grid.gate_range.first_gate_m as f32) / spacing_m).round();
    if gate < 0.0 || gate as usize >= grid.gate_range.gate_count {
        return None;
    }
    Some(gate as usize)
}

fn nearest_grid_row(
    cut: &ElevationCut,
    grid: &MomentGrid,
    azimuth_deg: f32,
) -> Option<(usize, usize)> {
    let row_count = grid.radial_indices.len();
    if row_count == 0 {
        return None;
    }
    let threshold_deg = (360.0 / row_count as f32 * 0.55).clamp(0.35, 0.8);
    grid.radial_indices
        .iter()
        .enumerate()
        .filter_map(|(row, radial_index)| {
            let radial = cut.radials.get(*radial_index)?;
            let delta = angle_delta_deg(azimuth_deg, radial.azimuth_deg);
            (delta <= threshold_deg).then_some((row, *radial_index, delta))
        })
        .min_by(|left, right| left.2.total_cmp(&right.2))
        .map(|(row, radial_index, _)| (row, radial_index))
}

fn grid_raw_value(grid: &MomentGrid, row: usize, gate: usize) -> Option<u16> {
    let index = row
        .checked_mul(grid.gate_range.gate_count)?
        .checked_add(gate)?;
    match &grid.storage {
        MomentStorage::U8(values) => values.get(index).map(|value| u16::from(*value)),
        MomentStorage::U16(values) => values.get(index).copied(),
        MomentStorage::F32(_) => None,
    }
}

fn velocity_vrot_probe(
    cut: &ElevationCut,
    grid: &MomentGrid,
    center_row: usize,
    center_gate: usize,
    product: &DisplayProduct,
    storm_motion: StormMotion,
) -> Option<VrotProbe> {
    if !matches!(
        product,
        DisplayProduct::Moment(MomentType::Velocity) | DisplayProduct::StormRelativeVelocity
    ) {
        return None;
    }
    if grid.gate_range.gate_count == 0 || grid.radial_indices.is_empty() {
        return None;
    }

    let row_count = grid.radial_indices.len();
    let gate_start = center_gate.saturating_sub(VROT_GATE_RADIUS);
    let gate_end = center_gate
        .saturating_add(VROT_GATE_RADIUS)
        .min(grid.gate_range.gate_count - 1);
    let mut inbound: Option<VelocitySample> = None;
    let mut outbound: Option<VelocitySample> = None;

    for row_delta in -(VROT_ROW_RADIUS as isize)..=(VROT_ROW_RADIUS as isize) {
        let row = (center_row as isize + row_delta).rem_euclid(row_count as isize) as usize;
        for gate in gate_start..=gate_end {
            let Some(sample) = velocity_sample(cut, grid, row, gate, product, storm_motion) else {
                continue;
            };
            if sample.value_mps < 0.0
                && inbound
                    .map(|current| sample.value_mps < current.value_mps)
                    .unwrap_or(true)
            {
                inbound = Some(sample);
            } else if sample.value_mps > 0.0
                && outbound
                    .map(|current| sample.value_mps > current.value_mps)
                    .unwrap_or(true)
            {
                outbound = Some(sample);
            }
        }
    }

    let inbound = inbound?;
    let outbound = outbound?;
    let delta_v_mps = outbound.value_mps - inbound.value_mps;
    let separation_km = (outbound.x_km - inbound.x_km).hypot(outbound.y_km - inbound.y_km);
    Some(VrotProbe {
        delta_v_mps,
        vrot_mps: delta_v_mps.abs() * 0.5,
        separation_km,
        inbound: inbound.vrot_gate(),
        outbound: outbound.vrot_gate(),
    })
}

#[derive(Clone, Copy)]
struct VelocitySample {
    row: usize,
    gate: usize,
    value_mps: f32,
    azimuth_deg: f32,
    x_km: f32,
    y_km: f32,
}

impl VelocitySample {
    fn vrot_gate(self) -> VrotGate {
        VrotGate {
            row: self.row,
            gate: self.gate,
            value_mps: self.value_mps,
            azimuth_deg: self.azimuth_deg,
        }
    }
}

fn velocity_sample(
    cut: &ElevationCut,
    grid: &MomentGrid,
    row: usize,
    gate: usize,
    product: &DisplayProduct,
    storm_motion: StormMotion,
) -> Option<VelocitySample> {
    let radial_index = *grid.radial_indices.get(row)?;
    let radial = cut.radials.get(radial_index)?;
    let base_velocity_mps = grid.scaled_value(row, gate)?;
    let value_mps = if product.is_storm_relative_velocity() {
        storm_relative_velocity_mps(base_velocity_mps, radial.azimuth_deg, storm_motion)
    } else {
        base_velocity_mps
    };
    let range_km = gate_center_range_km(grid, gate);
    let azimuth_rad = radial.azimuth_deg.to_radians();
    Some(VelocitySample {
        row,
        gate,
        value_mps,
        azimuth_deg: radial.azimuth_deg,
        x_km: range_km * azimuth_rad.sin(),
        y_km: range_km * azimuth_rad.cos(),
    })
}

fn gate_center_range_km(grid: &MomentGrid, gate: usize) -> f32 {
    let first_gate_m = grid.gate_range.first_gate_m.max(0) as f32;
    let spacing_m = grid.gate_range.gate_spacing_m.max(1) as f32;
    (first_gate_m + spacing_m * gate as f32) / 1000.0
}

fn angle_delta_deg(left: f32, right: f32) -> f32 {
    let delta = (left - right).abs().rem_euclid(360.0);
    delta.min(360.0 - delta)
}

fn moment_units(moment: &MomentType) -> &'static str {
    match moment {
        MomentType::Reflectivity => "dBZ",
        MomentType::Velocity | MomentType::SpectrumWidth => "m/s",
        MomentType::DifferentialReflectivity => "dB",
        MomentType::CorrelationCoefficient => "rho",
        MomentType::DifferentialPhase => "deg",
        MomentType::SpecificDifferentialPhase => "deg/km",
        MomentType::Unknown(_) => "",
    }
}

fn product_units(product: &DisplayProduct) -> &'static str {
    match product {
        DisplayProduct::Moment(moment) => moment_units(moment),
        DisplayProduct::StormRelativeVelocity => "m/s",
    }
}

fn format_cursor_readout(readout: &CursorReadout) -> String {
    let raw = readout
        .raw
        .map(|raw| raw.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let units = product_units(&readout.product);
    let value = if units.is_empty() {
        format!("{:.1}", readout.value)
    } else {
        format!("{:.1} {units}", readout.value)
    };
    let base_value = readout
        .base_value
        .map(|value| format!(" VEL {:.1} m/s", value))
        .unwrap_or_default();
    let vrot = readout
        .vrot
        .map(|probe| {
            format!(
                " Vrot {:.1} m/s dV {:.1} sep {:.2} km in r{}/g{} {:05.1} {:.1} out r{}/g{} {:05.1} {:.1}",
                probe.vrot_mps,
                probe.delta_v_mps,
                probe.separation_km,
                probe.inbound.row,
                probe.inbound.gate,
                probe.inbound.azimuth_deg,
                probe.inbound.value_mps,
                probe.outbound.row,
                probe.outbound.gate,
                probe.outbound.azimuth_deg,
                probe.outbound.value_mps
            )
        })
        .unwrap_or_default();
    let nyquist = readout
        .nyquist_velocity_mps
        .map(|nyquist| format!(" Nyq {:.1} m/s", nyquist))
        .unwrap_or_default();
    format!(
        "{} cut {} {} raw {} row {} gate {} @ {} m{}{} az {:05.1} src {:05.1} range {:.1} km elev {:.2}{}",
        readout.product.label(),
        readout.cut,
        value,
        raw,
        readout.row,
        readout.gate,
        readout.gate_spacing_m,
        base_value,
        vrot,
        readout.azimuth_deg,
        readout.source_azimuth_deg,
        readout.range_km,
        readout.elevation_deg,
        nyquist
    )
}

fn graticule_step(visible_degrees: f32) -> f32 {
    if visible_degrees > 140.0 {
        30.0
    } else if visible_degrees > 80.0 {
        20.0
    } else if visible_degrees > 40.0 {
        10.0
    } else if visible_degrees > 16.0 {
        5.0
    } else if visible_degrees > 6.0 {
        2.0
    } else if visible_degrees > 2.0 {
        1.0
    } else if visible_degrees > 0.7 {
        0.5
    } else {
        0.25
    }
}

fn world_place_label_rank(map_scale: f32) -> Option<u8> {
    if map_scale < 10.0 {
        None
    } else if map_scale < 28.0 {
        Some(0)
    } else if map_scale < 58.0 {
        Some(1)
    } else {
        None
    }
}

fn world_label_budget(map_scale: f32) -> usize {
    if map_scale < 28.0 { 18 } else { 36 }
}

fn place_label_rank(map_scale: f32) -> Option<u8> {
    if map_scale < 24.0 {
        None
    } else if map_scale < 42.0 {
        Some(0)
    } else if map_scale < 72.0 {
        Some(2)
    } else if map_scale < 130.0 {
        Some(4)
    } else if map_scale < 230.0 {
        Some(5)
    } else {
        Some(6)
    }
}

fn label_budget(map_scale: f32) -> usize {
    if map_scale < 72.0 {
        28
    } else if map_scale < 130.0 {
        54
    } else if map_scale < 230.0 {
        92
    } else {
        140
    }
}

fn left_label_rect(position: egui::Pos2, text: &str, font_size: f32) -> egui::Rect {
    let width = estimated_label_width(text, font_size);
    let height = font_size + 5.0;
    egui::Rect::from_min_size(
        egui::pos2(position.x, position.y - height * 0.5),
        egui::vec2(width, height),
    )
}

fn centered_label_rect(position: egui::Pos2, text: &str, font_size: f32) -> egui::Rect {
    let width = estimated_label_width(text, font_size);
    let height = font_size + 5.0;
    egui::Rect::from_center_size(position, egui::vec2(width, height))
}

fn estimated_label_width(text: &str, font_size: f32) -> f32 {
    text.chars().count() as f32 * font_size * 0.58 + 8.0
}

fn overlaps_any(existing: &[egui::Rect], candidate: egui::Rect) -> bool {
    existing.iter().any(|rect| rect.intersects(candidate))
}

fn draw_halo_text(
    painter: &egui::Painter,
    position: egui::Pos2,
    align: egui::Align2,
    text: &str,
    font: egui::FontId,
    text_color: egui::Color32,
    halo_color: egui::Color32,
) {
    for offset in [
        egui::vec2(-1.0, 0.0),
        egui::vec2(1.0, 0.0),
        egui::vec2(0.0, -1.0),
        egui::vec2(0.0, 1.0),
    ] {
        painter.text(position + offset, align, text, font.clone(), halo_color);
    }
    painter.text(position, align, text, font, text_color);
}

fn normalize_lon(longitude_deg: f32) -> f32 {
    let mut longitude_deg = longitude_deg;
    while longitude_deg > 180.0 {
        longitude_deg -= 360.0;
    }
    while longitude_deg < -180.0 {
        longitude_deg += 360.0;
    }
    longitude_deg
}

fn haversine_km(lat_a: f32, lon_a: f32, lat_b: f32, lon_b: f32) -> f32 {
    let earth_radius_km = 6371.0_f32;
    let d_lat = (lat_b - lat_a).to_radians();
    let d_lon = (lon_b - lon_a).to_radians();
    let lat_a = lat_a.to_radians();
    let lat_b = lat_b.to_radians();
    let a = (d_lat / 2.0).sin().powi(2) + lat_a.cos() * lat_b.cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * earth_radius_km * a.sqrt().atan2((1.0 - a).max(0.0).sqrt())
}

fn nearest_site_index(sites: &[RadarSite], target_lat: f32, target_lon: f32) -> Option<usize> {
    sites
        .iter()
        .enumerate()
        .filter_map(|(index, site)| {
            let (latitude_deg, longitude_deg) = site_location(site)?;
            let distance_km = haversine_km(target_lat, target_lon, latitude_deg, longitude_deg);
            Some((index, distance_km))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, _)| index)
}

fn product_order(available: &std::collections::BTreeSet<MomentType>) -> Vec<DisplayProduct> {
    let mut ordered = Vec::new();
    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::SpectrumWidth,
        MomentType::DifferentialReflectivity,
        MomentType::CorrelationCoefficient,
        MomentType::DifferentialPhase,
        MomentType::SpecificDifferentialPhase,
    ] {
        if available.contains(&moment) {
            if moment == MomentType::Velocity {
                ordered.push(DisplayProduct::Moment(MomentType::Velocity));
                ordered.push(DisplayProduct::StormRelativeVelocity);
            } else {
                ordered.push(DisplayProduct::Moment(moment));
            }
        }
    }
    for moment in available {
        let product = DisplayProduct::Moment(moment.clone());
        if !ordered.contains(&product) {
            ordered.push(product);
        }
    }
    ordered
}

fn global_displayable_products(volume: &RadarVolume) -> Vec<DisplayProduct> {
    let mut available = std::collections::BTreeSet::new();
    for cut_index in 0..volume.cuts.len() {
        available.extend(
            displayable_products(volume, cut_index)
                .into_iter()
                .map(|product| product.base_moment()),
        );
    }
    product_order(&available)
}

fn displayable_products(volume: &RadarVolume, cut_index: usize) -> Vec<DisplayProduct> {
    let Some(cut) = volume.cuts.get(cut_index) else {
        return Vec::new();
    };
    let available = cut
        .moments
        .values()
        .filter(|grid| grid.radial_count() >= displayable_radial_threshold(cut.radials.len()))
        .map(|grid| grid.moment.clone())
        .collect::<std::collections::BTreeSet<_>>();
    product_order(&available)
}

fn is_displayable_on_cut(volume: &RadarVolume, cut_index: usize, product: &DisplayProduct) -> bool {
    let Some(cut) = volume.cuts.get(cut_index) else {
        return false;
    };
    let base_moment = product.base_moment();
    let Some(grid) = cut.moments.get(&base_moment) else {
        return false;
    };
    grid.radial_count() >= displayable_radial_threshold(cut.radials.len())
}

fn displayable_radial_threshold(cut_radials: usize) -> usize {
    MIN_DISPLAYABLE_RADIALS.min((cut_radials / 2).max(1))
}

fn selection_for_installed_volume(
    previous_volume: Option<&RadarVolume>,
    previous_cut: usize,
    previous_product: &DisplayProduct,
    volume: &RadarVolume,
) -> (usize, DisplayProduct) {
    let same_site = previous_volume.is_some_and(|previous| previous.site.id == volume.site.id);
    if same_site && let Some(cut) = best_cut_for_product(volume, previous_cut, previous_product) {
        return (cut, previous_product.clone());
    }

    default_selection_for_volume(volume)
}

fn default_selection_for_volume(volume: &RadarVolume) -> (usize, DisplayProduct) {
    let reflectivity = DisplayProduct::Moment(MomentType::Reflectivity);
    if is_displayable_on_cut(volume, 0, &reflectivity) {
        return (0, reflectivity);
    }

    for cut_index in 0..volume.cuts.len() {
        if let Some(product) = displayable_products(volume, cut_index).first().cloned() {
            return (cut_index, product);
        }
    }

    (0, reflectivity)
}

fn best_cut_for_product(
    volume: &RadarVolume,
    current_cut: usize,
    product: &DisplayProduct,
) -> Option<usize> {
    let current_elevation = volume.cuts.get(current_cut).map(|cut| cut.elevation_deg);
    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(index, _)| is_displayable_on_cut(volume, *index, product))
        .min_by(|(left_index, left_cut), (right_index, right_cut)| {
            let left_delta = current_elevation
                .map(|elevation| (left_cut.elevation_deg - elevation).abs())
                .unwrap_or(*left_index as f32);
            let right_delta = current_elevation
                .map(|elevation| (right_cut.elevation_deg - elevation).abs())
                .unwrap_or(*right_index as f32);
            left_delta.total_cmp(&right_delta)
        })
        .map(|(index, _)| index)
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = egui::Color32::from_rgb(18, 22, 28);
    style.visuals.window_fill = egui::Color32::from_rgb(18, 22, 28);
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(50, 96, 138);
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(46, 58, 72);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    ctx.set_global_style(style);
}

fn radar_color_image_from_rgba(size: [usize; 2], rgba: &[u8]) -> egui::ColorImage {
    assert_eq!(
        size[0] * size[1] * 4,
        rgba.len(),
        "size: {:?}, rgba.len(): {}",
        size,
        rgba.len()
    );
    debug_assert_eq!(std::mem::size_of::<egui::Color32>(), 4);
    debug_assert_eq!(std::mem::align_of::<egui::Color32>(), 4);
    debug_assert!(radar_rgba_is_premultiplied_compatible(rgba));

    let mut pixels = Vec::<egui::Color32>::with_capacity(size[0] * size[1]);
    // SAFETY: Color32 is a repr(C), 4-byte-aligned wrapper over [u8; 4] in egui 0.34.
    // We allocate Color32 storage, copy whole RGBA texels into it, then set the exact texel length.
    unsafe {
        let dst = pixels.as_mut_ptr().cast::<u8>();
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), dst, rgba.len());
        pixels.set_len(size[0] * size[1]);
    }
    egui::ColorImage::new(size, pixels)
}

fn radar_rgba_is_premultiplied_compatible(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).all(|pixel| match pixel[3] {
        0 => pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0,
        255 => true,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_site_index_prefers_closest_coordinate() {
        let sites = vec![
            RadarSite::new("KFWS").with_location(
                Some("Fort Worth".to_owned()),
                Some(32.573),
                Some(-97.303),
            ),
            RadarSite::new("KTLX").with_location(
                Some("Norman".to_owned()),
                Some(35.333),
                Some(-97.278),
            ),
        ];

        let index = nearest_site_index(&sites, 35.4, -97.2).expect("nearest station");
        assert_eq!(sites[index].level2_id, "KTLX");
    }

    #[test]
    fn haversine_is_zero_for_same_point() {
        assert!(haversine_km(35.333, -97.278, 35.333, -97.278) < 0.001);
    }

    #[test]
    fn gate_for_range_uses_selected_gate_spacing() {
        let grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            radar_core::GateRange {
                first_gate_m: 500,
                gate_spacing_m: 250,
                gate_count: 4,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        );

        assert_eq!(gate_for_range(&grid, 0.50), Some(0));
        assert_eq!(gate_for_range(&grid, 0.75), Some(1));
        assert_eq!(gate_for_range(&grid, 1.25), Some(3));
        assert_eq!(gate_for_range(&grid, 1.50), None);
    }

    #[test]
    fn vrot_probe_uses_source_velocity_gates() {
        let gate_range = radar_core::GateRange {
            first_gate_m: 500,
            gate_spacing_m: 250,
            gate_count: 5,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.radials.push(test_radial(0.0, gate_range.clone()));
        cut.radials.push(test_radial(1.0, gate_range.clone()));

        let mut grid = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range,
            1.0,
            64.0,
            Some(0),
            Some(1),
        );
        grid.push_u8_row_slice(0, &[64, 54, 54, 64, 64])
            .expect("first velocity row");
        grid.push_u8_row_slice(1, &[64, 64, 84, 84, 64])
            .expect("second velocity row");

        let probe = velocity_vrot_probe(
            &cut,
            &grid,
            0,
            2,
            &DisplayProduct::Moment(MomentType::Velocity),
            StormMotion {
                direction_deg: 0.0,
                speed_mps: 0.0,
            },
        )
        .expect("vrot probe");

        assert_eq!(probe.delta_v_mps, 30.0);
        assert_eq!(probe.vrot_mps, 15.0);
        assert!(probe.separation_km > 0.0);
        assert_eq!(probe.inbound.row, 0);
        assert_eq!(probe.inbound.gate, 1);
        assert_eq!(probe.inbound.value_mps, -10.0);
        assert_eq!(probe.outbound.row, 1);
        assert_eq!(probe.outbound.gate, 2);
        assert_eq!(probe.outbound.value_mps, 20.0);
    }

    #[test]
    fn cursor_readout_format_reports_source_gate_provenance() {
        let readout = CursorReadout {
            product: DisplayProduct::Moment(MomentType::Velocity),
            cut: 1,
            value: 22.5,
            base_value: None,
            vrot: None,
            raw: Some(86),
            row: 42,
            gate: 123,
            gate_spacing_m: 250,
            range_km: 31.2,
            azimuth_deg: 181.2,
            source_azimuth_deg: 180.9,
            elevation_deg: 0.48,
            nyquist_velocity_mps: Some(32.0),
        };

        let formatted = format_cursor_readout(&readout);

        assert!(formatted.contains("row 42 gate 123"));
        assert!(formatted.contains("az 181.2 src 180.9"));
        assert!(formatted.contains("raw 86"));
    }

    #[test]
    fn cursor_readout_format_reports_vrot_gate_endpoints() {
        let readout = CursorReadout {
            product: DisplayProduct::Moment(MomentType::Velocity),
            cut: 1,
            value: 22.5,
            base_value: None,
            vrot: Some(VrotProbe {
                delta_v_mps: 42.0,
                vrot_mps: 21.0,
                separation_km: 1.25,
                inbound: VrotGate {
                    row: 4,
                    gate: 100,
                    value_mps: -18.0,
                    azimuth_deg: 210.5,
                },
                outbound: VrotGate {
                    row: 6,
                    gate: 103,
                    value_mps: 24.0,
                    azimuth_deg: 212.0,
                },
            }),
            raw: Some(86),
            row: 5,
            gate: 101,
            gate_spacing_m: 250,
            range_km: 31.2,
            azimuth_deg: 211.2,
            source_azimuth_deg: 211.0,
            elevation_deg: 0.48,
            nyquist_velocity_mps: Some(32.0),
        };

        let formatted = format_cursor_readout(&readout);

        assert!(formatted.contains("Vrot 21.0 m/s dV 42.0 sep 1.25 km"));
        assert!(formatted.contains("in r4/g100 210.5 -18.0"));
        assert!(formatted.contains("out r6/g103 212.0 24.0"));
    }

    #[test]
    fn cache_policy_scales_with_cpu_budget() {
        let low = RenderWorkerCachePolicy { threads: 4 };
        let mid = RenderWorkerCachePolicy { threads: 8 };
        let high = RenderWorkerCachePolicy { threads: 16 };

        assert_eq!(low.sample_cache_capacity(), 1);
        assert_eq!(low.moment_cache_capacity(), 1);
        assert_eq!(low.sample_cache_bytes(), LOW_END_SAMPLE_CACHE_BYTES);
        assert_eq!(mid.sample_cache_capacity(), 3);
        assert_eq!(mid.moment_cache_capacity(), 3);
        assert_eq!(mid.sample_cache_bytes(), MID_RANGE_SAMPLE_CACHE_BYTES);
        assert_eq!(high.sample_cache_capacity(), 6);
        assert_eq!(high.moment_cache_capacity(), 6);
        assert_eq!(high.sample_cache_bytes(), HIGH_END_SAMPLE_CACHE_BYTES);
    }

    #[test]
    fn rayon_thread_cap_overrides_machine_budget() {
        assert_eq!(configured_rayon_threads_from(Some("2")), Some(2));
        assert_eq!(configured_rayon_threads_from(Some(" 4 ")), Some(4));
        assert_eq!(configured_rayon_threads_from(Some("0")), None);
        assert_eq!(configured_rayon_threads_from(Some("not-a-number")), None);
        assert_eq!(configured_rayon_threads_from(None), None);
    }

    #[test]
    fn preview_policy_follows_effective_worker_budget() {
        assert!(should_preview_loads_for_threads(false, 2));
        assert!(should_preview_loads_for_threads(
            false,
            LOW_CORE_PREVIEW_THREADS
        ));
        assert!(!should_preview_loads_for_threads(
            false,
            LOW_CORE_PREVIEW_THREADS + 1
        ));
        assert!(should_preview_loads_for_threads(true, 64));
    }

    #[test]
    fn preview_head_start_is_only_for_low_core_budgets() {
        assert_eq!(
            preview_render_head_start(LOW_CORE_PREVIEW_THREADS),
            Duration::from_millis(LOW_CORE_PREVIEW_RENDER_HEAD_START_MS)
        );
        assert_eq!(
            preview_render_head_start(LOW_CORE_PREVIEW_THREADS + 1),
            Duration::ZERO
        );
    }

    #[test]
    fn cache_policy_warms_slow_low_end_direct_renders() {
        let low = RenderWorkerCachePolicy { threads: 2 };
        let mid = RenderWorkerCachePolicy { threads: 8 };

        assert!(!low.should_speculatively_warm_sample_cache(&test_rendered_texture(3.5, false)));
        assert!(low.should_speculatively_warm_sample_cache(&test_rendered_texture(4.0, false)));
        assert!(mid.should_speculatively_warm_sample_cache(&test_rendered_texture(0.25, false)));
        assert!(!mid.should_speculatively_warm_sample_cache(&test_rendered_texture(8.0, true)));
    }

    #[test]
    fn cache_policy_skips_sample_caches_that_cannot_fit_budget() {
        let low = RenderWorkerCachePolicy { threads: 2 };
        let high = RenderWorkerCachePolicy { threads: 16 };

        assert!(!low.should_build_sample_cache_for_viewport(test_viewport_key(1920, 1080)));
        assert!(
            low.should_speculatively_warm_sample_cache(&test_rendered_texture_with_size(
                4.0, false, 1920, 1080
            ))
        );
        assert!(high.should_build_sample_cache_for_viewport(test_viewport_key(3840, 2160)));
    }

    #[test]
    fn cache_policy_uses_exact_radar_footprint_for_active_cache_builds() {
        let low = RenderWorkerCachePolicy { threads: 2 };
        let volume = test_ref_then_velocity_volume();
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");
        let options = ViewportRasterOptions {
            width: 1920,
            height: 1080,
            radar_x_px: 960.0,
            radar_y_px: 540.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
        };

        assert!(!low.should_build_sample_cache_for_viewport(test_viewport_key(1920, 1080)));
        assert!(
            low.should_build_sample_cache_for_moment_cache(&cache, &volume, options)
                .expect("sample cache footprint estimate")
        );
    }

    #[test]
    fn cache_policy_prefetches_interaction_cache_only_with_cpu_budget() {
        let low = RenderWorkerCachePolicy { threads: 4 };
        let mid = RenderWorkerCachePolicy { threads: 8 };
        let high = RenderWorkerCachePolicy { threads: 16 };

        assert!(!low.should_prefetch_interaction_cache((1320, 820)));
        assert!(mid.should_prefetch_interaction_cache((1320, 820)));
        assert!(!mid.should_prefetch_interaction_cache((320, 240)));
        assert!(high.should_prefetch_interaction_cache((3840, 2160)));
    }

    #[test]
    fn velocity_prefetch_targets_nearest_displayable_velocity_cut() {
        let volume = Arc::new(test_ref_then_velocity_volume());
        let request = RenderRequest {
            key: TextureKey {
                cut: 0,
                product: DisplayProduct::Moment(MomentType::Reflectivity),
                storm_motion_key: (450, 350),
                viewport: test_viewport_key(1320, 820),
            },
            volume,
            cut: 0,
            product: DisplayProduct::Moment(MomentType::Reflectivity),
            storm_motion: StormMotion {
                direction_deg: 45.0,
                speed_mps: 35.0 * KNOT_TO_MPS,
            },
            viewport_options: ViewportRasterOptions {
                width: 1320,
                height: 820,
                radar_x_px: 660.0,
                radar_y_px: 410.0,
                km_per_px_x: 0.16,
                km_per_px_y: 0.16,
            },
            radar_range_km: DEFAULT_RADAR_RANGE_KM,
        };

        assert_eq!(ViewerApp::prefetch_velocity_cut(&request), Some(1));
        assert!(ViewerApp::should_prefetch_velocity_interaction_cache(
            &request,
            &test_rendered_texture_with_size(1.0, false, 1320, 820),
            RenderWorkerCachePolicy { threads: 8 },
        ));
    }

    #[test]
    fn same_site_install_preserves_velocity_selection() {
        let previous = test_ref_then_velocity_volume();
        let next = previous.clone();

        assert_eq!(
            selection_for_installed_volume(
                Some(&previous),
                0,
                &DisplayProduct::Moment(MomentType::Velocity),
                &next
            ),
            (1, DisplayProduct::Moment(MomentType::Velocity))
        );
        assert_eq!(
            selection_for_installed_volume(
                Some(&previous),
                0,
                &DisplayProduct::StormRelativeVelocity,
                &next
            ),
            (1, DisplayProduct::StormRelativeVelocity)
        );
    }

    #[test]
    fn different_site_install_starts_from_default_reflectivity() {
        let previous = test_ref_then_velocity_volume();
        let mut next = previous.clone();
        next.site.id = "OTHER".to_owned();

        assert_eq!(
            selection_for_installed_volume(
                Some(&previous),
                1,
                &DisplayProduct::Moment(MomentType::Velocity),
                &next
            ),
            (0, DisplayProduct::Moment(MomentType::Reflectivity))
        );
    }

    #[test]
    fn direct_viewport_lru_keeps_newest_signatures() {
        let policy = RenderWorkerCachePolicy { threads: 4 };
        let mut signatures = Vec::new();
        let first = test_viewport_signature(1);
        let second = test_viewport_signature(2);
        let third = test_viewport_signature(3);

        ViewerApp::remember_direct_viewport(&mut signatures, policy, first.clone());
        ViewerApp::remember_direct_viewport(&mut signatures, policy, second.clone());
        ViewerApp::remember_direct_viewport(&mut signatures, policy, third.clone());

        assert_eq!(signatures, vec![second, third]);
        assert!(!ViewerApp::has_direct_viewport(&signatures, &first));
    }

    #[test]
    fn radar_color_image_bulk_copy_preserves_rendered_texels() {
        let rgba = [
            0, 0, 0, 0, //
            255, 32, 16, 255, //
            4, 128, 255, 255, //
            0, 0, 0, 0,
        ];

        let image = radar_color_image_from_rgba([2, 2], &rgba);

        assert_eq!(image.pixels[0].to_array(), [0, 0, 0, 0]);
        assert_eq!(image.pixels[1].to_array(), [255, 32, 16, 255]);
        assert_eq!(image.pixels[2].to_array(), [4, 128, 255, 255]);
        assert_eq!(image.pixels[3].to_array(), [0, 0, 0, 0]);
    }

    #[test]
    fn radar_rgba_compatibility_rejects_non_rendered_alpha() {
        assert!(radar_rgba_is_premultiplied_compatible(&[
            0, 0, 0, 0, 16, 32, 48, 255
        ]));
        assert!(!radar_rgba_is_premultiplied_compatible(&[16, 0, 0, 0]));
        assert!(!radar_rgba_is_premultiplied_compatible(&[16, 32, 48, 128]));
    }

    #[test]
    fn metric_series_tracks_latest_percentiles_and_ring_capacity() {
        let mut series = MetricSeries::new();
        series.push(f32::NAN);
        series.push(-1.0);
        assert_eq!(series.summary(), None);

        for sample in 0..100 {
            series.push(sample as f32);
        }

        let summary = series.summary().expect("summary");
        assert_eq!(summary.count, PERF_SAMPLE_CAPACITY);
        assert_eq!(summary.latest, 99.0);
        assert_eq!(summary.min, 4.0);
        assert_eq!(summary.p50, 52.0);
        assert_eq!(summary.p95, 94.0);
        assert_eq!(summary.max, 99.0);
    }

    #[test]
    fn perf_telemetry_splits_direct_and_cached_render_samples() {
        let mut perf = PerfTelemetry::new();

        perf.record_decode(42.0);
        perf.record_render(8.0, false, 9.0, 2.0, Some(11.0));
        perf.record_render(0.5, true, 0.8, 1.5, None);

        assert_eq!(perf.decode.summary().expect("decode").latest, 42.0);
        assert_eq!(perf.direct_render.summary().expect("direct").latest, 8.0);
        assert_eq!(perf.cached_render.summary().expect("cached").latest, 0.5);
        assert_eq!(perf.worker.summary().expect("worker").count, 2);
        assert_eq!(perf.texture.summary().expect("texture").p95, 2.0);
        assert_eq!(
            perf.cache_build.summary().expect("cache build").latest,
            11.0
        );
    }

    #[test]
    fn basemap_regional_packs_have_real_content() {
        assert_eq!(REGIONAL_BASEMAP_LAYERS.len(), 3);
        assert!(basemap_data::BASEMAP_WORLD_COUNTRY_LINES.len() > 1_000);
        assert!(basemap_data::BASEMAP_WORLD_COUNTRY_LINES.len() < 2_000);
        assert!(basemap_data::BASEMAP_US_COUNTY_LINES.len() > 4_000);
        assert!(basemap_data::BASEMAP_US_PLACE_LABELS.len() > 500);

        for layer in REGIONAL_BASEMAP_LAYERS {
            assert!(layer.admin_lines.len() > 50);
            assert!(layer.admin_labels.len() > 10);
            assert!(layer.place_labels.len() > 50);
        }
    }

    #[test]
    fn basemap_detail_layers_are_gated_by_viewport() {
        let central_us = GeoBounds {
            west: -101.0,
            south: 35.0,
            east: -90.0,
            north: 40.0,
        };
        let canada_interior = GeoBounds {
            west: -111.0,
            south: 51.0,
            east: -100.0,
            north: 55.0,
        };
        let mexico_city = GeoBounds {
            west: -101.0,
            south: 18.0,
            east: -97.0,
            north: 21.0,
        };
        let japan_kanto = GeoBounds {
            west: 138.0,
            south: 34.0,
            east: 141.0,
            north: 37.0,
        };
        let alaska = GeoBounds {
            west: -154.0,
            south: 58.0,
            east: -149.0,
            north: 62.0,
        };

        assert!(us_detail_visible(central_us));
        assert!(us_detail_visible(alaska));
        assert!(!us_detail_visible(canada_interior));
        assert!(!us_detail_visible(mexico_city));
        assert!(!us_detail_visible(japan_kanto));

        assert_eq!(active_regional_layer_count(central_us), 0);
        assert_eq!(active_regional_layer_count(canada_interior), 1);
        assert_eq!(active_regional_layer_count(mexico_city), 1);
        assert_eq!(active_regional_layer_count(japan_kanto), 1);
        assert_eq!(active_regional_layer_count(alaska), 0);
    }

    #[test]
    fn basemap_culling_keeps_representative_views_bounded() {
        let central_us = GeoBounds {
            west: -101.0,
            south: 35.0,
            east: -90.0,
            north: 40.0,
        };
        let canada_interior = GeoBounds {
            west: -111.0,
            south: 51.0,
            east: -100.0,
            north: 55.0,
        };
        let japan_kanto = GeoBounds {
            west: 138.0,
            south: 34.0,
            east: 141.0,
            north: 37.0,
        };

        let us_counties =
            basemap_line_candidates(central_us, basemap_data::BASEMAP_US_COUNTY_LINES);
        assert!(us_counties.lines < 400);
        assert!(us_counties.points < 8_000);

        let canada_admin =
            basemap_line_candidates(canada_interior, basemap_data::BASEMAP_CANADA_ADMIN_LINES);
        assert!(canada_admin.lines > 0);
        assert!(canada_admin.lines < 60);
        assert!(canada_admin.points < 5_000);
        assert!(!us_detail_visible(canada_interior));

        let japan_admin =
            basemap_line_candidates(japan_kanto, basemap_data::BASEMAP_JAPAN_ADMIN_LINES);
        assert!(japan_admin.lines > 0);
        assert!(japan_admin.lines < 40);
        assert!(japan_admin.points < 2_000);
        assert!(!us_detail_visible(japan_kanto));
    }

    fn test_ref_then_velocity_volume() -> RadarVolume {
        let gate_range = radar_core::GateRange {
            first_gate_m: 500,
            gate_spacing_m: 250,
            gate_count: 3,
        };
        let mut volume = RadarVolume::new(
            radar_core::RadarSite::new("TEST"),
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        );

        let mut reflectivity_cut = ElevationCut::new(0.26, Some(1));
        reflectivity_cut
            .radials
            .push(test_radial(0.0, gate_range.clone()));
        let mut reflectivity_grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        reflectivity_grid
            .push_u8_row_slice(0, &[66, 80, 90])
            .expect("reflectivity row");
        reflectivity_cut
            .moments
            .insert(MomentType::Reflectivity, reflectivity_grid);
        volume.cuts.push(reflectivity_cut);

        let mut velocity_cut = ElevationCut::new(0.26, Some(2));
        velocity_cut
            .radials
            .push(test_radial(0.0, gate_range.clone()));
        let mut velocity_grid = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range,
            1.0,
            64.0,
            Some(0),
            Some(1),
        );
        velocity_grid
            .push_u8_row_slice(0, &[64, 74, 54])
            .expect("velocity row");
        velocity_cut
            .moments
            .insert(MomentType::Velocity, velocity_grid);
        volume.cuts.push(velocity_cut);

        volume
    }

    fn test_radial(azimuth_deg: f32, gate_range: radar_core::GateRange) -> radar_core::Radial {
        radar_core::Radial {
            azimuth_deg,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range,
            nyquist_velocity_mps: Some(32.0),
            radial_status: None,
        }
    }

    fn test_viewport_signature(width: u32) -> RenderWorkerViewportSignature {
        RenderWorkerViewportSignature::new(
            1,
            width as usize,
            MomentType::Velocity,
            test_viewport_key(width, 100),
        )
    }

    fn test_viewport_key(width: u32, height: u32) -> ViewportKey {
        ViewportKey {
            width,
            height,
            radar_x_px: 0,
            radar_y_px: 0,
            km_per_px_x: 1,
            km_per_px_y: 1,
        }
    }

    fn test_rendered_texture(render_ms: f32, used_sample_cache: bool) -> RenderedTexture {
        test_rendered_texture_with_size(render_ms, used_sample_cache, 720, 480)
    }

    fn test_rendered_texture_with_size(
        render_ms: f32,
        used_sample_cache: bool,
        width: u32,
        height: u32,
    ) -> RenderedTexture {
        RenderedTexture {
            width: width as usize,
            height: height as usize,
            rgba: Vec::new(),
            buffer_signature: RenderWorkerViewportSignature::new(
                1,
                1,
                MomentType::Velocity,
                test_viewport_key(width, height),
            ),
            render_ms,
            worker_ms: render_ms,
            sample_cache_build_ms: None,
            used_sample_cache,
            radar_range_km: 460.0,
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct BasemapLineCandidates {
        lines: usize,
        points: usize,
    }

    fn basemap_line_candidates(
        bounds: GeoBounds,
        lines: &[basemap_data::BasemapLine],
    ) -> BasemapLineCandidates {
        let mut candidates = BasemapLineCandidates {
            lines: 0,
            points: 0,
        };
        for line in lines {
            if bounds.intersects_bbox(line.bbox) {
                candidates.lines += 1;
                candidates.points += line.points.len();
            }
        }
        candidates
    }

    fn active_regional_layer_count(bounds: GeoBounds) -> usize {
        REGIONAL_BASEMAP_LAYERS
            .iter()
            .filter(|layer| bounds.intersects_bbox(layer.bounds))
            .count()
    }
}
