//! Layer styling and its generation.
//!
//! Style is a geometry input, not a paint-time decision: line width in screen
//! pixels decides how a line is expanded into triangles, and layer visibility
//! decides whether it is built at all. A style change therefore invalidates
//! retained geometry, which is why the style generation is part of the
//! geometry cache key.

use crate::dataset::MapLayer;

/// RGBA, straight alpha, sRGB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl LayerColor {
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

/// Scale at or below which counties replace state outlines.
pub const COUNTY_DETAIL_KM_PER_POINT: f32 = 1.2;
/// Scale above which only country outlines remain.
pub const COUNTRY_ONLY_KM_PER_POINT: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerStyle {
    pub color: LayerColor,
    /// Stroke width in screen pixels, held constant as the camera zooms.
    pub width_px: f32,
    /// Finest scale, in kilometres per point, at which the layer is drawn.
    /// Coarser US levels switch off once a finer one covers the same ground,
    /// which is what stops one shoreline being drawn twice.
    pub min_km_per_point: f32,
    /// Coarsest scale at which the layer is drawn.
    pub max_km_per_point: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapStyle {
    pub country: LayerStyle,
    pub foreign_admin: LayerStyle,
    pub state: LayerStyle,
    pub county: LayerStyle,
}

impl Default for MapStyle {
    fn default() -> Self {
        Self {
            // Country outlines take over only once state lines stop.
            country: LayerStyle {
                color: LayerColor::rgba(0.62, 0.68, 0.78, 0.95),
                width_px: 1.6,
                min_km_per_point: COUNTRY_ONLY_KM_PER_POINT,
                max_km_per_point: f32::MAX,
            },
            // Foreign provinces have no finer level to collide with, so they
            // stay on wherever they would be legible.
            foreign_admin: LayerStyle {
                color: LayerColor::rgba(0.52, 0.58, 0.68, 0.85),
                width_px: 1.2,
                min_km_per_point: 0.0,
                max_km_per_point: COUNTRY_ONLY_KM_PER_POINT,
            },
            state: LayerStyle {
                color: LayerColor::rgba(0.52, 0.58, 0.68, 0.85),
                width_px: 1.2,
                min_km_per_point: COUNTY_DETAIL_KM_PER_POINT,
                max_km_per_point: COUNTRY_ONLY_KM_PER_POINT,
            },
            county: LayerStyle {
                color: LayerColor::rgba(0.42, 0.47, 0.55, 0.85),
                width_px: 0.9,
                min_km_per_point: 0.0,
                max_km_per_point: COUNTY_DETAIL_KM_PER_POINT,
            },
        }
    }
}

impl MapStyle {
    pub fn layer(&self, layer: MapLayer) -> LayerStyle {
        match layer {
            MapLayer::Country => self.country,
            MapLayer::ForeignAdmin => self.foreign_admin,
            MapLayer::State => self.state,
            MapLayer::County => self.county,
        }
    }

    /// Whether the layer is drawn at this camera scale.
    pub fn is_visible(&self, layer: MapLayer, km_per_point: f32) -> bool {
        let style = self.layer(layer);
        km_per_point <= style.max_km_per_point && km_per_point > style.min_km_per_point
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly one US boundary level may be visible at a time. Two of them
    /// together draw the same shoreline twice, a pixel apart, which is what
    /// made the map look like several overlapping basemaps.
    #[test]
    fn only_one_us_level_is_visible_at_any_scale() {
        let style = MapStyle::default();
        for km_per_point in [
            0.05_f32, 0.2, 0.35, 1.0, 1.19, 1.21, 3.0, 7.9, 8.1, 40.0, 500.0,
        ] {
            let us_levels = [MapLayer::County, MapLayer::State, MapLayer::Country]
                .into_iter()
                .filter(|layer| style.is_visible(*layer, km_per_point))
                .count();
            assert_eq!(
                us_levels, 1,
                "{us_levels} US boundary levels drawn at {km_per_point} km/point"
            );
        }
    }

    #[test]
    fn detail_decreases_as_the_camera_pulls_back() {
        let style = MapStyle::default();
        assert!(
            style.is_visible(MapLayer::County, 0.2),
            "counties when close"
        );
        assert!(
            style.is_visible(MapLayer::State, 4.0),
            "states at mid scale"
        );
        assert!(
            style.is_visible(MapLayer::Country, 40.0),
            "countries when far out"
        );
        assert!(!style.is_visible(MapLayer::County, 4.0));
        assert!(!style.is_visible(MapLayer::State, 40.0));
    }

    #[test]
    fn foreign_admin_stays_on_where_counties_cannot_replace_it() {
        // Canada and Mexico have no county-level table, so their provinces
        // must remain visible at close zoom or a border radar loses context.
        let style = MapStyle::default();
        assert!(style.is_visible(MapLayer::ForeignAdmin, 0.2));
        assert!(style.is_visible(MapLayer::ForeignAdmin, 4.0));
        assert!(!style.is_visible(MapLayer::ForeignAdmin, 40.0));
    }

    #[test]
    fn widths_are_positive_for_every_layer() {
        let style = MapStyle::default();
        for layer in MapLayer::ALL {
            assert!(style.layer(layer).width_px > 0.0, "{}", layer.label());
        }
    }
}
