//! Rows with attributes and no shape are dropped and counted, and the run
//! carries on.
//!
//! Tested per format because the readers disagreed: FlatGeobuf passed such a
//! row to `to_geo`, which calls an absent geometry malformed, so one
//! attribute-only row stopped the whole run.

use std::io::BufWriter;
use std::path::Path;

use lyv::{Limits, Options, run};
use lyv_store::{SqliteStore, TileStore};

const EXTENT: u32 = 4096;
const MAX_ZOOM: u32 = 6;

/// One drawable line, one `null` geometry, and one with no `geometry` key (RFC
/// 7946 allows it; it's different JSON from `null`).
const MIXED: &str = r#"{"type":"FeatureCollection","features":[
    {"type":"Feature","id":1,"properties":{"name":"absent"},"geometry":null},
    {"type":"Feature","id":2,"properties":{"name":"real"},
     "geometry":{"type":"LineString","coordinates":[[174.7,-41.3],[174.8,-41.2]]}},
    {"type":"Feature","id":3,"properties":{"name":"missing"}}
]}"#;

/// Nothing drawable: no bounds, no maxzoom, nothing to write.
const ALL_ABSENT: &str = r#"{"type":"FeatureCollection","features":[
    {"type":"Feature","id":1,"properties":{"name":"a"},"geometry":null},
    {"type":"Feature","id":2,"properties":{"name":"b"},"geometry":null}
]}"#;

struct Built {
    _dir: tempfile::TempDir,
    options: Options,
}

fn build(extension: &str, write_input: impl FnOnce(&Path)) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pmtiles");
    // The reader is chosen by extension.
    let input = dir.path().join(format!("in.{extension}"));
    write_input(&input);

    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(MAX_ZOOM),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "rows".to_string(),
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
        limits: Limits {
            bytes: 0,
            features: 0,
        },
        max_tile_vertices: 0,
        buffer: 0,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    run(&options).expect("a row with no geometry should not stop the run");
    Built { _dir: dir, options }
}

fn geojson(body: &'static str) -> impl FnOnce(&Path) {
    move |path: &Path| std::fs::write(path, body).unwrap()
}

/// `MIXED` as FlatGeobuf, with the geometry genuinely absent on two features.
///
/// A committed fixture, because only GDAL writes this shape: geozero's
/// `GeoJsonReader` refuses null geometries, and `FgbWriter` writes empty ones
/// (tested below). Regenerate, from this directory:
///
/// ```text
/// ogr2ogr -f FlatGeobuf -lco SPATIAL_INDEX=NO data/no_geometry.fgb mixed.geojson
/// ```
///
/// GDAL refuses null geometries in an indexed FlatGeobuf, hence
/// `SPATIAL_INDEX=NO`.
const ABSENT_FIXTURE: &str = "tests/data/no_geometry.fgb";

fn copy_fixture(path: &Path) {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join(ABSENT_FIXTURE);
    std::fs::copy(&from, path).expect("the FlatGeobuf fixture should be committed");
}

/// The same rows written by `FgbWriter`, so the attribute-only features carry
/// an empty geometry rather than none — the other shape such a row can take.
fn empty_geometries(path: &Path) {
    use flatgeobuf::{FgbWriter, GeometryType};
    use geozero::geojson::GeoJson;
    use geozero::{ColumnValue, FeatureProcessor, PropertyProcessor};

    fn attribute_only(fgb: &mut FgbWriter<'_>, index: u64, name: &str) {
        fgb.property(0, "name", &ColumnValue::String(name)).unwrap();
        fgb.feature_end(index).unwrap();
    }

    let mut fgb = FgbWriter::create("rows", GeometryType::Unknown).unwrap();
    attribute_only(&mut fgb, 0, "absent");
    fgb.add_feature_geom(
        GeoJson(r#"{"type":"LineString","coordinates":[[174.7,-41.3],[174.8,-41.2]]}"#),
        |feature| {
            feature
                .property(0, "name", &ColumnValue::String("real"))
                .unwrap();
        },
    )
    .unwrap();
    attribute_only(&mut fgb, 2, "missing");

    fgb.write(BufWriter::new(std::fs::File::create(path).unwrap()))
        .unwrap();
}

/// Every feature id that reached the pyramid, at every zoom.
fn survivors(built: &Built) -> Vec<u64> {
    let store = SqliteStore::open(&built.options.store).unwrap();
    let mut found = std::collections::BTreeSet::new();
    for z in built.options.min_zoom..=built.options.deepest() {
        store
            .for_each_at(z, &mut |_, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    found.insert(chain.feature);
                }
                Ok(())
            })
            .unwrap();
    }
    found.into_iter().collect()
}

#[test]
fn geojson_drops_a_null_geometry_and_keeps_the_rest() {
    let built = build("geojson", geojson(MIXED));
    assert_eq!(
        survivors(&built).len(),
        1,
        "only the one drawable feature should reach the pyramid"
    );
}

#[test]
fn the_fixture_really_has_no_geometry() {
    // Or the next test passes for the wrong reason, as it once did with an
    // `FgbWriter` fixture that had empty geometries instead of none.
    use flatgeobuf::{FallibleStreamingIterator, FgbReader};

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(ABSENT_FIXTURE);
    let mut reader = std::io::BufReader::new(std::fs::File::open(path).unwrap());
    let mut features = FgbReader::open(&mut reader).unwrap().select_all().unwrap();

    let mut absent = 0;
    let mut present = 0;
    while let Some(feature) = features.next().unwrap() {
        if feature.geometry().is_none() {
            absent += 1;
        } else {
            present += 1;
        }
    }
    assert_eq!((absent, present), (2, 1));
}

#[test]
fn flatgeobuf_drops_an_absent_geometry_and_keeps_the_rest() {
    // Used to stop the run: `to_geo` can't tell absent from corrupt.
    let built = build("fgb", copy_fixture);
    assert_eq!(survivors(&built).len(), 1);
}

#[test]
fn flatgeobuf_drops_an_empty_geometry_too() {
    let built = build("fgb", empty_geometries);
    assert_eq!(survivors(&built).len(), 1);
}

#[test]
fn the_two_formats_agree_about_what_survives() {
    // The same row must survive in both formats. Compared by geometry, since
    // FlatGeobuf ids are file positions.
    let from_json = build("geojson", geojson(MIXED));
    let from_fgb = build("fgb", copy_fixture);

    let chains = |built: &Built| {
        let store = SqliteStore::open(&built.options.store).unwrap();
        let mut points = Vec::new();
        store
            .for_each_at(MAX_ZOOM, &mut |_, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    points.push(chain.points);
                }
                Ok(())
            })
            .unwrap();
        points
    };
    assert_eq!(chains(&from_json), chains(&from_fgb));
    assert!(!chains(&from_json).is_empty(), "the line should be there");
}

#[test]
fn a_layer_with_no_geometry_at_all_still_writes_an_archive() {
    // The run must still finish with a readable, empty archive.
    let built = build("geojson", geojson(ALL_ABSENT));
    assert!(survivors(&built).is_empty());
    let written = std::fs::metadata(&built.options.output).expect("an archive should exist");
    assert!(written.len() > 0, "the archive should not be empty bytes");
}
