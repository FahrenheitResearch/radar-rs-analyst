use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Duration;

use analyst_runtime::{
    FrameStage, Generation, LatestLaneReceiver, LatestLaneSender, latest_lane_channel,
};
use chrono::{DateTime, Utc};
use data_source::RealtimeLevel2Volume;
use eframe::egui;

const COMMAND_LANE: u8 = 0;
const RESULT_QUEUE_CAPACITY: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_millis(1_200);
const COMMAND_CHECK_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Eq, PartialEq)]
struct VolumeFingerprint {
    site: String,
    volume_id: u16,
    volume_time: DateTime<Utc>,
    chunk_count: usize,
    complete: bool,
    total_size: u64,
}

impl From<&RealtimeLevel2Volume> for VolumeFingerprint {
    fn from(volume: &RealtimeLevel2Volume) -> Self {
        Self {
            site: volume.site.clone(),
            volume_id: volume.volume_id,
            volume_time: volume.volume_time,
            chunk_count: volume.chunks.len(),
            complete: volume.complete,
            total_size: volume.total_size,
        }
    }
}

struct LiveSession {
    generation: Generation,
    site: String,
    cache_dir: PathBuf,
    last_fingerprint: Option<VolumeFingerprint>,
    last_error: Option<String>,
}

enum LiveCommand {
    Start {
        generation: Generation,
        site: String,
        cache_dir: PathBuf,
    },
    Stop,
}

pub enum LiveUpdate {
    Started {
        generation: Generation,
        site: String,
    },
    VolumeReady {
        generation: Generation,
        site: String,
        path: PathBuf,
        stage: FrameStage,
        volume_time: DateTime<Utc>,
        chunk_count: usize,
        total_size: u64,
        cache_hit: bool,
    },
    Failed {
        generation: Generation,
        site: String,
        message: String,
    },
    Stopped,
}

pub struct LiveService {
    sender: LatestLaneSender<u8, LiveCommand>,
    receiver: Receiver<LiveUpdate>,
}

impl LiveService {
    pub fn new(context: egui::Context) -> Self {
        let (command_sender, command_receiver) = latest_lane_channel::<u8, LiveCommand>();
        let (result_sender, result_receiver) = mpsc::sync_channel(RESULT_QUEUE_CAPACITY);
        let _worker = thread::Builder::new()
            .name("radar-workstation-live".to_owned())
            .spawn(move || run_worker(command_receiver, result_sender, context))
            .expect("failed to start live Level II worker");
        Self {
            sender: command_sender,
            receiver: result_receiver,
        }
    }

    pub fn start(
        &self,
        generation: Generation,
        site: impl Into<String>,
        cache_dir: PathBuf,
    ) -> Result<(), String> {
        let site = normalize_site(site.into())?;
        self.sender
            .submit(
                COMMAND_LANE,
                LiveCommand::Start {
                    generation,
                    site,
                    cache_dir,
                },
            )
            .map(|_| ())
            .map_err(|_| "live source worker is closed".to_owned())
    }

    pub fn stop(&self) {
        let _ = self.sender.submit(COMMAND_LANE, LiveCommand::Stop);
    }

    pub fn try_recv(&self) -> Option<LiveUpdate> {
        self.receiver.try_recv().ok()
    }
}

pub fn default_live_cache_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path)
            .join("FahrenheitResearch")
            .join("RadarWorkstation")
            .join("cache")
            .join("level2-live");
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path)
            .join("radar-workstation")
            .join("level2-live");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("radar-workstation")
            .join("level2-live");
    }
    std::env::temp_dir()
        .join("radar-workstation")
        .join("level2-live")
}

fn run_worker(
    commands: LatestLaneReceiver<u8, LiveCommand>,
    results: SyncSender<LiveUpdate>,
    context: egui::Context,
) {
    let mut session: Option<LiveSession> = None;
    loop {
        let command = if session.is_some() {
            commands.try_recv().map(|(_, command)| command)
        } else {
            commands.recv().map(|(_, command)| command)
        };
        if let Some(command) = command {
            apply_command(command, &mut session, &results, &context);
        } else if session.is_none() {
            break;
        }

        let Some(active) = session.as_mut() else {
            continue;
        };
        poll_session(active, &results, &context);

        let checks = (POLL_INTERVAL.as_millis() / COMMAND_CHECK_INTERVAL.as_millis()).max(1);
        for _ in 0..checks {
            thread::sleep(COMMAND_CHECK_INTERVAL);
            if let Some((_lane, command)) = commands.try_recv() {
                apply_command(command, &mut session, &results, &context);
                break;
            }
        }
    }
}

fn apply_command(
    command: LiveCommand,
    session: &mut Option<LiveSession>,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) {
    match command {
        LiveCommand::Start {
            generation,
            site,
            cache_dir,
        } => {
            *session = Some(LiveSession {
                generation,
                site: site.clone(),
                cache_dir,
                last_fingerprint: None,
                last_error: None,
            });
            let _ = results.try_send(LiveUpdate::Started { generation, site });
        }
        LiveCommand::Stop => {
            *session = None;
            let _ = results.try_send(LiveUpdate::Stopped);
        }
    }
    context.request_repaint();
}

fn poll_session(
    session: &mut LiveSession,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) {
    let volume = match data_source::latest_realtime_level2_volume(&session.site) {
        Ok(volume) => volume,
        Err(error) => {
            publish_error(session, error.to_string(), results, context);
            return;
        }
    };
    let fingerprint = VolumeFingerprint::from(&volume);
    if session.last_fingerprint.as_ref() == Some(&fingerprint) {
        session.last_error = None;
        return;
    }

    let downloaded = match data_source::download_realtime_volume(&volume, &session.cache_dir) {
        Ok(downloaded) => downloaded,
        Err(error) => {
            publish_error(session, error.to_string(), results, context);
            return;
        }
    };

    let stage = if volume.complete {
        FrameStage::Complete
    } else {
        FrameStage::Partial
    };
    let update = LiveUpdate::VolumeReady {
        generation: session.generation,
        site: session.site.clone(),
        path: downloaded.path,
        stage,
        volume_time: volume.volume_time,
        chunk_count: volume.chunks.len(),
        total_size: volume.total_size,
        cache_hit: downloaded.cache_hit,
    };
    if results.send(update).is_ok() {
        session.last_fingerprint = Some(fingerprint);
        session.last_error = None;
        context.request_repaint();
    }
}

fn publish_error(
    session: &mut LiveSession,
    message: String,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) {
    if session.last_error.as_deref() == Some(message.as_str()) {
        return;
    }
    session.last_error = Some(message.clone());
    let _ = results.try_send(LiveUpdate::Failed {
        generation: session.generation,
        site: session.site.clone(),
        message,
    });
    context.request_repaint();
}

fn normalize_site(site: String) -> Result<String, String> {
    let site = site.trim().to_ascii_uppercase();
    if site.len() != 4 || !site.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("radar site must be a four-character Level II identifier".to_owned());
    }
    Ok(site)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_ids_are_normalized_and_validated() {
        assert_eq!(normalize_site(" krtx ".to_owned()).unwrap(), "KRTX");
        assert!(normalize_site("RTX".to_owned()).is_err());
        assert!(normalize_site("KR/X".to_owned()).is_err());
    }

    #[test]
    fn fingerprint_changes_when_partial_volume_grows() {
        let base = VolumeFingerprint {
            site: "KRTX".to_owned(),
            volume_id: 4,
            volume_time: Utc::now(),
            chunk_count: 3,
            complete: false,
            total_size: 1_000,
        };
        assert_ne!(
            base,
            VolumeFingerprint {
                chunk_count: 4,
                total_size: 1_400,
                ..base.clone()
            }
        );
    }
}
