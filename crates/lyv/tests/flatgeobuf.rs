//! FlatGeobuf input must give exactly what the same data in GeoJSON gives.

use std::io::BufWriter;
use std::path::Path;

use lyv::{Options, run};

const EXTENT: u32 = 4096;

/// A lake with an island, and a river through it.
const SOURCE: &str = r#"{"type":"FeatureCollection","features":[
    {"type":"Feature","properties":{"name":"lake","kind":"water","area":78,
        "depth":2.5,"tidal":false},
     "geometry":{"type":"Polygon","coordinates":[
       [[174.0,-41.6],[176.0,-41.6],[176.0,-40.4],[174.0,-40.4],[174.0,-41.6]],
       [[174.6,-41.3],[175.4,-41.3],[175.4,-40.7],[174.6,-40.7],[174.6,-41.3]]
     ]}}
]}"#;

/// Convert with the reference writer, so the fixture isn't our own idea of the
/// format.
fn write_fgb(json: &str, path: &Path) {
    use flatgeobuf::{FgbWriter, GeometryType};
    use geozero::GeozeroDatasource;
    use geozero::geojson::GeoJsonReader;

    let mut fgb = FgbWriter::create("test", GeometryType::Polygon).unwrap();
    let mut bytes = json.as_bytes();
    GeoJsonReader(&mut bytes).process(&mut fgb).unwrap();
    fgb.write(BufWriter::new(std::fs::File::create(path).unwrap()))
        .unwrap();
}

fn build(input: &Path, output: &Path) -> Options {
    let options = Options {
        store: lyv::default_store_path(output),
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        min_zoom: 0,
        max_zoom: Some(9),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "water".to_string(),
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
        buffer: 64,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    run(&options).expect("the pipeline should run");
    options
}

/// Every finished tile's bytes, by address.
fn encoded(options: &Options) -> std::collections::BTreeMap<(u32, u32, u32), Vec<u8>> {
    lyv::Collected::from(options)
        .unwrap()
        .tiles
        .into_iter()
        .map(|(coord, data)| ((coord.z, coord.x, coord.y), data))
        .collect()
}

#[test]
fn flatgeobuf_and_geojson_produce_byte_identical_tiles() {
    // The same bytes, not just geometry — true only because attributes are
    // sorted by name on the way in.
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("in.geojson");
    let fgb_path = dir.path().join("in.fgb");
    std::fs::write(&json_path, SOURCE).unwrap();
    write_fgb(SOURCE, &fgb_path);

    let from_json = build(&json_path, &dir.path().join("json.pmtiles"));
    let from_fgb = build(&fgb_path, &dir.path().join("fgb.pmtiles"));

    let a = encoded(&from_json);
    let b = encoded(&from_fgb);

    assert!(!a.is_empty(), "the GeoJSON run produced nothing");
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "the two runs covered different tiles"
    );
    let differing: Vec<_> = a
        .iter()
        .filter(|(address, bytes)| b.get(*address) != Some(*bytes))
        .map(|(address, _)| *address)
        .collect();
    assert!(
        differing.is_empty(),
        "{} of {} tiles differ, e.g. {:?}",
        differing.len(),
        a.len(),
        &differing[..differing.len().min(3)]
    );
}

#[test]
fn a_flatgeobuf_file_declares_the_same_layer_and_fields() {
    let dir = tempfile::tempdir().unwrap();
    let fgb_path = dir.path().join("in.fgb");
    write_fgb(SOURCE, &fgb_path);
    let options = build(&fgb_path, &dir.path().join("out.pmtiles"));

    // The archive describes itself the same way whatever the source format.
    let bytes = std::fs::read(&options.output).unwrap();
    let at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap()) as usize;
    let blob = &bytes[at(24)..at(24) + at(32)];
    let json: Vec<u8> = if blob.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(blob), &mut out).unwrap();
        out
    } else {
        blob.to_vec()
    };
    let metadata: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let fields = &metadata["vector_layers"][0]["fields"];

    assert_eq!(metadata["vector_layers"][0]["id"], "water");
    assert_eq!(fields["name"], "String");
    assert_eq!(fields["area"], "Number");
    assert_eq!(fields["depth"], "Number");
    assert_eq!(fields["tidal"], "Boolean");
}
