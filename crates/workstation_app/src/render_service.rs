use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::Instant;

use analyst_runtime::{
    Camera2D, LatestLaneSender, PaneId, RenderStamp, StormMotionIntent, ViewportMetrics,
    latest_lane_channel,
};
use color_tables::ColorTableSet;
use eframe::egui;
use radar_core::RadarVolume;
use render2d::{
    StormMotion, ViewportMomentCache, ViewportRasterOptions, viewport_rgba_buffer_len,
};

use crate::product::DisplayProduct;

const RESULT_QUEUE_CAPACITY: usize = 8;

pub struct RenderRequest {
    pub pane: PaneId,
    pub stamp: RenderStamp,
    pub volume: Arc<RadarVolume>,
    pub cut_index: usize,
    pub product: DisplayProduct,
    pub camera: Camera2D,
    pub viewport: ViewportMetrics,
    pub storm_motion: StormMotionIntent,
    pub color_tables: Arc<ColorTableSet>,
}

pub struct RenderedPane {
    pub pane: PaneId,
    pub stamp: RenderStamp,
    pub camera: Camera2D,
    pub viewport: ViewportMetrics,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub elapsed_ms: f32,
}

pub enum RenderUpdate {
    Completed(RenderedPane),
    Failed {
        pane: PaneId,
        stamp: RenderStamp,
        message: String,
    },
}

pub struct RenderService {
    sender: LatestLaneSender<PaneId, RenderRequest>,
    receiver: Receiver<RenderUpdate>,
}

impl RenderService {
    pub fn new(context: egui::Context) -> Self {
        let (request_sender, request_receiver) = latest_lane_channel::<PaneId, RenderRequest>();
        let (result_sender, result_receiver) = mpsc::sync_channel(RESULT_QUEUE_CAPACITY);
        let _ = thread::Builder::new()
            .name("radar-workstation-render".to_owned())
            .spawn(move || {
                while let Some((_pane, request)) = request_receiver.recv() {
                    let update = match render_request(request) {
                        Ok(rendered) => RenderUpdate::Completed(rendered),
                        Err(failure) => failure,
                    };
                    if result_sender.send(update).is_err() {
                        break;
                    }
                    context.request_repaint();
                }
            })
            .expect("failed to start radar render worker");

        Self {
            sender: request_sender,
            receiver: result_receiver,
        }
    }

    pub fn request(&self, request: RenderRequest) -> Result<(), RenderRequest> {
        self.sender
            .submit(request.pane, request)
            .map(|_| ())
            .map_err(|closed| closed.0)
    }

    pub fn try_recv(&self) -> Option<RenderUpdate> {
        match self.receiver.try_recv() {
            Ok(update) => Some(update),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    pub fn queued_panes(&self) -> usize {
        self.sender.queued_lanes()
    }
}

fn render_request(request: RenderRequest) -> Result<RenderedPane, RenderUpdate> {
    let started = Instant::now();
    let raster_view = request.camera.radar_raster_view(request.viewport);
    let options = ViewportRasterOptions {
        width: raster_view.width_px,
        height: raster_view.height_px,
        radar_x_px: raster_view.radar_x_px,
        radar_y_px: raster_view.radar_y_px,
        km_per_px_x: raster_view.km_per_px,
        km_per_px_y: raster_view.km_per_px,
    };
    let mut rgba = vec![0_u8; viewport_rgba_buffer_len(options)];

    let cache = if request.product.uses_dealiased_velocity() {
        ViewportMomentCache::new_dealiased_velocity_with_color_tables(
            &request.volume,
            request.cut_index,
            &request.color_tables,
        )
    } else {
        ViewportMomentCache::new_with_color_tables(
            &request.volume,
            request.cut_index,
            request.product.source_moment(),
            &request.color_tables,
        )
    }
    .map_err(|error| RenderUpdate::Failed {
        pane: request.pane,
        stamp: request.stamp,
        message: error.to_string(),
    })?;

    let dimensions = if request.product.is_storm_relative() {
        let direction_toward_deg =
            (request.storm_motion.direction_from_deg + 180.0).rem_euclid(360.0);
        cache.render_storm_relative_velocity_rgba_into(
            &request.volume,
            StormMotion {
                direction_deg: direction_toward_deg,
                speed_mps: request.storm_motion.speed_mps,
            },
            options,
            &mut rgba,
        )
    } else {
        cache.render_moment_rgba_into(&request.volume, options, &mut rgba)
    }
    .map_err(|error| RenderUpdate::Failed {
        pane: request.pane,
        stamp: request.stamp,
        message: error.to_string(),
    })?;

    Ok(RenderedPane {
        pane: request.pane,
        stamp: request.stamp,
        camera: request.camera,
        viewport: request.viewport,
        width: dimensions.0,
        height: dimensions.1,
        rgba,
        elapsed_ms: started.elapsed().as_secs_f32() * 1_000.0,
    })
}
