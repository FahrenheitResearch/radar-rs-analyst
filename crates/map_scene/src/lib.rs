//! Retained world-space map scene.
//!
//! The scene owns geographic source data, the radar-local projection, the
//! off-thread geometry build, and the GPU resources that draw it. Its central
//! rule is that retained geometry is identified by dataset, projection, style
//! and LOD bucket alone. Camera centre and exact scale are deliberately absent,
//! so panning and small zooms reuse the geometry that is already resident and
//! cost only a uniform update.

pub mod build;
pub mod dataset;
pub mod generated;
pub mod geometry;
pub mod gpu;
pub mod labels;
pub mod projection;
pub mod residency;
pub mod scene;
pub mod style;

pub use build::{
    LOD_REFERENCE_KM_PER_POINT, MAX_BUILD_HALF_EXTENT_KM, MIN_BUILD_HALF_EXTENT_KM,
    MapBuildRequest, bucket_for_scale, build_geometry, build_half_extent_km,
};
pub use dataset::{
    GeoLineFeature, GeoPolygonFeature, LabelCandidate, LabelClass, MapDataset, MapLayer,
};
pub use geometry::{GeometryStats, MapDraw, MapGeometry, MapVertex, ProjectedLabel};
pub use labels::{MAX_LABELS_PLACED, PlacedLabel, PlacementMetrics, place_labels};
pub use projection::{PROJECTION_ALGORITHM_VERSION, ProjectionId, RadarProjection};
pub use residency::{Admission, GeometryResidency, ResidencyMetrics};
pub use scene::{MapSceneController, SceneMetrics};
pub use style::{LayerColor, LayerStyle, MapStyle};
