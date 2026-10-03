//! Dropping features to fit a tile budget, through the real pipeline. Other
//! tests run without limits; this one forces dropping and checks the
//! properties that make it safe (§8):
//!
//! - something is dropped when the budget demands it, nothing when it doesn't;
//! - **monotone in zoom**: a feature present at a zoom is present at every
//!   deeper one;
//! - **uniform across a level**: survival doesn't depend on which tile a
//!   feature is in.

use std::collections::{BTreeMap, HashSet};

use lyv::{Limits, Options, run};
use lyv_store::{SqliteStore, TileStore};

const EXTENT: u32 = 4096;

/// Small squares over New Zealand, numerous enough to exceed a feature budget
/// with no single one dominating.
fn source(across: i64) -> String {
    let mut features = Vec::new();
    for row in 0..across {
        for column in 0..across {
            let id = row * across + column;
            // Spread over four degrees, so they fall in several tiles from
            // zoom 4.
            let x = 172.0 + (column as f64) * 4.0 / (across as f64);
            let y = -44.0 + (row as f64) * 4.0 / (across as f64);
            let w = 1.6 / (across as f64);
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

fn build(across: i64, limits: Limits) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("squares.geojson");
    std::fs::write(&input, source(across)).unwrap();
    let output = dir.path().join("squares.pmtiles");

    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(6),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "squares".to_string(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        input_crs: None,
        regular_angles_check: true,
        extent: EXTENT,
        maxzoom_extent: None,
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        dust: true,
        threads: 1,
        // Kept, since these tests read the store.
        keep_store: true,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        limits,
        // No vertex budget: these tests vary the feature budget.
        max_tile_vertices: 0,
        buffer: 0,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    run(&options).unwrap();
    Built { _dir: dir, options }
}

/// Which features survive at each zoom, from the stored pyramid.
fn survivors(built: &Built) -> BTreeMap<u32, HashSet<u64>> {
    let store = SqliteStore::open(&built.options.store).unwrap();
    let mut found: BTreeMap<u32, HashSet<u64>> = BTreeMap::new();
    for z in built.options.min_zoom..=built.options.deepest() {
        let level = found.entry(z).or_default();
        store
            .for_each_at(z, &mut |_, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    level.insert(chain.feature);
                }
                Ok(())
            })
            .unwrap();
    }
    found
}

const UNLIMITED: Limits = Limits {
    bytes: 0,
    features: 0,
};

#[test]
fn nothing_is_dropped_when_everything_fits() {
    let built = build(12, UNLIMITED);
    let found = survivors(&built);
    assert_eq!(
        found[&6].len(),
        144,
        "all 144 squares should reach the deepest zoom"
    );
}

#[test]
fn a_tight_feature_budget_drops_features() {
    // Compared with an unlimited run, not with 144: features also collapse on
    // the way up, which isn't dropping.
    let loose = survivors(&build(12, UNLIMITED));
    let tight = survivors(&build(
        12,
        Limits {
            bytes: 0,
            features: 10,
        },
    ));

    // Maxzoom is thinned too: it has no deeper level to be corrected from.
    assert!(
        tight[&6].len() < loose[&6].len(),
        "maxzoom kept {} features under a 10-feature budget, the same as none",
        tight[&6].len()
    );
    assert!(
        tight[&0].len() < loose[&0].len(),
        "a 10-feature budget kept {} features at zoom 0, as many as no budget at all",
        tight[&0].len()
    );
    assert!(
        !tight[&0].is_empty(),
        "zoom 0 was emptied entirely; something should always survive"
    );
}

#[test]
fn a_feature_dropped_at_one_zoom_is_gone_from_every_shallower_one() {
    // Zooming out may lose features, never gain one.
    let built = build(
        12,
        Limits {
            bytes: 0,
            features: 10,
        },
    );
    let found = survivors(&built);
    for z in built.options.min_zoom..built.options.deepest() {
        let shallow = &found[&z];
        let deeper = &found[&(z + 1)];
        let reappeared: Vec<u64> = shallow.difference(deeper).copied().collect();
        assert!(
            reappeared.is_empty(),
            "features {reappeared:?} are at zoom {z} but not at zoom {}",
            z + 1
        );
    }
}

#[test]
fn the_same_features_are_dropped_everywhere_in_a_level() {
    // A feature clipped into several tiles is in all of them or none.
    let built = build(
        8,
        Limits {
            bytes: 0,
            features: 6,
        },
    );
    let store = SqliteStore::open(&built.options.store).unwrap();

    for z in 0..=built.options.deepest() {
        // The tiles each feature is in, at this zoom.
        let mut present: BTreeMap<u64, HashSet<(u32, u32)>> = BTreeMap::new();
        store
            .for_each_at(z, &mut |coord, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    present
                        .entry(chain.feature)
                        .or_default()
                        .insert((coord.x, coord.y));
                }
                Ok(())
            })
            .unwrap();

        // Checked against the deeper level: a feature in a child tile must be
        // in its parent.
        if z == built.options.deepest() {
            continue;
        }
        let mut wanted: BTreeMap<u64, HashSet<(u32, u32)>> = BTreeMap::new();
        store
            .for_each_at(z + 1, &mut |coord, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    wanted
                        .entry(chain.feature)
                        .or_default()
                        .insert((coord.x / 2, coord.y / 2));
                }
                Ok(())
            })
            .unwrap();

        for (feature, parents) in wanted {
            let Some(here) = present.get(&feature) else {
                continue; // dropped at this zoom, which is allowed
            };
            let missing: Vec<(u32, u32)> = parents.difference(here).copied().collect();
            assert!(
                missing.is_empty(),
                "feature {feature} survived zoom {z} but is missing from tiles {missing:?}"
            );
        }
    }
}
