//! Radar-local map projection.
//!
//! `render2d` places a gate at plain polar coordinates: `x = range * sin(az)`,
//! `y = range * cos(az)`, with no earth model. Screen distance from the radar
//! is therefore ground distance, so the map has to answer the same question the
//! radar does — how far, and in what direction, is this point from the antenna
//! — using true geodesic distance on WGS84.
//!
//! That makes this an azimuthal-equidistant projection whose distances and
//! azimuths come from Vincenty's formulae. A spherical approximation was tried
//! first and rejected: no single radius fits both the meridional and
//! prime-vertical curvature, which left a degree of latitude 250 m long and
//! about a kilometre of drift at 460 km range. At analysis zoom that is several
//! pixels of misalignment on a county line.

use analyst_runtime::WorldPoint;

/// Bump when the transform itself changes. It is part of projection identity,
/// so a change invalidates retained geometry built by an older algorithm.
pub const PROJECTION_ALGORITHM_VERSION: u16 = 2;

/// WGS84 semi-major axis, metres.
const WGS84_A_M: f64 = 6_378_137.0;
/// WGS84 flattening.
const WGS84_F: f64 = 1.0 / 298.257_223_563;
/// WGS84 semi-minor axis, metres.
const WGS84_B_M: f64 = WGS84_A_M * (1.0 - WGS84_F);

/// Vincenty converges in a handful of steps at radar ranges; the cap only
/// guards the near-antipodal case, which is culled long before it is drawn.
const MAX_ITERATIONS: usize = 32;
const CONVERGENCE: f64 = 1e-12;

/// Identity of a projection, for the projection half of a `GeometryCacheKey`.
/// The anchor is quantised to 1e-7 degrees (about 11 mm) so the identity is
/// hashable and exact; it is provenance, never a camera cache key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProjectionId {
    pub anchor_lat_e7: i32,
    pub anchor_lon_e7: i32,
    pub algorithm_version: u16,
}

/// Geodesic azimuthal-equidistant projection anchored at a radar site.
#[derive(Clone, Copy, Debug)]
pub struct RadarProjection {
    radar_lat_deg: f64,
    radar_lon_deg: f64,
    lat1_rad: f64,
    /// Reduced latitude of the anchor, cached because every forward call needs
    /// it.
    sin_u1: f64,
    cos_u1: f64,
}

impl RadarProjection {
    pub fn new(radar_lat_deg: f64, radar_lon_deg: f64) -> Self {
        let lat = if radar_lat_deg.is_finite() {
            radar_lat_deg.clamp(-90.0, 90.0)
        } else {
            0.0
        };
        let lon = normalize_longitude(radar_lon_deg);
        let lat_rad = lat.to_radians();
        let u1 = ((1.0 - WGS84_F) * lat_rad.tan()).atan();
        Self {
            radar_lat_deg: lat,
            radar_lon_deg: lon,
            lat1_rad: lat_rad,
            sin_u1: u1.sin(),
            cos_u1: u1.cos(),
        }
    }

    pub fn radar_lat_deg(&self) -> f64 {
        self.radar_lat_deg
    }

    pub fn radar_lon_deg(&self) -> f64 {
        self.radar_lon_deg
    }

    pub fn id(&self) -> ProjectionId {
        ProjectionId {
            anchor_lat_e7: quantize_e7(self.radar_lat_deg),
            anchor_lon_e7: quantize_e7(self.radar_lon_deg),
            algorithm_version: PROJECTION_ALGORITHM_VERSION,
        }
    }

    /// Project geographic coordinates into radar-local world kilometres.
    pub fn lon_lat_to_world(&self, lon_deg: f64, lat_deg: f64) -> WorldPoint {
        let Some((distance_m, azimuth_rad)) = self.inverse_geodesic(lon_deg, lat_deg) else {
            // Non-convergent, i.e. effectively antipodal. Such a point is far
            // outside any build region; park it at the origin rather than
            // returning a NaN that would poison a vertex buffer.
            return WorldPoint::ORIGIN;
        };
        let distance_km = distance_m / 1_000.0;
        WorldPoint {
            east_km: distance_km * azimuth_rad.sin(),
            north_km: distance_km * azimuth_rad.cos(),
        }
    }

    /// Inverse of [`Self::lon_lat_to_world`], returning `(lon_deg, lat_deg)`.
    pub fn world_to_lon_lat(&self, world: WorldPoint) -> (f64, f64) {
        let distance_km = world.east_km.hypot(world.north_km);
        if distance_km < 1e-9 {
            return (self.radar_lon_deg, self.radar_lat_deg);
        }
        let azimuth_rad = world.east_km.atan2(world.north_km);
        self.direct_geodesic(distance_km * 1_000.0, azimuth_rad)
    }

    /// Vincenty inverse: geodesic distance in metres and initial azimuth in
    /// radians, measured clockwise from north at the anchor.
    fn inverse_geodesic(&self, lon_deg: f64, lat_deg: f64) -> Option<(f64, f64)> {
        if !lon_deg.is_finite() || !lat_deg.is_finite() {
            return None;
        }
        let lat2 = lat_deg.clamp(-90.0, 90.0).to_radians();
        let l = shortest_longitude_delta(lon_deg, self.radar_lon_deg).to_radians();

        let u2_reduced = ((1.0 - WGS84_F) * lat2.tan()).atan();
        let (sin_u2, cos_u2) = u2_reduced.sin_cos();
        let (sin_u1, cos_u1) = (self.sin_u1, self.cos_u1);

        let mut lambda = l;
        let mut sin_sigma = 0.0;
        let mut cos_sigma = 0.0;
        let mut sigma = 0.0;
        let mut cos_sq_alpha = 0.0;
        let mut cos_2sigma_m = 0.0;
        let mut sin_lambda = 0.0;
        let mut cos_lambda = 0.0;
        let mut converged = false;

        for _ in 0..MAX_ITERATIONS {
            let sin_cos = lambda.sin_cos();
            sin_lambda = sin_cos.0;
            cos_lambda = sin_cos.1;

            let term_a = cos_u2 * sin_lambda;
            let term_b = cos_u1 * sin_u2 - sin_u1 * cos_u2 * cos_lambda;
            sin_sigma = term_a.hypot(term_b);
            if sin_sigma == 0.0 {
                // Coincident points.
                return Some((0.0, 0.0));
            }
            cos_sigma = sin_u1 * sin_u2 + cos_u1 * cos_u2 * cos_lambda;
            sigma = sin_sigma.atan2(cos_sigma);
            let sin_alpha = cos_u1 * cos_u2 * sin_lambda / sin_sigma;
            cos_sq_alpha = 1.0 - sin_alpha * sin_alpha;
            cos_2sigma_m = if cos_sq_alpha.abs() < f64::EPSILON {
                // Equatorial line: cos(2 sigma_m) is undefined, and zero is the
                // value that makes the series below reduce correctly.
                0.0
            } else {
                cos_sigma - 2.0 * sin_u1 * sin_u2 / cos_sq_alpha
            };
            let c = WGS84_F / 16.0 * cos_sq_alpha * (4.0 + WGS84_F * (4.0 - 3.0 * cos_sq_alpha));
            let previous = lambda;
            lambda = l
                + (1.0 - c)
                    * WGS84_F
                    * sin_alpha
                    * (sigma
                        + c * sin_sigma
                            * (cos_2sigma_m
                                + c * cos_sigma * (-1.0 + 2.0 * cos_2sigma_m * cos_2sigma_m)));
            if (lambda - previous).abs() < CONVERGENCE {
                converged = true;
                break;
            }
        }
        if !converged {
            return None;
        }

        let u_sq = cos_sq_alpha * (WGS84_A_M * WGS84_A_M - WGS84_B_M * WGS84_B_M)
            / (WGS84_B_M * WGS84_B_M);
        let a_series =
            1.0 + u_sq / 16384.0 * (4096.0 + u_sq * (-768.0 + u_sq * (320.0 - 175.0 * u_sq)));
        let b_series = u_sq / 1024.0 * (256.0 + u_sq * (-128.0 + u_sq * (74.0 - 47.0 * u_sq)));
        let delta_sigma = b_series
            * sin_sigma
            * (cos_2sigma_m
                + b_series / 4.0
                    * (cos_sigma * (-1.0 + 2.0 * cos_2sigma_m * cos_2sigma_m)
                        - b_series / 6.0
                            * cos_2sigma_m
                            * (-3.0 + 4.0 * sin_sigma * sin_sigma)
                            * (-3.0 + 4.0 * cos_2sigma_m * cos_2sigma_m)));

        let distance_m = WGS84_B_M * a_series * (sigma - delta_sigma);
        let azimuth = (cos_u2 * sin_lambda).atan2(cos_u1 * sin_u2 - sin_u1 * cos_u2 * cos_lambda);
        Some((distance_m, azimuth))
    }

    /// Vincenty direct: the point `distance_m` along `azimuth_rad` from the
    /// anchor, returned as `(lon_deg, lat_deg)`.
    fn direct_geodesic(&self, distance_m: f64, azimuth_rad: f64) -> (f64, f64) {
        let (sin_alpha1, cos_alpha1) = azimuth_rad.sin_cos();
        let tan_u1 = self.sin_u1 / self.cos_u1.max(f64::MIN_POSITIVE);
        let sigma1 = tan_u1.atan2(cos_alpha1);
        let sin_alpha = self.cos_u1 * sin_alpha1;
        let cos_sq_alpha = 1.0 - sin_alpha * sin_alpha;
        let u_sq = cos_sq_alpha * (WGS84_A_M * WGS84_A_M - WGS84_B_M * WGS84_B_M)
            / (WGS84_B_M * WGS84_B_M);
        let a_series =
            1.0 + u_sq / 16384.0 * (4096.0 + u_sq * (-768.0 + u_sq * (320.0 - 175.0 * u_sq)));
        let b_series = u_sq / 1024.0 * (256.0 + u_sq * (-128.0 + u_sq * (74.0 - 47.0 * u_sq)));

        let sigma_initial = distance_m / (WGS84_B_M * a_series);
        let mut sigma = sigma_initial;
        let mut cos_2sigma_m = 0.0;
        for _ in 0..MAX_ITERATIONS {
            cos_2sigma_m = (2.0 * sigma1 + sigma).cos();
            let (sin_sigma, cos_sigma) = sigma.sin_cos();
            let delta_sigma = b_series
                * sin_sigma
                * (cos_2sigma_m
                    + b_series / 4.0
                        * (cos_sigma * (-1.0 + 2.0 * cos_2sigma_m * cos_2sigma_m)
                            - b_series / 6.0
                                * cos_2sigma_m
                                * (-3.0 + 4.0 * sin_sigma * sin_sigma)
                                * (-3.0 + 4.0 * cos_2sigma_m * cos_2sigma_m)));
            let previous = sigma;
            sigma = sigma_initial + delta_sigma;
            if (sigma - previous).abs() < CONVERGENCE {
                break;
            }
        }

        let (sin_sigma, cos_sigma) = sigma.sin_cos();
        let temp = self.sin_u1 * sin_sigma - self.cos_u1 * cos_sigma * cos_alpha1;
        let lat2 = (self.sin_u1 * cos_sigma + self.cos_u1 * sin_sigma * cos_alpha1)
            .atan2((1.0 - WGS84_F) * (sin_alpha * sin_alpha + temp * temp).sqrt());
        let lambda = (sin_sigma * sin_alpha1)
            .atan2(self.cos_u1 * cos_sigma - self.sin_u1 * sin_sigma * cos_alpha1);
        let c = WGS84_F / 16.0 * cos_sq_alpha * (4.0 + WGS84_F * (4.0 - 3.0 * cos_sq_alpha));
        let l = lambda
            - (1.0 - c)
                * WGS84_F
                * sin_alpha
                * (sigma
                    + c * sin_sigma
                        * (cos_2sigma_m
                            + c * cos_sigma * (-1.0 + 2.0 * cos_2sigma_m * cos_2sigma_m)));

        (
            normalize_longitude(self.radar_lon_deg + l.to_degrees()),
            lat2.to_degrees().clamp(-90.0, 90.0),
        )
    }

    /// Anchor latitude in radians, for callers needing the raw value.
    pub fn anchor_lat_rad(&self) -> f64 {
        self.lat1_rad
    }
}

/// Wrap to (-180, 180].
fn normalize_longitude(lon_deg: f64) -> f64 {
    if !lon_deg.is_finite() {
        return 0.0;
    }
    let mut lon = (lon_deg + 180.0).rem_euclid(360.0) - 180.0;
    if lon <= -180.0 {
        lon += 360.0;
    }
    lon
}

/// Signed shortest angular distance from `from` to `to`, so a site beside the
/// antimeridian does not project its neighbours a full turn away.
fn shortest_longitude_delta(to_deg: f64, from_deg: f64) -> f64 {
    normalize_longitude(to_deg - from_deg)
}

fn quantize_e7(degrees: f64) -> i32 {
    let scaled = (degrees * 1e7).round();
    scaled.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sites the app is actually exercised against, plus antimeridian cases.
    const SITES: &[(&str, f64, f64)] = &[
        ("KTLX", 35.3333, -97.2778),
        ("KRTX", 45.7150, -122.9650),
        ("KAKQ", 36.9840, -77.0074),
        ("KAPX", 44.9072, -84.7198),
        ("PAKC", 58.6794, -156.6294),
        ("synthetic antimeridian", 51.8800, 179.9500),
    ];

    #[test]
    fn round_trips_within_a_millimetre_across_the_radar_footprint() {
        for (name, lat, lon) in SITES {
            let projection = RadarProjection::new(*lat, *lon);
            for range_km in [0.0_f64, 1.0, 50.0, 230.0, 460.0, 1_000.0] {
                for azimuth_deg in (0..360).step_by(15) {
                    let azimuth = f64::from(azimuth_deg).to_radians();
                    let world = WorldPoint::new(range_km * azimuth.sin(), range_km * azimuth.cos());
                    let (lon_deg, lat_deg) = projection.world_to_lon_lat(world);
                    let back = projection.lon_lat_to_world(lon_deg, lat_deg);
                    let error_km =
                        (back.east_km - world.east_km).hypot(back.north_km - world.north_km);
                    assert!(
                        error_km < 1e-6,
                        "{name}: round trip drifted {error_km} km at {range_km} km / {azimuth_deg}deg"
                    );
                }
            }
        }
    }

    #[test]
    fn the_anchor_projects_to_the_world_origin() {
        for (name, lat, lon) in SITES {
            let projection = RadarProjection::new(*lat, *lon);
            let origin = projection.lon_lat_to_world(*lon, *lat);
            assert!(
                origin.east_km.abs() < 1e-9 && origin.north_km.abs() < 1e-9,
                "{name}: anchor projected to {origin:?}"
            );
        }
    }

    /// Vincenty's own published test line, as redistributed by Geoscience
    /// Australia: Flinders Peak to Buninyong, 54 972.271 m, initial azimuth
    /// 306 deg 52' 05.37". This checks the geodesic engine against an external
    /// authority rather than against arithmetic of our own.
    /// Degrees, minutes, seconds to decimal degrees. The published case is
    /// specified in DMS; converting here rather than transcribing decimals
    /// keeps the reference exact to the millimetre.
    fn dms(degrees: f64, minutes: f64, seconds: f64) -> f64 {
        degrees + minutes / 60.0 + seconds / 3_600.0
    }

    #[test]
    fn matches_the_published_vincenty_test_line() {
        // Flinders Peak 37 57 03.72030 S, 144 25 29.52440 E.
        let projection =
            RadarProjection::new(-dms(37.0, 57.0, 3.72030), dms(144.0, 25.0, 29.52440));
        // Buninyong 37 39 10.15610 S, 143 55 35.38390 E.
        let world =
            projection.lon_lat_to_world(dms(143.0, 55.0, 35.38390), -dms(37.0, 39.0, 10.15610));

        let distance_m = world.east_km.hypot(world.north_km) * 1_000.0;
        assert!(
            (distance_m - 54_972.271).abs() < 0.001,
            "distance was {distance_m} m, expected 54972.271 m"
        );

        let azimuth_deg = world
            .east_km
            .atan2(world.north_km)
            .to_degrees()
            .rem_euclid(360.0);
        let expected_deg = 306.0 + 52.0 / 60.0 + 5.37 / 3_600.0;
        assert!(
            (azimuth_deg - expected_deg).abs() < 1e-6,
            "azimuth was {azimuth_deg} deg, expected {expected_deg} deg"
        );
    }

    /// The WGS84 meridian arc from 35.3333N to 36.3333N, evaluated from the
    /// standard series expansion, is 110.9559 km. A fixed-radius sphere gives
    /// 111.195 km here, which is the error this projection exists to avoid.
    #[test]
    fn a_degree_of_latitude_matches_the_wgs84_meridian_arc() {
        let projection = RadarProjection::new(35.3333, -97.2778);
        let world = projection.lon_lat_to_world(-97.2778, 36.3333);
        assert!(
            (world.north_km - 110.9559).abs() < 0.001,
            "meridian distance was {} km, expected 110.9559 km",
            world.north_km
        );
        assert!(
            world.east_km.abs() < 1e-6,
            "due north should have no easting"
        );
    }

    #[test]
    fn due_east_and_due_north_land_on_the_expected_axes() {
        let projection = RadarProjection::new(35.3333, -97.2778);
        let north = projection.world_to_lon_lat(WorldPoint::new(0.0, 100.0));
        assert!(
            (north.0 - -97.2778).abs() < 1e-9,
            "due north kept longitude"
        );
        assert!(north.1 > 35.3333, "due north increased latitude");

        let east = projection.world_to_lon_lat(WorldPoint::new(100.0, 0.0));
        assert!(east.0 > -97.2778, "due east increased longitude");
    }

    #[test]
    fn crossing_the_antimeridian_stays_local() {
        let projection = RadarProjection::new(51.88, 179.95);
        // A tenth of a degree east is across the antimeridian at -179.95.
        let world = projection.lon_lat_to_world(-179.95, 51.88);
        assert!(
            world.east_km > 0.0 && world.east_km < 20.0,
            "expected a short eastward hop, got {world:?}"
        );
        // And the inverse must come back across rather than wrapping the globe.
        let (lon, _lat) = projection.world_to_lon_lat(world);
        assert!((lon - -179.95).abs() < 1e-6, "inverse returned {lon}");
    }

    #[test]
    fn non_finite_input_cannot_poison_a_vertex_buffer() {
        let projection = RadarProjection::new(35.3333, -97.2778);
        for (lon, lat) in [(f64::NAN, 35.0), (-97.0, f64::INFINITY)] {
            let world = projection.lon_lat_to_world(lon, lat);
            assert!(world.east_km.is_finite() && world.north_km.is_finite());
        }
    }

    #[test]
    fn identity_tracks_the_anchor_and_the_algorithm_version() {
        let a = RadarProjection::new(35.3333, -97.2778);
        let b = RadarProjection::new(35.3333, -97.2778);
        let c = RadarProjection::new(45.7150, -122.9650);
        assert_eq!(a.id(), b.id());
        assert_ne!(a.id(), c.id());
        assert_eq!(a.id().algorithm_version, PROJECTION_ALGORITHM_VERSION);
    }

    #[test]
    fn longitude_normalizes_into_the_expected_half_open_range() {
        assert_eq!(normalize_longitude(180.0), 180.0);
        assert_eq!(normalize_longitude(-180.0), 180.0);
        assert_eq!(normalize_longitude(190.0), -170.0);
        assert_eq!(normalize_longitude(-190.0), 170.0);
    }
}
