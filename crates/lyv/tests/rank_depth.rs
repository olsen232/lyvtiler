//! Bucketing the ranking deeper than maxzoom.
//!
//! Within one bucket the weighted draw favours large features but guarantees
//! small ones nothing. Deeper buckets give a floor: one feature per fine tile
//! per round, whatever its size. The fixture puts every square in one maxzoom
//! tile, shrinking sharply west to east (the last column is 1/17,000 of the
//! first's weight).

use std::collections::HashSet;

use lyv::{Limits, Options, run};
use lyv_store::{SqliteStore, TileStore};

/// Squares in one maxzoom tile, shrinking west to east, so largest means
/// westmost.
fn source() -> String {
    let mut features = Vec::new();
    for row in 0..16i64 {
        for column in 0..16i64 {
            let id = row * 16 + column;
            let x = 172.0 + (column as f64) * 0.002;
            let y = -44.0 + (row as f64) * 0.002;
            // Widest in the west.
            let w = 0.0016 * 0.85f64.powi(column as i32);
            features.push(format!(
                r#"{{"type":"Feature","id":{id},"properties":{{}},"geometry":
                   {{"type":"Polygon","coordinates":[[[{x},{y}],[{x2},{y}],
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

fn build(rank_depth: u32, limits: Limits) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("squares.geojson");
    std::fs::write(&input, source()).unwrap();
    let output = dir.path().join("squares.pmtiles");
    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(12),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "squares".to_string(),
        output_name: None,
        attributes: None,
        extent: 4096,
        maxzoom_extent: None,
        ranker: "evenstar".to_string(),
        rank_depth,
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

/// The features surviving at `z`.
fn survivors(built: &Built, z: u32) -> HashSet<u64> {
    let store = SqliteStore::open(&built.options.store).unwrap();
    let mut found = HashSet::new();
    store
        .for_each_at(z, &mut |_, data| {
            for chain in lyv_mvt::decode_chains(&data).unwrap() {
                found.insert(chain.feature);
            }
            Ok(())
        })
        .unwrap();
    found
}

/// How many of the eight columns still hold a square (column is `id % 8`, west
/// to east, largest to smallest).
fn columns_alive(kept: &HashSet<u64>) -> usize {
    kept.iter()
        .map(|id| id % 16)
        .collect::<HashSet<u64>>()
        .len()
}

/// A budget that forces most of the 64 squares out.
const TIGHT: Limits = Limits {
    bytes: 0,
    features: 16,
};

#[test]
fn bucketing_at_maxzoom_alone_leaves_the_small_end_thin() {
    // Without depth, pinned as the baseline: the draw thins the east rather
    // than erasing it, but with weights four orders of magnitude apart most of
    // it goes.
    let built = build(0, TIGHT);
    let alive = columns_alive(&survivors(&built, 12));
    assert!(
        alive <= 8,
        "expected the draw to concentrate on the large end, got {alive} of 16 columns"
    );
}

#[test]
fn bucketing_deeper_spreads_the_survivors_across_the_tile() {
    let shallow = columns_alive(&survivors(&build(0, TIGHT), 12));
    let deep = columns_alive(&survivors(&build(4, TIGHT), 12));
    // A clear margin, not one lucky draw.
    assert!(
        deep >= shallow + 3,
        "depth 4 reached {deep} of 16 columns against depth 0's {shallow}, \
         which is too close to call"
    );
}

#[test]
fn depth_changes_the_ranking_without_changing_what_is_in_the_tileset() {
    // Depth changes which features survive a budget, nothing else: with no
    // budget the pyramids match.
    let unlimited = Limits {
        bytes: 0,
        features: 0,
    };
    assert_eq!(
        survivors(&build(0, unlimited), 12),
        survivors(&build(4, unlimited), 12),
        "depth changed the tileset when nothing was being dropped"
    );
}

#[test]
fn a_depth_too_deep_for_the_extent_is_trimmed_rather_than_refused() {
    // Depth is capped by the extent; asking for more still builds.
    let built = build(30, TIGHT);
    assert!(!survivors(&built, 12).is_empty());
}
