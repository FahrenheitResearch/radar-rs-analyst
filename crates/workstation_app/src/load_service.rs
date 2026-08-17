use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::Instant;

use analyst_runtime::{FrameStage, Generation, LatestLaneSender, latest_lane_channel};
use eframe::egui;
use radar_core::RadarVolume;

const LOAD_LANE: u8 = 0;
const MIN_PREVIEW_RADIALS: usize = 180;
const RESULT_QUEUE_CAPACITY: usize = 8;

pub struct LoadRequest {
    pub generation: Generation,
    pub path: PathBuf,
}

pub struct LoadedVolume {
    pub generation: Generation,
    pub path: PathBuf,
    pub stage: FrameStage,
    pub volume: Arc<RadarVolume>,
    pub elapsed_ms: f32,
}

pub enum LoadUpdate {
    Started {
        generation: Generation,
        path: PathBuf,
    },
    Volume(LoadedVolume),
    Failed {
        generation: Generation,
        path: PathBuf,
        message: String,
    },
}

pub struct LoadService {
    sender: LatestLaneSender<u8, LoadRequest>,
    receiver: Receiver<LoadUpdate>,
}

impl LoadService {
    pub fn new(context: egui::Context) -> Self {
        let (request_sender, request_receiver) = latest_lane_channel::<u8, LoadRequest>();
        let (result_sender, result_receiver) = mpsc::sync_channel(RESULT_QUEUE_CAPACITY);
        let _worker = thread::Builder::new()
            .name("radar-workstation-load".to_owned())
            .spawn(move || {
                while let Some((_lane, request)) = request_receiver.recv() {
                    process_request(request, &result_sender, &context);
                }
            })
            .expect("failed to start radar load worker");

        Self {
            sender: request_sender,
            receiver: result_receiver,
        }
    }

    pub fn request(&self, request: LoadRequest) -> Result<(), LoadRequest> {
        self.sender
            .submit(LOAD_LANE, request)
            .map(|_| ())
            .map_err(|closed| closed.0)
    }

    pub fn try_recv(&self) -> Option<LoadUpdate> {
        match self.receiver.try_recv() {
            Ok(update) => Some(update),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

fn process_request(request: LoadRequest, sender: &SyncSender<LoadUpdate>, context: &egui::Context) {
    let generation = request.generation;
    let path = request.path;
    let _ = sender.send(LoadUpdate::Started {
        generation,
        path: path.clone(),
    });
    context.request_repaint();

    let started = Instant::now();
    let result = std::fs::read(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))
        .and_then(|raw| decode_with_previews(&raw, generation, &path, started, sender, context));

    match result {
        Ok(mut volume) => {
            volume.metadata.source_path = Some(path.display().to_string());
            let _ = sender.send(LoadUpdate::Volume(LoadedVolume {
                generation,
                path,
                stage: FrameStage::Complete,
                volume: Arc::new(volume),
                elapsed_ms: started.elapsed().as_secs_f32() * 1_000.0,
            }));
        }
        Err(message) => {
            let _ = sender.send(LoadUpdate::Failed {
                generation,
                path,
                message,
            });
        }
    }
    context.request_repaint();
}

fn decode_with_previews(
    raw: &[u8],
    generation: Generation,
    path: &Path,
    started: Instant,
    sender: &SyncSender<LoadUpdate>,
    context: &egui::Context,
) -> Result<RadarVolume, String> {
    let mut publish_preview = |mut preview: RadarVolume| {
        preview.metadata.source_path = Some(path.display().to_string());
        let update = LoadUpdate::Volume(LoadedVolume {
            generation,
            path: path.to_path_buf(),
            stage: FrameStage::Preview,
            volume: Arc::new(preview),
            elapsed_ms: started.elapsed().as_secs_f32() * 1_000.0,
        });
        if sender.try_send(update).is_ok() {
            context.request_repaint();
        }
    };

    if raw.starts_with(&[0x1f, 0x8b]) {
        nexrad_io::decode_gzip_volume_from_bytes_with_preview(
            raw,
            MIN_PREVIEW_RADIALS,
            &mut publish_preview,
        )
        .map_err(|error| error.to_string())
    } else {
        nexrad_io::decode_volume_from_bytes_with_bzip_preview(
            raw,
            MIN_PREVIEW_RADIALS,
            &mut publish_preview,
        )
        .map_err(|error| error.to_string())
    }
}
