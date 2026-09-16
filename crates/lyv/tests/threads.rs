//! The worker count must not change the output. Tiles finish in whatever order
//! the threads manage, so anything depending on that order would come and go
//! with machine load.

use std::path::PathBuf;

use lyv::{Limits, Options, run};

/// Squares with distinct attributes over a few hundred tiles, so workers
/// really interleave.
fn source() -> String {
    let mut features = Vec::new();
    for row in 0..20i64 {
        for column in 0..20i64 {
            let id = row * 20 + column;
            let x = 172.0 + (column as f64) * 0.05;
            let y = -44.0 + (row as f64) * 0.05;
            features.push(format!(
                r#"{{"type":"Feature","id":{id},"properties":
                   {{"name":"square {id}","area":{id}.5}},
                   "geometry":{{"type":"Polygon","coordinates":[[[{x},{y}],[{x2},{y}],
                     [{x2},{y2}],[{x},{y2}],[{x},{y}]]]}}}}"#,
                x2 = x + 0.04,
                y2 = y + 0.04,
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
    output: PathBuf,
}

/// Points, plus a MultiPoint whose parts converge as the pyramid halves.
/// Points merge by their own path, deduplicating through a set, which needs the
/// same proof.
fn points() -> String {
    let mut features = Vec::new();
    for i in 0..400i64 {
        let x = 172.0 + (i % 20) as f64 * 0.05;
        let y = -44.0 + (i / 20) as f64 * 0.05;
        features.push(format!(
            r#"{{"type":"Feature","id":{i},"properties":{{"name":"point {i}"}},
               "geometry":{{"type":"Point","coordinates":[{x},{y}]}}}}"#
        ));
    }
    features.push(
        r#"{"type":"Feature","id":900,"properties":{"name":"pair"},
           "geometry":{"type":"MultiPoint","coordinates":[[172.5,-43.5],[172.51,-43.51]]}}"#
            .to_string(),
    );
    format!(
        r#"{{"type":"FeatureCollection","features":[{}]}}"#,
        features.join(",")
    )
}

fn build(threads: usize) -> Built {
    build_from(threads, source())
}

fn build_from(threads: usize, body: String) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("squares.geojson");
    std::fs::write(&input, body).unwrap();
    let output = dir.path().join("squares.pmtiles");
    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output: output.clone(),
        min_zoom: 0,
        max_zoom: Some(11),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "squares".to_string(),
        output_name: None,
        attributes: None,
        extent: 4096,
        maxzoom_extent: Some(8192),
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        dust: true,
        threads,
        limits: Limits {
            bytes: 0,
            features: 0,
        },
        // No vertex budget, which would coarsen the geometry.
        max_tile_vertices: 0,
        // A real buffer, so the nine-tile read happens.
        buffer: 64,
        keep_store: false,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    run(&options).unwrap();
    Built { _dir: dir, output }
}

#[test]
fn the_number_of_workers_does_not_change_the_archive() {
    let one = std::fs::read(build(1).output).unwrap();
    let eight = std::fs::read(build(8).output).unwrap();
    assert_eq!(
        one.len(),
        eight.len(),
        "one worker produced {} bytes and eight produced {}",
        one.len(),
        eight.len()
    );
    assert!(one == eight, "the archives differ byte for byte");
}

#[test]
fn an_odd_number_of_workers_agrees_too() {
    // Three workers split nothing evenly, exposing off-by-ones.
    assert_eq!(
        std::fs::read(build(1).output).unwrap(),
        std::fs::read(build(3).output).unwrap()
    );
}

#[test]
fn a_point_layer_does_not_depend_on_the_worker_count_either() {
    assert_eq!(
        std::fs::read(build_from(1, points()).output).unwrap(),
        std::fs::read(build_from(8, points()).output).unwrap()
    );
}

#[test]
fn more_workers_than_tiles_is_harmless() {
    // Workers that find no work must exit, not hang.
    assert_eq!(
        std::fs::read(build(1).output).unwrap(),
        std::fs::read(build(64).output).unwrap()
    );
}
