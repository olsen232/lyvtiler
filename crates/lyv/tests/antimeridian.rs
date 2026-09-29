//! Longitude wraps: a tile in the last column takes its eastern margin from
//! the first. Latitude doesn't wrap.

use lyv::Options;
use lyv_core::{Local, TileCoord};

const EXTENT: u32 = 4096;
const E: Local = EXTENT as Local;
const BUFFER: u32 = 5;

/// Two small polygons either side of the antimeridian, each inside the other's
/// margin, at one latitude so no north or south neighbour can supply them.
/// Polygons, since lines aren't buffered; separate features, so they don't
/// weld.
fn source() -> &'static str {
    r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":1,"properties":{},"geometry":{"type":"Polygon",
         "coordinates":[[[179.0,-10.5],[180.0,-10.5],[180.0,-9.5],[179.0,-9.5],
                         [179.0,-10.5]]]}},
        {"type":"Feature","id":2,"properties":{},"geometry":{"type":"Polygon",
         "coordinates":[[[-180.0,-10.5],[-179.0,-10.5],[-179.0,-9.5],
                         [-180.0,-9.5],[-180.0,-10.5]]]}}
    ]}"#
}

struct Built {
    _dir: tempfile::TempDir,
    #[allow(dead_code)]
    options: Options,
    tiles: lyv::Collected,
}

fn build(max_zoom: u32, buffer: u32) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.geojson");
    std::fs::write(&input, source()).unwrap();
    let output = dir.path().join("out.pmtiles");

    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(max_zoom),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "lines".to_string(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        input_crs: None,
        extent: EXTENT,
        maxzoom_extent: None,
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        dust: true,
        threads: 1,
        // Kept, since these tests read the store.
        keep_store: true,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        // No limits: these tests check exact geometry.
        limits: lyv::Limits {
            bytes: 0,
            features: 0,
        },
        // No vertex budget, which would coarsen the geometry.
        max_tile_vertices: 0,
        buffer,
        simplification: lyv_simplify::Tolerance::NONE,
    };
    // Collected: finished tiles go straight to the archive.
    let mut tiles = lyv::Collected::default();
    lyv::run_into(&options, &mut tiles, &lyv::Progress::silent()).expect("the pipeline should run");
    Built {
        _dir: dir,
        options,
        tiles,
    }
}

/// Every x coordinate of an encoded tile.
fn xs(built: &Built, z: u32, x: u32, y: u32) -> Vec<Local> {
    let data = built
        .tiles
        .tiles
        .get(&TileCoord { z, x, y })
        .unwrap_or_else(|| panic!("tile {z}/{x}/{y} is missing"));
    lyv_mvt::decode_chains(data)
        .unwrap()
        .iter()
        .flat_map(|c| c.points.iter().map(|p| p.x))
        .collect()
}

/// The row holding latitude -10 at the given zoom, found rather than assumed.
fn row(built: &Built, z: u32) -> u32 {
    let mut rows: Vec<u32> = addresses(built, z).into_iter().map(|(_, y)| y).collect();
    rows.sort_unstable();
    rows.dedup();
    assert_eq!(rows.len(), 1, "the fixture should occupy a single row");
    rows[0]
}

/// The tiles that exist at a zoom (only the two columns by the antimeridian).
fn addresses(built: &Built, z: u32) -> Vec<(u32, u32)> {
    built
        .tiles
        .tiles
        .keys()
        .filter(|coord| coord.z == z)
        .map(|coord| (coord.x, coord.y))
        .collect()
}

#[test]
fn the_last_column_borrows_its_eastern_margin_from_the_first() {
    let built = build(2, BUFFER);
    let y = row(&built, 2);

    // Feature 2 is in column 0; from column 3 it's just past the eastern edge.
    let east_of_the_edge: Vec<Local> = xs(&built, 2, 3, y).into_iter().filter(|&x| x > E).collect();
    assert!(
        !east_of_the_edge.is_empty(),
        "column 3 has no geometry past its eastern edge, so nothing came \
         across the antimeridian"
    );
}

#[test]
fn the_first_column_borrows_its_western_margin_from_the_last() {
    let built = build(2, BUFFER);
    let y = row(&built, 2);

    let west_of_the_edge: Vec<Local> = xs(&built, 2, 0, y).into_iter().filter(|&x| x < 0).collect();
    assert!(
        !west_of_the_edge.is_empty(),
        "column 0 has no geometry before its western edge"
    );
}

#[test]
fn a_wrapped_margin_stays_within_the_margin() {
    // A neighbour is placed one tile away whatever the wrap did, never a whole
    // world away.
    let built = build(2, BUFFER);
    let margin = lyv_buffer::margin_units(BUFFER, EXTENT);

    for (x, y) in addresses(&built, 2) {
        for point in xs(&built, 2, x, y) {
            assert!(
                (-margin..=E + margin).contains(&point),
                "x = {point} in tile 2/{x}/{y} is beyond the {margin}-unit margin"
            );
        }
    }
}

#[test]
fn the_root_tile_wraps_onto_itself() {
    // At zoom 0 the single tile is its own neighbour both ways.
    let built = build(2, BUFFER);

    let root = xs(&built, 0, 0, 0);
    assert!(
        root.iter().any(|&x| x > E),
        "the root tile has nothing past its eastern edge"
    );
    assert!(
        root.iter().any(|&x| x < 0),
        "the root tile has nothing before its western edge"
    );
}

#[test]
fn without_a_buffer_nothing_wraps() {
    // With no margin, tiles stay exactly clipped, antimeridian included.
    let built = build(2, 0);

    for (x, y) in addresses(&built, 2) {
        for point in xs(&built, 2, x, y) {
            assert!(
                (0..=E).contains(&point),
                "x = {point} in tile 2/{x}/{y} escaped an unbuffered tile"
            );
        }
    }
}
