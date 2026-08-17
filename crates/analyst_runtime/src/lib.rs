//! UI-independent runtime contracts for the radar workstation.
//!
//! This crate owns serializable user intent and pure scheduling/history policy.
//! It deliberately does not depend on egui, network clients, decoders, GPU
//! handles, or application panels.

#![forbid(unsafe_code)]

mod generation;
mod history;
mod jobs;
mod view;
mod workspace;

pub use generation::{Generation, GenerationClock, RenderStamp, SceneStamp};
pub use history::{
    FrameIdentity, FrameOrigin, FrameStage, HistoryPolicy, InstallDisposition, InstallReport,
    PlaybackState, VolumeFrame, VolumeHistory, estimate_radar_volume_bytes,
};
pub use jobs::{
    LatestLaneReceiver, LatestLaneSender, SendClosed, SubmitOutcome, latest_lane_channel,
};
pub use view::{
    Camera2D, DEFAULT_KM_PER_POINT, GeometryCacheKey, LodBucket, LodSelector, MAX_KM_PER_POINT,
    MIN_KM_PER_POINT, RasterView, ScreenPoint, ViewportMetrics, WorldPoint,
};
pub use workspace::{
    MAX_PANES, PaneId, PaneIntent, PaneLayout, PaneLinkGroups, SmoothingMode, StormMotionIntent,
    TiltSelection, WorkspaceState,
};
