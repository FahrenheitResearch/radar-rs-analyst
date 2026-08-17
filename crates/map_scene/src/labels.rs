//! Bounded label placement.
//!
//! Every candidate is already projected into world kilometres by the geometry
//! build, so placement never touches geographic coordinates. The pass is hard
//! bounded in candidates inspected and labels accepted, and it is deterministic:
//! the same camera and geometry always produce the same labels in the same
//! order, so text does not shimmer as the view moves.

use analyst_runtime::{Camera2D, ScreenPoint, ViewportMetrics};

use crate::dataset::LabelClass;
use crate::geometry::{MapGeometry, ProjectedLabel};

/// Hard ceiling on candidates examined per pane per frame.
pub const MAX_CANDIDATES_INSPECTED: usize = 4_000;
/// Hard ceiling on labels drawn per pane.
pub const MAX_LABELS_PLACED: usize = 64;
/// Approximate glyph box used for overlap rejection, in screen points.
const CHARACTER_WIDTH_POINTS: f32 = 6.0;
const LINE_HEIGHT_POINTS: f32 = 13.0;
/// Padding around a placed label so text does not crowd.
const PADDING_POINTS: f32 = 3.0;

/// A label that survived placement, in pane-local screen points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacedLabel {
    pub name: &'static str,
    pub position: ScreenPoint,
    pub class: LabelClass,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlacementMetrics {
    pub inspected: usize,
    pub placed: usize,
    pub rejected_offscreen: usize,
    pub rejected_overlap: usize,
    pub budget_exhausted: bool,
}

#[derive(Clone, Copy, Debug)]
struct Box2D {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl Box2D {
    fn intersects(self, other: Self) -> bool {
        self.min_x < other.max_x
            && other.min_x < self.max_x
            && self.min_y < other.max_y
            && other.min_y < self.max_y
    }
}

/// Choose which labels to draw for one pane.
pub fn place_labels(
    geometry: &MapGeometry,
    camera: Camera2D,
    viewport: ViewportMetrics,
    max_labels: usize,
) -> (Vec<PlacedLabel>, PlacementMetrics) {
    let viewport = viewport.sanitized();
    let mut metrics = PlacementMetrics::default();
    let mut placed: Vec<PlacedLabel> = Vec::new();
    let mut occupied: Vec<Box2D> = Vec::new();
    let limit = max_labels.min(MAX_LABELS_PLACED);

    // Candidates arrive in dataset order, which is stable; ranking by
    // (class, rank) keeps the important ones when the budget runs out.
    let mut candidates: Vec<&ProjectedLabel> = geometry
        .labels
        .iter()
        .take(MAX_CANDIDATES_INSPECTED)
        .collect();
    candidates.sort_by_key(|label| (label.class, label.rank, label.name));

    for candidate in candidates {
        if placed.len() >= limit {
            metrics.budget_exhausted = true;
            break;
        }
        metrics.inspected += 1;

        let world = analyst_runtime::WorldPoint::new(
            f64::from(candidate.east_km),
            f64::from(candidate.north_km),
        );
        let screen = camera.world_to_screen(world, viewport);
        if screen.x < 0.0
            || screen.y < 0.0
            || screen.x > viewport.width_points
            || screen.y > viewport.height_points
        {
            metrics.rejected_offscreen += 1;
            continue;
        }

        let half_width =
            candidate.name.chars().count() as f32 * CHARACTER_WIDTH_POINTS * 0.5 + PADDING_POINTS;
        let half_height = LINE_HEIGHT_POINTS * 0.5 + PADDING_POINTS;
        let bounds = Box2D {
            min_x: screen.x - half_width,
            min_y: screen.y - half_height,
            max_x: screen.x + half_width,
            max_y: screen.y + half_height,
        };
        if occupied.iter().any(|other| other.intersects(bounds)) {
            metrics.rejected_overlap += 1;
            continue;
        }

        occupied.push(bounds);
        placed.push(PlacedLabel {
            name: candidate.name,
            position: screen,
            class: candidate.class,
        });
    }

    metrics.placed = placed.len();
    (placed, metrics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::{Generation, GeometryCacheKey, LodBucket};

    use crate::geometry::{GeometryStats, MapGeometry};

    fn geometry(labels: Vec<ProjectedLabel>) -> MapGeometry {
        MapGeometry::new(
            GeometryCacheKey {
                dataset: Generation::new(1),
                projection: Generation::new(1),
                style: Generation::new(1),
                lod: LodBucket(0),
            },
            Vec::new(),
            Vec::new(),
            Vec::new(),
            labels,
            GeometryStats::default(),
        )
    }

    fn label(name: &'static str, east: f32, north: f32, rank: u8) -> ProjectedLabel {
        ProjectedLabel {
            class: LabelClass::Place,
            name,
            east_km: east,
            north_km: north,
            rank,
        }
    }

    fn viewport() -> ViewportMetrics {
        ViewportMetrics {
            width_points: 800.0,
            height_points: 600.0,
            pixels_per_point: 1.0,
        }
    }

    fn camera() -> Camera2D {
        Camera2D {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: 1.0,
            rotation_rad: 0.0,
        }
    }

    #[test]
    fn labels_outside_the_pane_are_rejected() {
        let scene = geometry(vec![
            label("Inside", 0.0, 0.0, 0),
            label("FarEast", 5_000.0, 0.0, 0),
        ]);
        let (placed, metrics) = place_labels(&scene, camera(), viewport(), 64);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].name, "Inside");
        assert_eq!(metrics.rejected_offscreen, 1);
    }

    #[test]
    fn overlapping_labels_lose_to_the_better_ranked_one() {
        let scene = geometry(vec![
            label("Important", 0.0, 0.0, 0),
            label("Crowding", 1.0, 1.0, 9),
        ]);
        let (placed, metrics) = place_labels(&scene, camera(), viewport(), 64);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].name, "Important", "rank must decide");
        assert_eq!(metrics.rejected_overlap, 1);
    }

    #[test]
    fn placement_is_bounded_however_many_candidates_arrive() {
        // Spread far enough apart that only the budget can stop them.
        let labels: Vec<ProjectedLabel> = (0..5_000)
            .map(|index| {
                label(
                    "Town",
                    (index % 50) as f32 * 8.0 - 200.0,
                    (index / 50) as f32 * 8.0 - 200.0,
                    0,
                )
            })
            .collect();
        let scene = geometry(labels);
        let (placed, metrics) = place_labels(&scene, camera(), viewport(), MAX_LABELS_PLACED);
        assert!(placed.len() <= MAX_LABELS_PLACED);
        assert!(metrics.inspected <= MAX_CANDIDATES_INSPECTED);
        assert!(metrics.budget_exhausted);
    }

    #[test]
    fn placement_is_deterministic_for_the_same_camera() {
        let labels: Vec<ProjectedLabel> = (0..200)
            .map(|index| {
                label(
                    "Place",
                    (index % 20) as f32 * 15.0 - 150.0,
                    (index / 20) as f32 * 15.0,
                    3,
                )
            })
            .collect();
        let scene = geometry(labels);
        let first = place_labels(&scene, camera(), viewport(), 32).0;
        let second = place_labels(&scene, camera(), viewport(), 32).0;
        assert_eq!(first, second, "the same view must place the same labels");
    }

    #[test]
    fn an_empty_scene_places_nothing() {
        let scene = geometry(Vec::new());
        let (placed, metrics) = place_labels(&scene, camera(), viewport(), 64);
        assert!(placed.is_empty());
        assert_eq!(metrics.placed, 0);
    }
}
