//! A run that finishes deletes its store; a run that fails keeps it. A stale
//! store is large, and makes the next run with different settings refuse.

use std::path::{Path, PathBuf};

use lyv::{Limits, Options, run};

fn source() -> &'static str {
    r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":1,"properties":{},"geometry":{"type":"LineString",
         "coordinates":[[174.0,-41.0],[175.0,-40.0]]}}
    ]}"#
}

struct Job {
    _dir: tempfile::TempDir,
    options: Options,
}

fn job(max_zoom: u32, keep_store: bool) -> Job {
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
        output_layer: "l".to_string(),
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
        limits: Limits {
            bytes: 0,
            features: 0,
        },
        // No vertex budget, which would coarsen the geometry.
        max_tile_vertices: 0,
        buffer: 0,
        keep_store,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    Job { _dir: dir, options }
}

/// The store and SQLite's files beside it.
fn store_files(store: &Path) -> Vec<PathBuf> {
    let mut name = store.as_os_str().to_owned();
    name.push("-wal");
    let wal = PathBuf::from(name);
    let mut name = store.as_os_str().to_owned();
    name.push("-shm");
    let shm = PathBuf::from(name);
    [store.to_path_buf(), wal, shm]
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

#[test]
fn a_finished_run_leaves_no_store_behind() {
    let job = job(4, false);
    run(&job.options).unwrap();

    assert!(job.options.output.exists(), "the archive should be written");
    let left = store_files(&job.options.store);
    assert!(
        left.is_empty(),
        "a finished run left {left:?} behind; there is nothing to resume from"
    );
}

#[test]
fn keeping_the_store_is_available_for_asking() {
    // `--keep-store` keeps it, for `inspect` and `rank_stats`.
    let job = job(4, true);
    run(&job.options).unwrap();
    assert!(
        !store_files(&job.options.store).is_empty(),
        "--keep-store did not keep the store"
    );
}

#[test]
fn a_failed_run_keeps_its_store() {
    // A failed run keeps its store.
    let job = job(4, false);
    let mut broken = job.options.clone();
    broken.output = job.options.output.with_extension("geojson"); // no such writer

    assert!(
        run(&broken).is_err(),
        "an unknown output format should fail"
    );
    assert!(
        !store_files(&broken.store).is_empty(),
        "a failed run deleted the store it should have kept"
    );
}

#[test]
fn a_second_run_after_a_clean_finish_does_not_trip_on_the_old_parameters() {
    // A stale store must not make the next run with a different maxzoom
    // refuse.
    let job = job(4, false);
    run(&job.options).unwrap();

    let mut deeper = job.options.clone();
    deeper.max_zoom = Some(5);
    run(&deeper).expect("a deeper rebuild should just work once the store is gone");
}
