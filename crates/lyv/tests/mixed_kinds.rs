//! One layer mixing points, lines and polygons: each feature keeps its own
//! kind through the cut, simplification, dust, ranking and every merge.

use std::collections::{BTreeMap, BTreeSet};

use lyv_mvt::GeometryKind;

const SOURCE: &str = r#"{"type":"FeatureCollection","features":[
    {"type":"Feature","id":1,"properties":{},"geometry":
        {"type":"Point","coordinates":[174.78,-41.29]}},
    {"type":"Feature","id":2,"properties":{},"geometry":
        {"type":"LineString","coordinates":[[174.70,-41.30],[174.90,-41.20]]}},
    {"type":"Feature","id":3,"properties":{},"geometry":
        {"type":"Polygon","coordinates":[[[174.60,-41.40],[174.95,-41.40],
            [174.95,-41.10],[174.60,-41.10],[174.60,-41.40]]]}},
    {"type":"Feature","id":4,"properties":{},"geometry":
        {"type":"GeometryCollection","geometries":[
            {"type":"Point","coordinates":[174.80,-41.25]},
            {"type":"Polygon","coordinates":[[[174.75,-41.30],[174.85,-41.30],
                [174.85,-41.22],[174.75,-41.30]]]}]}}
]}"#;

/// Whether a kind is a point, a line or a polygon, ignoring how many parts.
fn family(kind: GeometryKind) -> &'static str {
    match kind {
        GeometryKind::Point { .. } => "point",
        GeometryKind::Line { .. } => "line",
        GeometryKind::Areal { .. } => "polygon",
    }
}

#[test]
fn each_feature_keeps_its_own_kind_at_every_zoom() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("mixed.geojson");
    std::fs::write(&input, SOURCE).unwrap();
    let output = dir.path().join("mixed.pmtiles");
    let options = lyv::Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(10),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "mixed".to_string(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        input_crs: None,
        extent: 4096,
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
        buffer: 64,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    let tiles = lyv::Collected::from(&options).unwrap();

    let expected: BTreeMap<u64, &str> =
        [(1, "point"), (2, "line"), (3, "polygon"), (4, "polygon")].into();
    for z in 0..=10 {
        let mut seen: BTreeMap<u64, BTreeSet<&str>> = BTreeMap::new();
        for (_, data) in tiles.at(z) {
            for (id, kind) in lyv_mvt::decode_geometry_kinds(data).unwrap() {
                seen.entry(id).or_default().insert(family(kind));
            }
        }
        for (id, want) in &expected {
            let kinds = seen
                .get(id)
                .unwrap_or_else(|| panic!("z{z} lost feature {id}"));
            assert_eq!(kinds, &BTreeSet::from([*want]), "z{z} feature {id}");
        }
    }
}
