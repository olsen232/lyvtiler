//! Choosing which attributes come out, and what the layer is called.
//!
//! Attributes are narrowed during the read. Every asked-for name must turn up
//! on some feature — checked at the end, since a name might be on one row in
//! ten million — and "nobody has it" is kept apart from "only some do".

use std::io::BufWriter;
use std::path::Path;

use lyv::{Collected, Limits, Options, Progress, run, run_into};

const MAX_ZOOM: u32 = 5;

/// Two squares, only the second with `depth`: sparse, not absent.
const SOURCE: &str = r#"{"type":"FeatureCollection","features":[
    {"type":"Feature","id":1,"properties":{"name":"first","kind":"park","area":12},
     "geometry":{"type":"Polygon","coordinates":[
       [[174.0,-41.6],[174.4,-41.6],[174.4,-41.2],[174.0,-41.2],[174.0,-41.6]]]}},
    {"type":"Feature","id":2,"properties":{"name":"second","kind":"lake","area":8,
        "depth":3},
     "geometry":{"type":"Polygon","coordinates":[
       [[175.0,-41.6],[175.4,-41.6],[175.4,-41.2],[175.0,-41.2],[175.0,-41.6]]]}}
]}"#;

/// The same squares with the same names on both, for FlatGeobuf fixtures:
/// `FgbWriter` builds its header from the features, so a new name on a later
/// feature makes earlier ones fail to parse. GDAL fills the gap with nulls.
const UNIFORM: &str = r#"{"type":"FeatureCollection","features":[
    {"type":"Feature","id":1,"properties":{"name":"first","kind":"park","area":12},
     "geometry":{"type":"Polygon","coordinates":[
       [[174.0,-41.6],[174.4,-41.6],[174.4,-41.2],[174.0,-41.2],[174.0,-41.6]]]}},
    {"type":"Feature","id":2,"properties":{"name":"second","kind":"lake","area":8},
     "geometry":{"type":"Polygon","coordinates":[
       [[175.0,-41.6],[175.4,-41.6],[175.4,-41.2],[175.0,-41.2],[175.0,-41.6]]]}}
]}"#;

fn options(dir: &Path, input: &Path) -> Options {
    let output = dir.join("out.pmtiles");
    Options {
        store: lyv::default_store_path(&output),
        input: input.to_path_buf(),
        output,
        min_zoom: 0,
        max_zoom: Some(MAX_ZOOM),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "out".to_string(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        input_crs: None,
        extent: 4096,
        maxzoom_extent: None,
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
    }
}

fn write_geojson(body: &'static str) -> impl FnOnce(&Path) {
    move |path: &Path| std::fs::write(path, body).unwrap()
}

/// The same features as a FlatGeobuf file, whose layer is named `water`.
fn write_fgb(body: &'static str) -> impl FnOnce(&Path) {
    move |path: &Path| {
        use flatgeobuf::{FgbWriter, GeometryType};
        use geozero::GeozeroDatasource;
        use geozero::geojson::GeoJsonReader;

        let mut fgb = FgbWriter::create("water", GeometryType::Polygon).unwrap();
        let mut bytes = body.as_bytes();
        GeoJsonReader(&mut bytes).process(&mut fgb).unwrap();
        fgb.write(BufWriter::new(std::fs::File::create(path).unwrap()))
            .unwrap();
    }
}

/// Build into memory, to read the tiles back with attributes (the store holds
/// none).
fn build(
    extension: &str,
    write: impl FnOnce(&Path),
    adjust: impl FnOnce(&mut Options),
) -> Result<Collected, String> {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join(format!("in.{extension}"));
    write(&input);
    let mut options = options(dir.path(), &input);
    adjust(&mut options);

    let mut tiles = Collected::default();
    match run_into(&options, &mut tiles, &Progress::silent()) {
        Ok(()) => Ok(tiles),
        Err(error) => Err(format!("{error:#}")),
    }
}

/// The error a build produced. (`expect_err` would need `Debug` on every tile
/// the run produced.)
fn refused(result: Result<Collected, String>, expected: &str) -> String {
    match result {
        Ok(tiles) => panic!(
            "{expected}, but the build succeeded with {} tiles",
            tiles.tiles.len()
        ),
        Err(error) => error,
    }
}

/// Every attribute name the tiles carry, deduplicated.
fn attribute_names(tiles: &Collected) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for data in tiles.tiles.values() {
        for (_, properties) in lyv_mvt::decode_attributes(data).unwrap() {
            for (name, _) in properties {
                names.insert(name);
            }
        }
    }
    names.into_iter().collect()
}

#[test]
fn every_attribute_is_carried_by_default() {
    let tiles = build("geojson", write_geojson(SOURCE), |_| {}).unwrap();
    assert_eq!(attribute_names(&tiles), ["area", "depth", "kind", "name"]);
}

#[test]
fn an_empty_list_carries_none() {
    // `--no-attributes`: an empty list, distinct from `None` (all).
    let tiles = build("geojson", write_geojson(SOURCE), |options| {
        options.attributes = lyv::Attributes::Only(Vec::new());
    })
    .unwrap();
    assert!(attribute_names(&tiles).is_empty());
}

#[test]
fn a_named_list_carries_only_those() {
    let tiles = build("geojson", write_geojson(SOURCE), |options| {
        options.attributes = lyv::Attributes::Only(vec!["name".into(), "depth".into()]);
    })
    .unwrap();
    // `depth` is on one feature: sparse, which is fine.
    assert_eq!(attribute_names(&tiles), ["depth", "name"]);
}

#[test]
fn an_attribute_no_feature_has_is_refused() {
    // A name nobody has is probably a typo, so it's an error.
    let error = refused(
        build("geojson", write_geojson(SOURCE), |options| {
            options.attributes = lyv::Attributes::Only(vec!["name".into(), "naem".into()]);
        }),
        "a name nothing carries should be refused",
    );
    assert!(error.contains("naem"), "it should name the one at fault");
    assert!(
        !error.contains("name,") && !error.contains(", name"),
        "and should not blame the one that was fine: {error}"
    );
}

#[test]
fn the_attribute_check_runs_on_flatgeobuf_too() {
    // Each reader filters its own way, so each needs its own proof.
    let error = refused(
        build("fgb", write_fgb(UNIFORM), |options| {
            options.attributes = lyv::Attributes::Only(vec!["nonesuch".into()]);
        }),
        "a name nothing carries should be refused",
    );
    assert!(error.contains("nonesuch"), "got: {error}");
}

#[test]
fn flatgeobuf_filters_attributes_too() {
    let tiles = build("fgb", write_fgb(UNIFORM), |options| {
        options.attributes = lyv::Attributes::Only(vec!["kind".into()]);
    })
    .unwrap();
    assert_eq!(attribute_names(&tiles), ["kind"]);
}

#[test]
fn the_output_layer_name_is_independent_of_the_input() {
    // The FlatGeobuf layer is `water`; the output is asked for `hydrography`.
    // Checked in the archive's metadata, where consumers look.
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.fgb");
    write_fgb(UNIFORM)(&input);

    let mut options = options(dir.path(), &input);
    options.output_layer = "hydrography".into();
    run(&options).unwrap();

    let bytes = std::fs::read(&options.output).unwrap();
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
    let metadata: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(metadata["vector_layers"][0]["id"], "hydrography");
}
