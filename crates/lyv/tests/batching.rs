//! How much the archive writer holds at once.
//!
//! Tiles are gzipped in parallel batches, held both uncompressed and
//! compressed, so the batch size can set peak memory. A tile count alone
//! doesn't bound it — tiles range from kilobytes to megabytes, and the count
//! scales with cores — so a byte ceiling closes the batch too.
//!
//! Invisible on small data: on 3.5M parcels (a z9 of 87 tiles up to 4 MB) it's
//! worth 210 MB; on everything smaller, under 3%.

use lyv::batch_is_full;

const TILES: usize = 80;
const BYTES: usize = 32 * 1024 * 1024;

#[test]
fn an_empty_batch_is_never_full() {
    assert!(!batch_is_full(0, 0, TILES, BYTES));
}

#[test]
fn ordinary_tiles_fill_it_by_count() {
    // Ordinary tiles: eighty are far below the byte ceiling.
    let each = 40 * 1024;
    assert!(!batch_is_full(79, 79 * each, TILES, BYTES));
    assert!(batch_is_full(80, 80 * each, TILES, BYTES));
}

#[test]
fn large_tiles_fill_it_by_size_long_before_the_count() {
    // Eight 4 MB tiles reach the ceiling; eighty would be 320 MB, held twice.
    let each = 4 * 1024 * 1024;
    assert!(!batch_is_full(7, 7 * each, TILES, BYTES));
    assert!(batch_is_full(8, 8 * each, TILES, BYTES));
}

#[test]
fn one_tile_larger_than_the_whole_budget_still_closes_the_batch() {
    // A threshold, not a quota, or one huge tile would wait forever.
    assert!(batch_is_full(1, BYTES * 3, TILES, BYTES));
}

#[test]
fn a_zero_limit_means_no_limit_on_that_axis() {
    assert!(!batch_is_full(1_000_000, 0, 0, BYTES), "no count limit");
    assert!(!batch_is_full(0, usize::MAX / 2, TILES, 0), "no byte limit");
    assert!(!batch_is_full(1_000_000, usize::MAX / 2, 0, 0), "neither");
}

#[test]
fn the_pmtiles_writer_declares_both_limits() {
    // The byte ceiling must not scale with the worker count.
    let dir = tempfile::tempdir().unwrap();
    let writer = lyv_archive::create(
        &dir.path().join("out.pmtiles"),
        &lyv_archive::Metadata {
            name: "t".into(),
            generator: None,
            min_zoom: 0,
            max_zoom: 1,
            bounds: None,
            antimeridian_adjusted_bounds: None,
            center: None,
            layers: Vec::new(),
        },
    )
    .unwrap();
    assert!(writer.batch_size() > 0, "it batches");
    assert_eq!(writer.batch_bytes(), BYTES);
}
