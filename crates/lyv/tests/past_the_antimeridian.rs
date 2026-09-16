//! Data running past longitude 180, as sources near the antimeridian often
//! write it (New Zealand's Chathams at 183.5, say). The part past 180 wraps to
//! the first columns, and the archive declares bounds a client can use.

use lyv::Options;
use lyv_core::{Local, TileCoord};

const EXTENT: u32 = 4096;

fn options(dir: &tempfile::TempDir, source: &str, max_zoom: u32) -> Options {
    let input = dir.path().join("in.geojson");
    std::fs::write(&input, source).unwrap();
    let output = dir.path().join("out.pmtiles");
    Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(max_zoom),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "l".to_string(),
        output_name: None,
        attributes: None,
        extent: EXTENT,
        maxzoom_extent: None,
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        dust: true,
        threads: 1,
        keep_store: false,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        // No limits: these tests count what arrives.
        limits: lyv::Limits {
            bytes: 0,
            features: 0,
        },
        max_tile_vertices: 0,
        buffer: 0,
        simplification: lyv_simplify::Tolerance::NONE,
    }
}

fn tiles(source: &str, max_zoom: u32) -> lyv::Collected {
    let dir = tempfile::tempdir().unwrap();
    lyv::Collected::from(&options(&dir, source, max_zoom)).unwrap()
}

fn collection(features: &[&str]) -> String {
    format!(
        r#"{{"type":"FeatureCollection","features":[{}]}}"#,
        features.join(",")
    )
}

fn point(id: u64, lon: f64, lat: f64) -> String {
    format!(
        r#"{{"type":"Feature","id":{id},"properties":{{}},"geometry":{{"type":"Point","coordinates":[{lon},{lat}]}}}}"#
    )
}

fn polygon(id: u64, west: f64, south: f64, east: f64, north: f64) -> String {
    format!(
        r#"{{"type":"Feature","id":{id},"properties":{{}},"geometry":{{"type":"Polygon","coordinates":[[[{west},{south}],[{east},{south}],[{east},{north}],[{west},{north}],[{west},{south}]]]}}}}"#
    )
}

/// Which features each column holds at a zoom.
fn features_by_column(tiles: &lyv::Collected, z: u32) -> Vec<(u32, u64)> {
    let mut found: Vec<(u32, u64)> = tiles
        .at(z)
        .flat_map(|(coord, data)| {
            lyv_mvt::decode_chains(data)
                .unwrap()
                .into_iter()
                .map(move |chain| (coord.x, chain.feature))
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// Doubled shoelace area of every ring at a zoom, in tile units.
fn area_at(tiles: &lyv::Collected, z: u32) -> i64 {
    tiles
        .at(z)
        .flat_map(|(_, data)| lyv_mvt::decode_chains(data).unwrap())
        .map(|chain| lyv_seam::double_area(&chain.points))
        .sum()
}

#[test]
fn a_point_past_180_lands_in_the_first_column() {
    let tiles = tiles(
        &collection(&[&point(1, 175.0, -40.0), &point(2, 185.0, -40.0)]),
        4,
    );
    assert_eq!(features_by_column(&tiles, 4), vec![(0, 2), (15, 1)]);
}

#[test]
fn a_polygon_across_180_keeps_its_eastern_half() {
    // The same box, once across the antimeridian and once clear of it: the
    // same area in tile units, since Mercator x is linear in longitude.
    let across = tiles(&collection(&[&polygon(1, 178.0, -41.0, 182.0, -39.0)]), 4);
    let clear = tiles(&collection(&[&polygon(1, 170.0, -41.0, 174.0, -39.0)]), 4);
    assert_eq!(features_by_column(&across, 4), vec![(0, 1), (15, 1)]);
    assert_eq!(area_at(&across, 4), area_at(&clear, 4));
}

#[test]
fn tiles_a_polygon_swallows_past_180_are_filled() {
    // Shifted by half the world, which at z4 is exactly eight columns: every
    // tile, squares and all, must come out the same.
    let across = tiles(&collection(&[&polygon(1, 150.0, -50.0, 210.0, -10.0)]), 4);
    let clear = tiles(&collection(&[&polygon(1, -30.0, -50.0, 30.0, -10.0)]), 4);
    let at_z4 = |tiles: &lyv::Collected, shift: u32| -> Vec<(TileCoord, Vec<u8>)> {
        let mut out: Vec<(TileCoord, Vec<u8>)> = tiles
            .at(4)
            .map(|(coord, data)| {
                let x = (coord.x + shift) % 16;
                (TileCoord { x, ..coord }, data.clone())
            })
            .collect();
        out.sort_by_key(|(coord, _)| (coord.x, coord.y));
        out
    };
    assert_eq!(at_z4(&across, 8), at_z4(&clear, 0));

    // Either side of 180 the box swallows a tile whole, or this tests nothing.
    let e = EXTENT as Local;
    let square = [(0, 0), (e, 0), (e, e), (0, e), (0, 0)];
    let swallowed: Vec<u32> = across
        .at(4)
        .filter(|(_, data)| {
            lyv_mvt::decode_chains(data).unwrap().iter().any(|chain| {
                let mut points: Vec<(Local, Local)> =
                    chain.points.iter().map(|p| (p.x, p.y)).collect();
                points.sort_unstable();
                let mut expected = square.to_vec();
                expected.sort_unstable();
                points == expected
            })
        })
        .map(|(coord, _)| coord.x)
        .collect();
    assert!(
        swallowed.contains(&15) && swallowed.contains(&0),
        "swallowed tiles in columns {swallowed:?}"
    );
}

/// What an archive declares about where its data is.
struct Declared {
    bounds: [f64; 4],
    centre: (f64, f64, u8),
    /// `antimeridian_adjusted_bounds` from the metadata, parsed.
    adjusted: Option<[f64; 4]>,
}

fn declared(source: &str) -> Declared {
    let dir = tempfile::tempdir().unwrap();
    let options = options(&dir, source, 12);
    lyv::run(&options).unwrap();
    let bytes = std::fs::read(&options.output).unwrap();
    let e7 = |offset: usize| {
        f64::from(i32::from_le_bytes(
            bytes[offset..offset + 4].try_into().unwrap(),
        )) / 1e7
    };

    let at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap()) as usize;
    let blob = &bytes[at(24)..at(24) + at(32)];
    let mut json = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(blob), &mut json).unwrap();
    let metadata: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let adjusted = metadata.get("antimeridian_adjusted_bounds").map(|value| {
        let numbers: Vec<f64> = value
            .as_str()
            .expect("a string, as bounds are")
            .split(',')
            .map(|n| n.parse().unwrap())
            .collect();
        numbers.try_into().unwrap()
    });

    Declared {
        bounds: [e7(102), e7(106), e7(110), e7(114)],
        centre: (e7(119), e7(123), bytes[118]),
        adjusted,
    }
}

fn near(a: [f64; 4], b: [f64; 4]) -> bool {
    a.iter().zip(b).all(|(a, b)| (a - b).abs() < 0.01)
}

#[test]
fn data_across_180_declares_every_longitude_and_its_real_extent() {
    // MapLibre clamps bounds to ±180 and fetches nothing outside them, so
    // 175..185 would hide the eastern half. The real extent goes alongside.
    let declared = declared(&collection(&[
        &point(1, 175.0, -40.0),
        &point(2, 185.0, -38.0),
    ]));
    assert!(
        near(declared.bounds, [-180.0, -40.0, 180.0, -38.0]),
        "{:?}",
        declared.bounds
    );
    let adjusted = declared.adjusted.expect("antimeridian_adjusted_bounds");
    assert!(near(adjusted, [175.0, -40.0, 185.0, -38.0]), "{adjusted:?}");
    assert!(
        (declared.centre.0.abs() - 180.0).abs() < 0.01,
        "{:?}",
        declared.centre
    );
}

#[test]
fn data_across_180_in_the_usual_convention_is_found_there_too() {
    // 179 and -179: two degrees apart, not 358. The centre is between them,
    // not on the far side of the world, and the view opens close enough to
    // see them.
    let declared = declared(&collection(&[
        &point(1, 179.0, -17.0),
        &point(2, -179.0, -16.0),
    ]));
    let adjusted = declared.adjusted.expect("antimeridian_adjusted_bounds");
    assert!(near(adjusted, [179.0, -17.0, 181.0, -16.0]), "{adjusted:?}");
    assert!(
        (declared.centre.0.abs() - 180.0).abs() < 0.01,
        "{:?}",
        declared.centre
    );
    assert!(declared.centre.2 >= 7, "opens at z{}", declared.centre.2);
}

#[test]
fn data_wholly_past_180_is_declared_where_it_wraps_to() {
    let declared = declared(&collection(&[
        &point(1, 182.0, -40.0),
        &point(2, 186.0, -38.0),
    ]));
    assert!(
        near(declared.bounds, [-178.0, -40.0, -174.0, -38.0]),
        "{:?}",
        declared.bounds
    );
    assert!(declared.adjusted.is_none(), "{:?}", declared.adjusted);
    assert!(
        (declared.centre.0 + 176.0).abs() < 0.01,
        "{:?}",
        declared.centre
    );
}

#[test]
fn data_clear_of_180_has_no_adjusted_bounds() {
    // Across the prime meridian is the ordinary case, narrow in -180..180.
    let declared = declared(&collection(&[&point(1, -3.0, 50.0), &point(2, 2.0, 52.0)]));
    assert!(
        near(declared.bounds, [-3.0, 50.0, 2.0, 52.0]),
        "{:?}",
        declared.bounds
    );
    assert!(declared.adjusted.is_none(), "{:?}", declared.adjusted);
}

#[test]
fn a_polygon_spanning_the_whole_world_is_built_whole() {
    // Its sides lie on -180 and 180. Both used to land in column 0, where the
    // polygon read as self-intersecting and was dropped.
    let world = tiles(&collection(&[&polygon(1, -180.0, -60.0, 180.0, 60.0)]), 3);
    let west = tiles(&collection(&[&polygon(1, -180.0, -60.0, 0.0, 60.0)]), 3);
    let east = tiles(&collection(&[&polygon(1, 0.0, -60.0, 180.0, 60.0)]), 3);
    assert_ne!(area_at(&world, 3), 0, "the polygon was dropped");
    assert_eq!(area_at(&world, 3), area_at(&west, 3) + area_at(&east, 3));
}

#[test]
fn points_on_180_are_drawable_at_every_zoom() {
    // MapLibre skips any point at x or y >= extent, so a point on a tile's
    // east edge is invisible. Points on 180 go to x = 0 of the first column,
    // and one rounding onto an edge on the way up is pulled back inside.
    let column: Vec<String> = (0..40)
        .map(|i| point(i + 1, 180.0, -60.0 + f64::from(i as u32) * 3.0))
        .collect();
    let refs: Vec<&str> = column.iter().map(String::as_str).collect();
    let tiles = tiles(&collection(&refs), 6);

    for z in 0..=6 {
        let mut seen = std::collections::BTreeSet::new();
        for (coord, data) in tiles.at(z) {
            let extent = lyv_mvt::decode_extent(data).unwrap() as Local;
            for chain in lyv_mvt::decode_chains(data).unwrap() {
                let p = chain.points[0];
                assert!(
                    (0..extent).contains(&p.x) && (0..extent).contains(&p.y),
                    "feature {} at {p:?} in {coord:?} is off the drawable area",
                    chain.feature
                );
                seen.insert(chain.feature);
            }
        }
        assert_eq!(seen.len(), 40, "z{z} holds {} of the 40 points", seen.len());
    }
}
