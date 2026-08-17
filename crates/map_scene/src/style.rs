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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerStyle {
    pub color: LayerColor,
    /// Stroke width in screen pixels, held constant as the camera zooms.
    pub width_px: f32,
    /// Coarsest scale, in kilometres per point, at which the layer is drawn.
    /// Counties disappear when zoomed far out instead of turning the pane into
    /// a grey mat.
    pub max_km_per_point: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapStyle {
    pub country: LayerStyle,
    pub state: LayerStyle,
    pub county: LayerStyle,
}

impl Default for MapStyle {
    fn default() -> Self {
        Self {
            country: LayerStyle {
                color: LayerColor::rgba(0.62, 0.68, 0.78, 0.95),
                width_px: 1.6,
                max_km_per_point: f32::MAX,
            },
            state: LayerStyle {
                color: LayerColor::rgba(0.52, 0.58, 0.68, 0.85),
                width_px: 1.2,
                max_km_per_point: 8.0,
            },
            county: LayerStyle {
                color: LayerColor::rgba(0.34, 0.38, 0.45, 0.75),
                width_px: 0.9,
                max_km_per_point: 1.2,
            },
        }
    }
}

impl MapStyle {
    pub fn layer(&self, layer: MapLayer) -> LayerStyle {
        match layer {
            MapLayer::Country => self.country,
            MapLayer::StateProvince => self.state,
            MapLayer::County => self.county,
        }
    }

    /// Whether the layer is drawn at this camera scale.
    pub fn is_visible(&self, layer: MapLayer, km_per_point: f32) -> bool {
        km_per_point <= self.layer(layer).max_km_per_point
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counties_drop_out_before_states_and_countries() {
        let style = MapStyle::default();
        // Zoomed in tight: everything is drawn.
        assert!(style.is_visible(MapLayer::County, 0.2));
        assert!(style.is_visible(MapLayer::StateProvince, 0.2));
        assert!(style.is_visible(MapLayer::Country, 0.2));

        // Mid scale: counties are gone, states remain.
        assert!(!style.is_visible(MapLayer::County, 4.0));
        assert!(style.is_visible(MapLayer::StateProvince, 4.0));

        // Continental: only countries.
        assert!(!style.is_visible(MapLayer::StateProvince, 40.0));
        assert!(style.is_visible(MapLayer::Country, 40.0));
    }

    #[test]
    fn widths_are_positive_for_every_layer() {
        let style = MapStyle::default();
        for layer in MapLayer::ALL {
            assert!(style.layer(layer).width_px > 0.0, "{}", layer.label());
        }
    }
}
