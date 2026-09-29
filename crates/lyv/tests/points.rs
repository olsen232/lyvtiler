//! Point layers through the whole pipeline.
//!
//! A one-point chain is a point, which is only safe if no stage ever changes a
//! chain's kind: a run may vanish but never become a point, and a point stays a
//! point. Otherwise a line collapsed onto one coordinate would leave a `line`
//! style layer and join a `circle` one. `simplify_points` enforces it.

use std::collections::{BTreeMap, BTreeSet};

use lyv::{Collected, Limits, Options, Progress, run_into};

const MAX_ZOOM: u32 = 9;

/// A grid of points, a MultiPoint, and a line, for a mixed layer.
fn source() -> String {
    let mut features = Vec::new();
    for i in 0..60i64 {
        let x = 172.0 + (i % 10) as f64 * 0.08;
        let y = -44.0 + (i / 10) as f64 * 0.08;
        features.push(format!(
            r#"{{"type":"Feature","id":{i},"properties":{{"name":"tree {i}"}},
               "geometry":{{"type":"Point","coordinates":[{x},{y}]}}}}"#
        ));
    }
    features.push(
        r#"{"type":"Feature","id":900,"properties":{"name":"cluster"},
           "geometry":{"type":"MultiPoint","coordinates":[[173.0,-43.0],[173.3,-43.3]]}}"#
            .to_string(),
    );
    features.push(
        r#"{"type":"Feature","id":901,"properties":{"name":"track"},
           "geometry":{"type":"LineString","coordinates":[[172.1,-43.9],[172.7,-43.2]]}}"#
            .to_string(),
    );
    // Short enough to collapse onto one coordinate within a level or two. It
    // must vanish, not become a point.
    features.push(
        r#"{"type":"Feature","id":902,"properties":{"name":"stub"},
           "geometry":{"type":"LineString","coordinates":[[172.5,-43.5],[172.5004,-43.5004]]}}"#
            .to_string(),
    );
    format!(
        r#"{{"type":"FeatureCollection","features":[{}]}}"#,
        features.join(",")
    )
}

/// Build into memory, to read tiles back with attributes.
fn build(body: &str) -> Collected {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.geojson");
    std::fs::write(&input, body).unwrap();
    let output = dir.path().join("out.pmtiles");

    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(MAX_ZOOM),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "points".to_string(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        input_crs: None,
        extent: 4096,
        // One resolution everywhere, so the collapse test's halving is plain.
        maxzoom_extent: Some(4096),
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        dust: true,
        threads: 1,
        keep_store: false,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        limits: Limits {
            bytes: 0,
            features: 0,
        },
        max_tile_vertices: 0,
        buffer: 0,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    let mut tiles = Collected::default();
    run_into(&options, &mut tiles, &Progress::silent()).expect("a point layer should build");
    tiles
}

/// Every chain in the tileset, as `(zoom, feature id, vertex count)`.
fn chains(tiles: &Collected) -> Vec<(u32, u64, usize)> {
    let mut out = Vec::new();
    for (coord, data) in &tiles.tiles {
        for chain in lyv_mvt::decode_chains(data).unwrap() {
            out.push((coord.z, chain.feature, chain.points.len()));
        }
    }
    out
}

/// Which feature ids appear at each zoom.
fn by_zoom(tiles: &Collected) -> BTreeMap<u32, BTreeSet<u64>> {
    let mut found: BTreeMap<u32, BTreeSet<u64>> = BTreeMap::new();
    for (zoom, feature, _) in chains(tiles) {
        found.entry(zoom).or_default().insert(feature);
    }
    found
}

#[test]
fn a_single_point_reaches_every_zoom() {
    let tiles = build(
        r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":1,"properties":{"name":"one"},
             "geometry":{"type":"Point","coordinates":[174.7,-41.3]}}
        ]}"#,
    );
    let found = by_zoom(&tiles);
    for zoom in 0..=MAX_ZOOM {
        assert!(
            found.get(&zoom).is_some_and(|ids| ids.contains(&1)),
            "the point is missing at zoom {zoom}"
        );
    }
}

#[test]
fn a_point_stays_one_vertex_at_every_zoom() {
    // Nor does a point ever grow into a run.
    let tiles = build(&source());
    for (zoom, feature, vertices) in chains(&tiles) {
        if feature < 100 || feature == 900 {
            assert_eq!(
                vertices, 1,
                "feature {feature} has {vertices} vertices at zoom {zoom}"
            );
        }
    }
}

#[test]
fn a_line_never_becomes_a_point() {
    // Feature 902 eventually collapses; it must disappear, not become a dot.
    let tiles = build(&source());
    let mut seen_somewhere = false;
    for (zoom, feature, vertices) in chains(&tiles) {
        if feature == 901 || feature == 902 {
            seen_somewhere = true;
            assert!(
                vertices >= 2,
                "line {feature} came out as {vertices} vertices at zoom {zoom}"
            );
        }
    }
    assert!(seen_somewhere, "neither line survived at all");
}

#[test]
fn a_multipoint_stays_one_feature() {
    // A MultiPoint is one feature with one id: the merge rejoins pieces by id.
    let tiles = build(&source());
    let at_maxzoom: Vec<_> = chains(&tiles)
        .into_iter()
        .filter(|(z, f, _)| *z == MAX_ZOOM && *f == 900)
        .collect();
    assert_eq!(at_maxzoom.len(), 2, "the MultiPoint's two parts");
}

#[test]
fn points_and_lines_share_a_layer() {
    // Kinds coexist in one layer, told apart by chain length alone.
    let tiles = build(&source());
    let deepest = &by_zoom(&tiles)[&MAX_ZOOM];
    assert!(deepest.contains(&0), "a point is missing");
    assert!(deepest.contains(&900), "the MultiPoint is missing");
    assert!(deepest.contains(&901), "the line is missing");
}

#[test]
fn coincident_points_of_one_feature_collapse_on_the_way_up() {
    // Two parts that round onto one coordinate become one dot, not two.
    let tiles = build(
        r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":1,"properties":{},
             "geometry":{"type":"MultiPoint","coordinates":[[174.70,-41.30],[174.71,-41.31]]}}
        ]}"#,
    );
    let mut parts: BTreeMap<u32, usize> = BTreeMap::new();
    for (zoom, _, _) in chains(&tiles) {
        *parts.entry(zoom).or_default() += 1;
    }
    assert_eq!(parts[&MAX_ZOOM], 2, "distinct at maxzoom");
    assert_eq!(parts[&0], 1, "and one dot by zoom 0");
}

#[test]
fn two_features_at_one_coordinate_both_survive() {
    // Only within a feature: two features at one coordinate both stay, and
    // thinning them is the ranking's job.
    let tiles = build(
        r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":1,"properties":{},
             "geometry":{"type":"Point","coordinates":[174.7,-41.3]}},
            {"type":"Feature","id":2,"properties":{},
             "geometry":{"type":"Point","coordinates":[174.7,-41.3]}}
        ]}"#,
    );
    assert_eq!(by_zoom(&tiles)[&0], BTreeSet::from([1, 2]));
}

#[test]
fn points_carry_their_attributes() {
    let tiles = build(&source());
    let mut named = 0;
    for data in tiles.tiles.values() {
        for (id, properties) in lyv_mvt::decode_attributes(data).unwrap() {
            if id == 900 {
                assert_eq!(
                    properties,
                    vec![("name".to_string(), lyv_mvt::Value::Str("cluster".into()))]
                );
                named += 1;
            }
        }
    }
    assert!(named > 0, "the MultiPoint carried no attributes anywhere");
}
