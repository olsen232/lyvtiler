//! A layer of regular angles (eighths and sixths of a turn) keeps them: left
//! to choose, lyv goes deeper than z12 when z12's grid would bend them, and
//! only then.

const EXTENT: u32 = 4096;

fn options(dir: &tempfile::TempDir, source: &str, max_zoom: Option<u32>) -> lyv::Options {
    let input = dir.path().join("in.geojson");
    std::fs::write(&input, source).unwrap();
    let output = dir.path().join("out.pmtiles");
    lyv::Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom,
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "l".to_string(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        input_crs: None,
        regular_angles_check: true,
        extent: EXTENT,
        maxzoom_extent: Some(8192),
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        dust: true,
        threads: 1,
        keep_store: false,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        limits: lyv::Limits {
            bytes: 0,
            features: 0,
        },
        max_tile_vertices: 0,
        buffer: 0,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    }
}

/// The deepest zoom a build holds.
fn maxzoom(source: &str, max_zoom: Option<u32>) -> u32 {
    maxzoom_checking(source, max_zoom, true)
}

fn maxzoom_checking(source: &str, max_zoom: Option<u32>, check: bool) -> u32 {
    let dir = tempfile::tempdir().unwrap();
    let options = lyv::Options {
        regular_angles_check: check,
        ..options(&dir, source, max_zoom)
    };
    let tiles = lyv::Collected::from(&options).unwrap();
    tiles.tiles.keys().map(|coord| coord.z).max().unwrap()
}

/// `count` polygons around Wellington, each `ring` given its centre in
/// degrees and its index.
fn layer(count: usize, ring: impl Fn(f64, f64, usize) -> Vec<(f64, f64)>) -> String {
    let features: Vec<String> = (0..count)
        .map(|i| {
            let (x, y) = (174.7 + (i % 20) as f64 * 0.002, -41.3 + (i / 20) as f64 * 0.002);
            let points: Vec<String> = ring(x, y, i)
                .iter()
                .map(|(x, y)| format!("[{x},{y}]"))
                .collect();
            format!(
                r#"{{"type":"Feature","id":{i},"properties":{{}},"geometry":{{"type":"Polygon","coordinates":[[{}]]}}}}"#,
                points.join(",")
            )
        })
        .collect();
    format!(
        r#"{{"type":"FeatureCollection","features":[{}]}}"#,
        features.join(",")
    )
}

/// Metres to degrees around latitude 41.3, as a (lon, lat) offset.
fn metres(east: f64, north: f64) -> (f64, f64) {
    (
        east / (111_320.0 * 41.3f64.to_radians().cos()),
        north / 110_540.0,
    )
}

/// A building: a 6 by 9 m rectangle turned 25 degrees off the grid.
fn building(x: f64, y: f64, _: usize) -> Vec<(f64, f64)> {
    let (sin, cos) = 25f64.to_radians().sin_cos();
    let mut ring: Vec<(f64, f64)> = [(0.0, 0.0), (6.0, 0.0), (6.0, 9.0), (0.0, 9.0), (0.0, 0.0)]
        .iter()
        .map(|&(e, n)| {
            let (dx, dy) = metres(e * cos - n * sin, e * sin + n * cos);
            (x + dx, y + dy)
        })
        .collect();
    ring.dedup();
    ring
}

/// A blob of the same size: twelve vertices at uneven angles and radii.
fn blob(x: f64, y: f64, i: usize) -> Vec<(f64, f64)> {
    let mut ring: Vec<(f64, f64)> = (0..12)
        .map(|k| {
            let wobble = ((i * 7 + k * 13) % 11) as f64 / 11.0;
            let angle = (k as f64 + 0.37 * wobble) * std::f64::consts::TAU / 12.0;
            let radius = 4.0 + 2.0 * wobble;
            let (dx, dy) = metres(radius * angle.cos(), radius * angle.sin());
            (x + dx, y + dy)
        })
        .collect();
    ring.push(ring[0]);
    ring
}

#[test]
fn buildings_raise_the_maxzoom_until_their_corners_survive() {
    // At z12 a unit is about a metre, and 6 m walls bend.
    assert!(maxzoom(&layer(200, building), None) > 12);
}

/// A hexagon 4 m across the flats, whose turns are all sixths of a turn.
fn hexagon(x: f64, y: f64, _: usize) -> Vec<(f64, f64)> {
    let mut ring: Vec<(f64, f64)> = (0..=6)
        .map(|k| {
            let angle = k as f64 * std::f64::consts::TAU / 6.0;
            let (dx, dy) = metres(2.3 * angle.cos(), 2.3 * angle.sin());
            (x + dx, y + dy)
        })
        .collect();
    ring[6] = ring[0];
    ring
}

#[test]
fn hexagons_raise_the_maxzoom_too() {
    assert!(maxzoom(&layer(200, hexagon), None) > 12);
}

#[test]
fn shapes_without_structure_stay_at_the_floor() {
    assert_eq!(maxzoom(&layer(200, blob), None), 12);
}

#[test]
fn a_given_maxzoom_is_kept_whatever_the_structure() {
    assert_eq!(maxzoom(&layer(200, building), Some(12)), 12);
}

#[test]
fn the_check_can_be_turned_off() {
    assert_eq!(maxzoom_checking(&layer(200, building), None, false), 12);
}
