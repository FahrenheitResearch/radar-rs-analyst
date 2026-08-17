use radar_core::ProductId;
use serde::{Deserialize, Serialize};

use crate::Camera2D;

pub const MAX_PANES: usize = 4;
const PANE_0: PaneId = PaneId(0);
const PANE_1: PaneId = PaneId(1);
const PANE_2: PaneId = PaneId(2);
const PANE_3: PaneId = PaneId(3);
const ONE_PANE: &[PaneId] = &[PANE_0];
const TWO_PANES: &[PaneId] = &[PANE_0, PANE_1];
const FOUR_PANES: &[PaneId] = &[PANE_0, PANE_1, PANE_2, PANE_3];

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneId(u8);

impl PaneId {
    pub const fn new(index: u8) -> Option<Self> {
        if index < MAX_PANES as u8 {
            Some(Self(index))
        } else {
            None
        }
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PaneLayout {
    One,
    TwoHorizontal,
    TwoVertical,
    Four,
}

impl PaneLayout {
    pub const fn visible_panes(self) -> &'static [PaneId] {
        match self {
            Self::One => ONE_PANE,
            Self::TwoHorizontal | Self::TwoVertical => TWO_PANES,
            Self::Four => FOUR_PANES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TiltSelection {
    LowestAvailable,
    NearestElevationTenths(i16),
    CutIndex(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SmoothingMode {
    Nearest,
    Linear,
    HighQuality,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StormMotionIntent {
    /// Meteorological direction from which the storm moves.
    pub direction_from_deg: f32,
    pub speed_mps: f32,
}

impl Default for StormMotionIntent {
    fn default() -> Self {
        Self {
            direction_from_deg: 240.0,
            speed_mps: 15.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneLinkGroups {
    pub camera: Option<u8>,
    pub timeline: Option<u8>,
    pub tilt: Option<u8>,
    pub product: Option<u8>,
    pub cursor: Option<u8>,
}

impl Default for PaneLinkGroups {
    fn default() -> Self {
        Self {
            camera: Some(0),
            timeline: Some(0),
            tilt: None,
            product: None,
            cursor: Some(0),
        }
    }
}

/// Serializable user intent for one pane. Runtime-only textures, worker state,
/// and caches live outside this type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneIntent {
    pub product: ProductId,
    pub tilt: TiltSelection,
    pub camera: Camera2D,
    pub opacity: u8,
    pub smoothing: SmoothingMode,
    pub storm_motion: StormMotionIntent,
    pub links: PaneLinkGroups,
    pub overlays_visible: bool,
}

impl Default for PaneIntent {
    fn default() -> Self {
        Self {
            product: ProductId("REF".to_owned()),
            tilt: TiltSelection::LowestAvailable,
            camera: Camera2D::default(),
            opacity: u8::MAX,
            smoothing: SmoothingMode::Linear,
            storm_motion: StormMotionIntent::default(),
            links: PaneLinkGroups::default(),
            overlays_visible: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub layout: PaneLayout,
    pub active_pane: PaneId,
    pub panes: [PaneIntent; MAX_PANES],
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            layout: PaneLayout::One,
            active_pane: PANE_0,
            panes: std::array::from_fn(|_| PaneIntent::default()),
        }
    }
}

impl WorkspaceState {
    pub fn visible_panes(&self) -> &'static [PaneId] {
        self.layout.visible_panes()
    }

    pub fn pane(&self, id: PaneId) -> &PaneIntent {
        &self.panes[id.index()]
    }

    pub fn pane_mut(&mut self, id: PaneId) -> &mut PaneIntent {
        &mut self.panes[id.index()]
    }

    pub fn active(&self) -> &PaneIntent {
        self.pane(self.active_pane)
    }

    pub fn active_mut(&mut self) -> &mut PaneIntent {
        self.pane_mut(self.active_pane)
    }

    pub fn set_layout(&mut self, layout: PaneLayout) {
        self.layout = layout;
        if !self.visible_panes().contains(&self.active_pane) {
            self.active_pane = self.visible_panes()[0];
        }
    }

    pub fn set_active(&mut self, pane: PaneId) -> bool {
        if !self.visible_panes().contains(&pane) {
            return false;
        }
        self.active_pane = pane;
        true
    }

    pub fn cycle_active(&mut self, delta: isize) -> PaneId {
        let visible = self.visible_panes();
        let current = visible
            .iter()
            .position(|pane| *pane == self.active_pane)
            .unwrap_or(0) as isize;
        let len = visible.len() as isize;
        let next = (current + delta).rem_euclid(len) as usize;
        self.active_pane = visible[next];
        self.active_pane
    }

    /// Install a camera change from one pane and propagate it to panes in the
    /// same camera link group. Returns every pane whose intent changed.
    pub fn apply_camera_from(&mut self, source: PaneId, camera: Camera2D) -> Vec<PaneId> {
        let source_group = self.pane(source).links.camera;
        let mut changed = Vec::new();
        for index in 0..MAX_PANES {
            let pane = PaneId(index as u8);
            let linked = pane == source
                || source_group.is_some_and(|group| self.pane(pane).links.camera == Some(group));
            if linked && self.pane(pane).camera != camera {
                self.pane_mut(pane).camera = camera;
                changed.push(pane);
            }
        }
        changed
    }

    pub fn apply_product_from(&mut self, source: PaneId, product: ProductId) -> Vec<PaneId> {
        let source_group = self.pane(source).links.product;
        let mut changed = Vec::new();
        for index in 0..MAX_PANES {
            let pane = PaneId(index as u8);
            let linked = pane == source
                || source_group.is_some_and(|group| self.pane(pane).links.product == Some(group));
            if linked && self.pane(pane).product != product {
                self.pane_mut(pane).product = product.clone();
                changed.push(pane);
            }
        }
        changed
    }

    pub fn apply_tilt_from(&mut self, source: PaneId, tilt: TiltSelection) -> Vec<PaneId> {
        let source_group = self.pane(source).links.tilt;
        let mut changed = Vec::new();
        for index in 0..MAX_PANES {
            let pane = PaneId(index as u8);
            let linked = pane == source
                || source_group.is_some_and(|group| self.pane(pane).links.tilt == Some(group));
            if linked && self.pane(pane).tilt != tilt {
                self.pane_mut(pane).tilt = tilt;
                changed.push(pane);
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_limits_active_panes() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        assert!(workspace.set_active(PANE_3));
        workspace.set_layout(PaneLayout::One);
        assert_eq!(workspace.active_pane, PANE_0);
        assert!(!workspace.set_active(PANE_1));
    }

    #[test]
    fn camera_propagates_only_inside_link_group() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        workspace.pane_mut(PANE_2).links.camera = Some(1);
        workspace.pane_mut(PANE_3).links.camera = None;
        let camera = Camera2D {
            center_east_km: 70.0,
            center_north_km: -20.0,
            ..Camera2D::default()
        };
        let changed = workspace.apply_camera_from(PANE_0, camera);
        assert_eq!(changed, vec![PANE_0, PANE_1]);
        assert_eq!(workspace.pane(PANE_0).camera, camera);
        assert_eq!(workspace.pane(PANE_1).camera, camera);
        assert_ne!(workspace.pane(PANE_2).camera, camera);
        assert_ne!(workspace.pane(PANE_3).camera, camera);
    }

    #[test]
    fn product_links_are_independent_from_camera_links() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        workspace.pane_mut(PANE_0).links.product = Some(4);
        workspace.pane_mut(PANE_2).links.product = Some(4);
        let changed = workspace.apply_product_from(PANE_0, ProductId("VEL".to_owned()));
        assert_eq!(changed, vec![PANE_0, PANE_2]);
        assert_eq!(workspace.pane(PANE_1).product.0, "REF");
    }

    #[test]
    fn active_pane_cycles_within_visible_layout() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::TwoVertical);
        assert_eq!(workspace.cycle_active(1), PANE_1);
        assert_eq!(workspace.cycle_active(1), PANE_0);
        assert_eq!(workspace.cycle_active(-1), PANE_1);
    }
}
