//! Attributes from GeoJSON to finished tiles. They're stored once per feature
//! and attached only when encoding, so two things to check: that they reach the
//! finished tiles, and that the intermediate pyramid stays free of them.

use lyv::{Options, run};
use lyv_core::TileCoord;
use lyv_store::{SqliteStore, TileStore, Value};

const EXTENT: u32 = 4096;

fn source() -> &'static str {
    r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":1,
         "properties":{"name":"Wairarapa","kind":"lake","area":78,"depth":2.5,
                       "tidal":false,"tags":["water"],"absent":null},
         "geometry":{"type":"Polygon","coordinates":[
           [[174.0,-41.6],[176.0,-41.6],[176.0,-40.4],[174.0,-40.4],[174.0,-41.6]],
           [[174.6,-41.3],[175.4,-41.3],[175.4,-40.7],[174.6,-40.7],[174.6,-41.3]]
         ]}},
        {"type":"Feature","id":2,
         "properties":{"name":"Ruamahanga","kind":"river","length":118},
         "geometry":{"type":"LineString",
          "coordinates":[[173.5,-41.9],[175.0,-41.0],[176.5,-40.1]]}},
        {"type":"Feature","id":3,"properties":{},
         "geometry":{"type":"LineString","coordinates":[[174.2,-41.0],[174.3,-41.1]]}}
    ]}"#
}

struct Built {
    _dir: tempfile::TempDir,
    options: Options,
}

fn build(max_zoom: u32) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("water.geojson");
    std::fs::write(&input, source()).unwrap();
    let output = dir.path().join("water.pmtiles");

    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(max_zoom),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "water".to_string(),
        output_name: None,
        attributes: None,
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
        buffer: 64,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    run(&options).expect("the pipeline should run");
    Built { _dir: dir, options }
}

/// Every `(feature id, attributes)` pair in a stage, across every zoom.
fn attributes_in(store: &SqliteStore) -> Vec<(u64, Vec<(String, Value)>)> {
    let mut out = Vec::new();
    for z in store.zooms().unwrap() {
        store
            .for_each_at(z, &mut |_c: TileCoord, data| {
                out.extend(lyv_mvt::decode_attributes(&data).unwrap());
                Ok(())
            })
            .unwrap();
    }
    out
}

#[test]
fn the_side_table_holds_each_features_attributes_once() {
    let built = build(8);
    let store = SqliteStore::open(&built.options.store).unwrap();

    let lake = store
        .feature(1)
        .unwrap()
        .expect("the lake should be recorded");
    let named: Vec<&str> = lake.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        named,
        vec!["area", "depth", "kind", "name", "tags", "tidal"],
        "the null-valued key should be absent and the rest present"
    );
    assert!(lake.contains(&("area".to_string(), Value::Int(78))));
    assert!(lake.contains(&("depth".to_string(), Value::Double(2.5))));
    assert!(lake.contains(&("tidal".to_string(), Value::Bool(false))));

    // An empty properties object isn't recorded.
    assert_eq!(store.feature(3).unwrap(), None);
}

/// Every feature's attributes across every finished tile.
fn attributes_emitted(options: &Options) -> Vec<(u64, Vec<(String, Value)>)> {
    let mut tiles = lyv::Collected::default();
    lyv::run_into(options, &mut tiles, &lyv::Progress::silent()).unwrap();
    let mut found = Vec::new();
    for data in tiles.tiles.values() {
        found.extend(lyv_mvt::decode_attributes(data).unwrap());
    }
    found
}

#[test]
fn attributes_reach_the_encoded_tiles() {
    let built = build(8);
    let found = attributes_emitted(&built.options);
    assert!(
        !found.is_empty(),
        "no features at all in the finished tiles"
    );

    let lake: Vec<_> = found.iter().filter(|(id, _)| *id == 1).collect();
    assert!(!lake.is_empty(), "the lake never appeared");
    for (_, properties) in &lake {
        assert!(
            properties.contains(&("name".to_string(), Value::Str("Wairarapa".into()))),
            "a piece of the lake reached a tile without its name: {properties:?}"
        );
        assert!(properties.contains(&("area".to_string(), Value::Int(78))));
    }

    let river: Vec<_> = found.iter().filter(|(id, _)| *id == 2).collect();
    assert!(!river.is_empty(), "the river never appeared");
    for (_, properties) in &river {
        assert!(properties.contains(&("name".to_string(), Value::Str("Ruamahanga".into()))));
    }

    // The feature with no properties carries none, not a neighbour's.
    for (_, properties) in found.iter().filter(|(id, _)| *id == 3) {
        assert!(
            properties.is_empty(),
            "an unnamed feature gained {properties:?}"
        );
    }
}

#[test]
fn the_internal_pyramid_carries_no_attributes() {
    // Internal tiles carry no attributes: a feature across a thousand tiles
    // would otherwise be copied a thousand times per zoom.
    let built = build(8);
    let store = SqliteStore::open(&built.options.store).unwrap();

    for (id, properties) in attributes_in(&store) {
        assert!(
            properties.is_empty(),
            "feature {id} carried {properties:?} into the pyramid"
        );
    }
}

#[test]
fn a_feature_split_across_tiles_repeats_its_attributes_once_per_tile() {
    // A feature in two tiles has attributes in both (MVT pools per tile), but
    // never twice in one tile, however many pieces it has there.
    let built = build(8);
    let mut collected = lyv::Collected::default();
    lyv::run_into(&built.options, &mut collected, &lyv::Progress::silent()).unwrap();

    let mut tiles_holding_the_lake = 0;
    for data in collected.tiles.values() {
        let per_feature = lyv_mvt::decode_attributes(data).unwrap();
        let lake: Vec<_> = per_feature.iter().filter(|(id, _)| *id == 1).collect();
        if !lake.is_empty() {
            tiles_holding_the_lake += 1;
            assert_eq!(
                lake.len(),
                1,
                "the lake appeared as {} separate features in one tile",
                lake.len()
            );
        }
    }
    assert!(
        tiles_holding_the_lake > 4,
        "only {tiles_holding_the_lake} tiles held the lake"
    );
}

// ---------------------------------------------------------------------------
// What the archive says about itself
// ---------------------------------------------------------------------------

/// The PMTiles metadata JSON, decompressed.
fn archive_metadata(path: &std::path::Path) -> serde_json::Value {
    let bytes = std::fs::read(path).unwrap();
    let at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap()) as usize;
    let (offset, length) = (at(24), at(32));
    let blob = &bytes[offset..offset + length];

    let json: Vec<u8> = if blob.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(blob), &mut out).unwrap();
        out
    } else {
        blob.to_vec()
    };
    serde_json::from_slice(&json).expect("metadata should be JSON")
}

#[test]
fn the_tileset_is_named_after_the_output_unless_told_otherwise() {
    let built = build(4);
    assert_eq!(archive_metadata(&built.options.output)["name"], "water");

    let named = lyv::Options {
        output_name: Some("Wairarapa water".to_string()),
        ..built.options.clone()
    };
    run(&named).unwrap();
    assert_eq!(archive_metadata(&named.output)["name"], "Wairarapa water");
}

#[test]
fn an_output_with_no_file_name_needs_a_tileset_name() {
    // `..` has no stem to name the tileset after: refused before any work,
    // with the flag that fixes it.
    let built = build(4);
    let nameless = lyv::Options {
        output: built.options.output.with_file_name(".."),
        ..built.options.clone()
    };
    let error = run(&nameless).expect_err("no name to give the tileset");
    assert!(format!("{error:#}").contains("--output-name"), "{error:#}");

    let named = lyv::Options {
        output_name: Some("given".to_string()),
        ..nameless.clone()
    };
    assert_eq!(named.tileset_name().unwrap(), "given");
}

#[test]
fn the_archive_declares_its_layer_and_fields() {
    // `vector_layers` is how a consumer learns a layer exists without decoding
    // a tile.
    let built = build(8);
    let metadata = archive_metadata(&built.options.output);

    let layers = metadata["vector_layers"]
        .as_array()
        .expect("vector_layers should be present");
    assert_eq!(layers.len(), 1);

    let layer = &layers[0];
    assert_eq!(layer["id"], "water");
    assert_eq!(layer["minzoom"], 0);
    assert_eq!(layer["maxzoom"], 8);

    let fields = layer["fields"]
        .as_object()
        .expect("fields should be an object");
    // Every numeric width becomes TileJSON's `Number`.
    assert_eq!(fields["name"], "String");
    assert_eq!(fields["area"], "Number", "an integer is a Number");
    assert_eq!(fields["depth"], "Number", "so is a double");
    assert_eq!(fields["tidal"], "Boolean");
    assert_eq!(
        fields["tags"], "String",
        "a nested value arrives as its JSON text"
    );
    assert!(
        !fields.contains_key("absent"),
        "a null-valued key should not be declared"
    );
}

#[test]
fn a_key_used_with_two_types_is_declared_as_a_string() {
    // A key seen with two types is `String`: the one type every value can be
    // read as.
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("mixed.geojson");
    std::fs::write(
        &input,
        r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":1,"properties":{"n":1},
             "geometry":{"type":"LineString","coordinates":[[174,-41],[175,-40]]}},
            {"type":"Feature","id":2,"properties":{"n":"two"},
             "geometry":{"type":"LineString","coordinates":[[175,-41],[176,-40]]}}
        ]}"#,
    )
    .unwrap();
    let output = dir.path().join("mixed.pmtiles");
    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output: output.clone(),
        min_zoom: 0,
        max_zoom: Some(5),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "mixed".to_string(),
        output_name: None,
        attributes: None,
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
        buffer: 0,
        simplification: lyv_simplify::Tolerance::NONE,
    };
    run(&options).unwrap();

    let metadata = archive_metadata(&output);
    assert_eq!(metadata["vector_layers"][0]["fields"]["n"], "String");
}

#[test]
fn an_mbtiles_archive_declares_the_same_layers() {
    // MBTiles keeps TileJSON's extra fields in one `json` row.
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("water.geojson");
    std::fs::write(&input, source()).unwrap();
    let output = dir.path().join("water.mbtiles");
    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output: output.clone(),
        min_zoom: 0,
        max_zoom: Some(6),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "water".to_string(),
        output_name: None,
        attributes: None,
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
        buffer: 0,
        simplification: lyv_simplify::Tolerance::NONE,
    };
    run(&options).unwrap();

    let conn = rusqlite::Connection::open(&output).unwrap();
    let json: String = conn
        .query_row("SELECT value FROM metadata WHERE name = 'json'", [], |r| {
            r.get(0)
        })
        .expect("an mbtiles archive should carry a json metadata row");
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["vector_layers"][0]["id"], "water");
    assert_eq!(parsed["vector_layers"][0]["fields"]["name"], "String");
}
