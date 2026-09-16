//! Web Mercator, from degrees or metres onto integer grids.
//!
//! The only floating point in the geometry pipeline; everything downstream is
//! integers. Nothing is lost: the maxzoom grid is the finest the tileset can
//! express anyway. Maxzoom 22 is a 2^34 grid, well inside an f64 mantissa.

/// Project WGS84 lon/lat (degrees) onto the global grid for zoom `z`, which is
/// `extent << z` units across, origin top-left, y down.
pub fn project_global(lon: f64, lat: f64, z: u32, extent: u32) -> (i64, i64) {
    // Normalised world coordinates, 0..1, origin top-left.
    let world_x = (lon + 180.0) / 360.0;
    let lat_rad = lat.to_radians();
    let world_y = 0.5 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / (2.0 * std::f64::consts::PI);

    let world_units = f64::from(extent) * (1u64 << z) as f64;

    (
        (world_x * world_units).round() as i64,
        (world_y * world_units).round() as i64,
    )
}

/// Half the width of the Web Mercator world in metres, `π × 6378137`. EPSG:3857
/// spans ±this on both axes: exactly the square the grid covers.
pub const WORLD_METRES: f64 = 20_037_508.342_789_244;

/// Project EPSG:3857 metres onto the same grid as [`project_global`].
///
/// EPSG:3857 is the grid's own system in metres, so this is just a shift and a
/// scale; the two agree to within the last rounded unit. Northing runs up and
/// the grid down, hence the subtraction.
pub fn project_global_metres(x: f64, y: f64, z: u32, extent: u32) -> (i64, i64) {
    // Normalised world coordinates, 0..1, origin top-left.
    let world_x = (x + WORLD_METRES) / (2.0 * WORLD_METRES);
    let world_y = (WORLD_METRES - y) / (2.0 * WORLD_METRES);

    let world_units = f64::from(extent) * (1u64 << z) as f64;

    (
        (world_x * world_units).round() as i64,
        (world_y * world_units).round() as i64,
    )
}

/// The coordinate systems a source may be in. Both are Web Mercator — the
/// sphere it's defined on and the plane it projects to — so neither needs a
/// projection database. Anything else is refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Crs {
    /// EPSG:4326 (equivalently OGC:CRS84): lon/lat in degrees.
    #[default]
    Wgs84,
    /// EPSG:3857: easting/northing in metres.
    WebMercator,
}

impl Crs {
    /// The EPSG code `code`, if it is one of the two that can be read.
    pub fn from_epsg(code: i64) -> Option<Self> {
        match code {
            // 4979 is 4326 in 3D. Formats hand both over as lon/lat.
            4326 | 4979 => Some(Crs::Wgs84),
            // 3785 and 900913 are older codes for 3857.
            3857 | 3785 | 900_913 => Some(Crs::WebMercator),
            _ => None,
        }
    }

    /// Project a coordinate in this CRS onto the global grid.
    pub fn project_global(self, x: f64, y: f64, z: u32, extent: u32) -> (i64, i64) {
        match self {
            Crs::Wgs84 => project_global(x, y, z, extent),
            Crs::WebMercator => project_global_metres(x, y, z, extent),
        }
    }
}

/// The inverse of [`project_global`], to within the grid's resolution. Needed
/// because a tileset declares its bounds in lon/lat.
pub fn unproject_global(x: i64, y: i64, z: u32, extent: u32) -> (f64, f64) {
    let world_units = f64::from(extent) * (1u64 << z) as f64;
    let lon = x as f64 / world_units * 360.0 - 180.0;

    // Inverting `0.5 - ln(tan(lat) + sec(lat)) / 2π`: the inner term is
    // `asinh(tan(lat))`, so latitude comes back through `sinh`.
    let world_y = y as f64 / world_units;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * world_y))
        .sinh()
        .atan()
        .to_degrees();
    (lon, lat)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The latitude where the Web Mercator projection is truncated.
    const MAX_LAT: f64 = 85.051_128_779_806_59;

    #[test]
    fn the_global_grid_spans_extent_shifted_by_zoom() {
        // At zoom 3 the world is 8 tiles across, so 8 * 4096 units.
        assert_eq!(project_global(0.0, 0.0, 3, 4096), (16384, 16384));
        assert_eq!(project_global(-180.0, MAX_LAT, 3, 4096), (0, 0));
        assert_eq!(project_global(180.0, -MAX_LAT, 3, 4096), (32768, 32768));
    }

    #[test]
    fn unprojecting_returns_the_coordinate_that_was_projected() {
        // To within a unit: about 2 mm at z14, so 0.001° is loose but still
        // catches a wrong constant or a flipped axis.
        for &(lon, lat) in &[
            (0.0, 0.0),
            (174.7, -41.3),
            (-122.4, 37.8),
            (13.4, 52.5),
            (179.9, -85.0),
            (-179.9, 85.0),
        ] {
            for z in [0, 3, 10, 14] {
                let (x, y) = project_global(lon, lat, z, 4096);
                let (back_lon, back_lat) = unproject_global(x, y, z, 4096);
                let tolerance = 360.0 / (4096.0 * (1u64 << z) as f64);
                assert!(
                    (back_lon - lon).abs() <= tolerance,
                    "lon {lon} at zoom {z} came back as {back_lon}"
                );
                assert!(
                    (back_lat - lat).abs() <= tolerance.max(1e-9) * 2.0,
                    "lat {lat} at zoom {z} came back as {back_lat}"
                );
            }
        }
    }

    /// EPSG:3857's defining formula, so the metres don't come from the code
    /// under test.
    fn to_metres(lon: f64, lat: f64) -> (f64, f64) {
        const R: f64 = 6_378_137.0;
        let y = R * ((45.0 + lat / 2.0).to_radians().tan()).ln();
        (R * lon.to_radians(), y)
    }

    #[test]
    fn the_world_in_metres_spans_the_same_grid_as_the_world_in_degrees() {
        assert_eq!(project_global_metres(0.0, 0.0, 3, 4096), (16384, 16384));
        assert_eq!(
            project_global_metres(-WORLD_METRES, WORLD_METRES, 3, 4096),
            (0, 0)
        );
        assert_eq!(
            project_global_metres(WORLD_METRES, -WORLD_METRES, 3, 4096),
            (32768, 32768)
        );
    }

    #[test]
    fn a_coordinate_lands_on_the_same_grid_unit_in_either_crs() {
        for (lon, lat) in [
            (0.0, 0.0),
            (174.7, -41.3),
            (-122.4, 37.8),
            (13.4, 52.5),
            (179.9, -85.0),
            (-179.9, 85.0),
        ] {
            let (east, north) = to_metres(lon, lat);
            for z in [0, 3, 10, 14] {
                let degrees = project_global(lon, lat, z, 4096);
                let metres = project_global_metres(east, north, z, 4096);
                // Float rounding can straddle a unit boundary, never more.
                assert!(
                    (degrees.0 - metres.0).abs() <= 1 && (degrees.1 - metres.1).abs() <= 1,
                    "({lon}, {lat}) at zoom {z}: {degrees:?} in degrees, {metres:?} in metres"
                );
            }
        }
    }

    #[test]
    fn the_two_readable_crs_are_the_two_forms_of_web_mercator() {
        assert_eq!(Crs::from_epsg(4326), Some(Crs::Wgs84));
        assert_eq!(Crs::from_epsg(3857), Some(Crs::WebMercator));
        assert_eq!(Crs::from_epsg(900_913), Some(Crs::WebMercator));
        // NZTM needs a datum shift, so it's refused.
        assert_eq!(Crs::from_epsg(2193), None);
        assert_eq!(Crs::from_epsg(27700), None);
    }

    #[test]
    fn a_crs_projects_the_way_its_own_function_does() {
        let (east, north) = to_metres(174.7, -41.3);
        assert_eq!(
            Crs::Wgs84.project_global(174.7, -41.3, 12, 4096),
            project_global(174.7, -41.3, 12, 4096)
        );
        assert_eq!(
            Crs::WebMercator.project_global(east, north, 12, 4096),
            project_global_metres(east, north, 12, 4096)
        );
    }

    #[test]
    fn the_corners_of_the_world_unproject_to_the_corners_of_the_world() {
        let span = 1i64 << 3;
        let world = span * 4096;
        let (lon, lat) = unproject_global(0, 0, 3, 4096);
        assert!((lon + 180.0).abs() < 1e-9, "west edge came back at {lon}");
        assert!(
            (lat - MAX_LAT).abs() < 1e-9,
            "north edge came back at {lat}"
        );

        let (lon, lat) = unproject_global(world, world, 3, 4096);
        assert!((lon - 180.0).abs() < 1e-9, "east edge came back at {lon}");
        assert!(
            (lat + MAX_LAT).abs() < 1e-9,
            "south edge came back at {lat}"
        );
    }

    #[test]
    fn y_increases_southwards() {
        let (_, north) = project_global(0.0, 45.0, 0, 4096);
        let (_, south) = project_global(0.0, -45.0, 0, 4096);
        assert!(north < south, "northern y {north}, southern y {south}");
    }
}
