//! Choosing the maxzoom from the data. Too shallow a maxzoom looks like a
//! dropping problem: 368k parcels lose 97% at zoom 12 and none at zoom 14.
//!
//! The key test is the last: data is projected onto the deepest grid the run
//! might pick, so choosing a shallower zoom must give exactly what asking for
//! it gives.

use std::collections::{BTreeMap, HashSet};

use lyv::{Limits, Options, run};
use lyv_store::{SqliteStore, TileStore};

/// `across` × `across` small squares packed into `span` degrees, each with a
/// distinct string attribute so nothing pools. Spread over two degrees they
/// fit zoom 12 easily; packed into a twentieth of a degree they share one tile,
/// which is what automatic depth is for.
fn source(across: i64, span: f64) -> String {
    let mut features = Vec::new();
    for row in 0..across {
        for column in 0..across {
            let id = row * across + column;
            let x = 172.0 + (column as f64) * span / (across as f64);
            let y = -44.0 + (row as f64) * span / (across as f64);
            let w = 0.4 * span / (across as f64);
            features.push(format!(
                r#"{{"type":"Feature","id":{id},"properties":
                   {{"address":"{id} Someplace Road, Suburbia","title":"NA{id}A/{id}"}},
                   "geometry":{{"type":"Polygon","coordinates":[[[{x},{y}],[{x2},{y}],
                     [{x2},{y2}],[{x},{y2}],[{x},{y}]]]}}}}"#,
                x2 = x + w,
                y2 = y + w,
            ));
        }
    }
    format!(
        r#"{{"type":"FeatureCollection","features":[{}]}}"#,
        features.join(",")
    )
}

struct Built {
    _dir: tempfile::TempDir,
    options: Options,
}

fn build(across: i64, span: f64, max_zoom: Option<u32>, cap: u32, limits: Limits) -> Built {
    build_with(across, span, max_zoom, cap, limits, None)
}

fn build_with(
    across: i64,
    span: f64,
    max_zoom: Option<u32>,
    cap: u32,
    limits: Limits,
    maxzoom_extent: Option<u32>,
) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("squares.geojson");
    std::fs::write(&input, source(across, span)).unwrap();
    let output = dir.path().join("squares.pmtiles");
    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom,
        min_auto_zoom: 12,
        max_auto_zoom: cap,
        output_layer: "squares".to_string(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        input_crs: None,
        regular_angles_check: true,
        extent: 4096,
        maxzoom_extent,
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        dust: true,
        threads: 1,
        limits,
        // No vertex budget: these tests vary the feature budget.
        max_tile_vertices: 0,
        buffer: 0,
        keep_store: true,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    run(&options).unwrap();
    Built { _dir: dir, options }
}

/// The deepest zoom the store actually holds tiles at.
fn built_to(built: &Built) -> u32 {
    let store = SqliteStore::open(&built.options.store).unwrap();
    *store.zooms().unwrap().iter().max().unwrap()
}

/// Every tile in the pyramid, so two runs can be compared exactly.
fn pyramid(built: &Built) -> BTreeMap<(u32, u32, u32), Vec<u8>> {
    let store = SqliteStore::open(&built.options.store).unwrap();
    let mut all = BTreeMap::new();
    for z in store.zooms().unwrap() {
        store
            .for_each_at(z, &mut |coord, data| {
                all.insert((coord.z, coord.x, coord.y), data);
                Ok(())
            })
            .unwrap();
    }
    all
}

const ROOMY: Limits = Limits {
    bytes: 1_000_000,
    features: 200_000,
};

#[test]
fn a_tiny_layer_does_not_get_a_deep_maxzoom() {
    // Nothing to fix, so the probe stays where it started: each level deeper
    // quadruples the maxzoom tiles.
    let built = build(4, 0.05, None, 16, ROOMY);
    assert_eq!(
        built_to(&built),
        12,
        "sixteen squares should not need more than the starting zoom"
    );
}

#[test]
fn a_crowded_layer_gets_a_deeper_maxzoom() {
    // A budget the start zoom can't meet; deepening spreads the features out.
    let tight = Limits {
        bytes: 4_000,
        features: 0,
    };
    let built = build(30, 0.05, None, 16, tight);
    assert!(
        built_to(&built) > 12,
        "a layer over budget at zoom 12 was left at zoom {}",
        built_to(&built)
    );
}

#[test]
fn the_cap_is_respected_however_badly_it_overflows() {
    // An impossible budget: stop at the cap and drop features.
    let impossible = Limits {
        bytes: 1,
        features: 1,
    };
    let built = build(20, 0.05, None, 14, impossible);
    assert_eq!(built_to(&built), 14, "the cap should have stopped it");
}

#[test]
fn an_explicit_maxzoom_is_obeyed_even_when_it_is_a_bad_one() {
    // A named zoom is used as given.
    let tight = Limits {
        bytes: 4_000,
        features: 0,
    };
    let built = build(30, 0.05, Some(10), 16, tight);
    assert_eq!(built_to(&built), 10);
}

#[test]
fn choosing_a_zoom_builds_the_same_pyramid_as_asking_for_it() {
    // An automatic run landing on zoom 13 must produce what `-z 13` produces.
    // Scaling down rounds a second time, so coordinates may differ by the odd
    // unit (§4); which tiles exist and which features are in them may not.
    let tight = Limits {
        bytes: 30_000,
        features: 0,
    };
    let chosen = build(24, 0.05, None, 16, tight);
    let depth = built_to(&chosen);
    assert!(
        (13..16).contains(&depth),
        "expected the probe to land between 13 and 15, got {depth}"
    );

    let asked = build(24, 0.05, Some(depth), 16, tight);
    let chosen_tiles: HashSet<(u32, u32, u32)> = pyramid(&chosen).into_keys().collect();
    let asked_tiles: HashSet<(u32, u32, u32)> = pyramid(&asked).into_keys().collect();
    assert_eq!(
        chosen_tiles, asked_tiles,
        "choosing zoom {depth} produced a different set of tiles than asking for it"
    );
}

/// The PMTiles v3 bounds, read from the header bytes.
fn header_bounds(path: &std::path::Path) -> [f64; 4] {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..7], b"PMTiles", "not a PMTiles archive");
    assert_eq!(bytes[7], 3, "expected spec version 3");
    let at = |offset: usize| -> i32 {
        i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    };
    [at(102), at(106), at(110), at(114)].map(|v| f64::from(v) / 10_000_000.0)
}

#[test]
fn a_chosen_maxzoom_still_declares_where_the_data_is() {
    // The extent is accumulated on the deepest grid, not the chosen zoom's;
    // unprojected at the wrong zoom, the bounds silently point at open ocean.
    // `polygons.rs` names its maxzoom, so can't show this.
    let tight = Limits {
        bytes: 30_000,
        features: 0,
    };
    let built = build(24, 0.05, None, 16, tight);
    assert!(
        built_to(&built) < 16,
        "this fixture should settle short of the cap, or the two zooms coincide \
         and the test proves nothing"
    );

    // The fixture sits at 172E, 44S.
    let [min_lon, min_lat, max_lon, max_lat] = header_bounds(&built.options.output);
    assert!(
        (171.9..172.2).contains(&min_lon) && (171.9..172.2).contains(&max_lon),
        "longitude came back as {min_lon}..{max_lon}, expected about 172"
    );
    assert!(
        (-44.1..-43.8).contains(&min_lat) && (-44.1..-43.8).contains(&max_lat),
        "latitude came back as {min_lat}..{max_lat}, expected about -44"
    );
}

#[test]
fn a_finer_maxzoom_extent_does_not_move_the_tiles() {
    // The maxzoom extent applies to the chosen zoom. Asked before the choice,
    // `extent_at` answers for the cap, and every tile address doubles —
    // silently. Other tests leave the maxzoom extent unset.
    let tight = Limits {
        bytes: 30_000,
        features: 0,
    };
    let chosen = build_with(24, 0.05, None, 16, tight, Some(8192));
    let depth = built_to(&chosen);
    assert!(
        depth < 16,
        "this fixture must settle short of the cap, or the chosen maxzoom and \
         the one `extent_at` assumes coincide and the test proves nothing"
    );

    let asked = build_with(24, 0.05, Some(depth), 16, tight, Some(8192));
    let chosen_tiles: HashSet<(u32, u32, u32)> = pyramid(&chosen).into_keys().collect();
    let asked_tiles: HashSet<(u32, u32, u32)> = pyramid(&asked).into_keys().collect();
    assert_eq!(
        chosen_tiles, asked_tiles,
        "choosing zoom {depth} put the tiles somewhere else than asking for it"
    );
}

/// Build the given GeoJSON, choosing the maxzoom.
fn build_body(body: &str) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("degenerate.geojson");
    std::fs::write(&input, body).unwrap();
    let output = dir.path().join("degenerate.pmtiles");
    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        // Nothing says how deep; the data barely constrains it.
        max_zoom: None,
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "degenerate".to_string(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        input_crs: None,
        regular_angles_check: true,
        extent: 4096,
        maxzoom_extent: None,
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        dust: true,
        threads: 1,
        limits: ROOMY,
        max_tile_vertices: 0,
        buffer: 0,
        keep_store: true,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    run(&options).expect("a degenerate layer should still build");
    Built { _dir: dir, options }
}

#[test]
fn one_point_with_no_maxzoom_given_still_builds() {
    // A single point: an extent of zero area, the case that divides by zero if
    // anything does. `points.rs` names its maxzoom, so never probes.
    let built = build_body(
        r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":1,"properties":{"name":"one"},
             "geometry":{"type":"Point","coordinates":[174.7,-41.3]}}
        ]}"#,
    );

    // Anywhere within bounds: the probe must terminate.
    let depth = built_to(&built);
    assert!(
        (12..=16).contains(&depth),
        "chose zoom {depth}, outside the range it was allowed"
    );

    // The point survives to every zoom.
    let tiles = pyramid(&built);
    for zoom in 0..=depth {
        assert!(
            tiles.keys().any(|&(z, _, _)| z == zoom),
            "no tile at zoom {zoom}"
        );
    }
}

#[test]
fn a_zero_width_extent_is_declared_without_nans() {
    // A zero-width box is fine; `NaN` bounds are not — they pass as bytes and a
    // viewer opens on nothing.
    let built = build_body(
        r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":1,"properties":{"name":"one"},
             "geometry":{"type":"Point","coordinates":[174.7,-41.3]}}
        ]}"#,
    );
    let bounds = header_bounds(&built.options.output);
    assert!(
        bounds.iter().all(|v| v.is_finite()),
        "bounds are not finite: {bounds:?}"
    );
    // Degenerate, and on the point, not the origin.
    for (got, want) in bounds.iter().zip([174.7, -41.3, 174.7, -41.3]) {
        assert!(
            (got - want).abs() < 1e-5,
            "bounds {bounds:?} do not sit on the point"
        );
    }
}
