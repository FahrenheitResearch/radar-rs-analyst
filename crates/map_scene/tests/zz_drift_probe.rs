//! TEMPORARY diagnostic probe. Deleted before hand-off.
use analyst_runtime::{Camera2D, Generation, GeometryCacheKey, LodBucket, ScreenPoint, ViewportMetrics};
use map_scene::{MapBuildRequest, MapDataset, MapStyle, RadarProjection, build_geometry};

fn view() -> ViewportMetrics {
    ViewportMetrics { width_points: 1600.0, height_points: 900.0, pixels_per_point: 1.0 }
}

#[test]
fn probe() {
    let p = RadarProjection::new(35.33304977416992, -97.27774810791016);
    let dataset = MapDataset::from_generated(Generation::new(1));

    // J) Douglas-Peucker displacement between adjacent LOD buckets, in SCREEN
    //    points at the camera scale each bucket is used at. Does geography slide
    //    under the (fixed) site markers when the bucket flips?
    println!("J) bucket, camera kpp, vertices, screen-space simplification budget");
    for b in [-4i16, -2, 0, 2, 4, 6, 8, 10, 12, 14] {
        let lod = LodBucket(b);
        let key = GeometryCacheKey { dataset: Generation::new(1), projection: Generation::new(1), style: Generation::new(1), lod };
        let g = build_geometry(&MapBuildRequest { key, dataset: dataset.clone(), projection: p, style: MapStyle::default() });
        println!("   {b:>3} kpp {:>7.3} retained {:>7} culled {:>5} labels {:>5} verts {:>8}",
            lod.center_scale(0.35), g.stats.retained_points, g.stats.features_culled, g.labels.len(), g.vertex_count());
    }

    // K) the zoom ceiling, with the REAL default camera and one-notch steps.
    let f_out = analyst_runtime::zoom_factor_for_notches(-1.0);
    let f_in = analyst_runtime::zoom_factor_for_notches(1.0);
    let anchor = ScreenPoint::new(1200.0, 300.0);
    let mut cam = Camera2D::default();
    let mut saturated_at = None;
    for n in 1..=40 {
        let before = cam.km_per_point;
        cam.zoom_about(f_out, anchor, view());
        if cam.km_per_point == before && saturated_at.is_none() { saturated_at = Some(n); }
    }
    println!("K) from the default camera, one notch at a time: the wheel stops having any effect at notch {saturated_at:?} (scale {:.3} km/pt)", cam.km_per_point);
    println!("   at that scale a 1600-point pane spans {:.0} km; the earth is 40008 km round", 1600.0 * cam.km_per_point);
    // now scroll back in the same number of notches
    for _ in 0..40 { cam.zoom_about(f_in, anchor, view()); }
    println!("   40 out then 40 in returns to {:.5} km/pt and centre ({:.0},{:.0}) km, not 0.35 and (0,0)",
        cam.km_per_point, cam.center_east_km, cam.center_north_km);
}
