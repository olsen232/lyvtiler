//! The vertex budget through the real pipeline. Unlike the byte and feature
//! budgets it drops nothing: it simplifies the levels above a breach as though
//! they were a zoom higher. Checked:
//!
//! - maxzoom is never coarsened;
//! - levels above the breach lose vertices;
//! - no feature is dropped at any level;
//! - a budget nothing breaches changes nothing.

use std::collections::{BTreeMap, HashSet};

use lyv::{Limits, Options, run};
use lyv_store::{SqliteStore, TileStore};

const EXTENT: u32 = 4096;
const MAX_ZOOM: u32 = 6;

/// Wiggly lines with far more vertices than shape. Lines, because squares have
/// nothing to simplify. `sin` of successive integers is effectively noise, so
/// the wiggle survives a fine tolerance and only a coarsened one flattens it.
fn source(lines: i64, points: i64) -> String {
    let mut features = Vec::new();
    for line in 0..lines {
        let base = -44.0 + (line as f64) * 2.0 / (lines as f64);
        let coordinates: Vec<String> = (0..points)
            .map(|point| {
                let x = 172.0 + (point as f64) * 2.0 / (points as f64);
                let y = base + 0.05 * (point as f64).sin();
                format!("[{x},{y}]")
            })
            .collect();
        features.push(format!(
            r#"{{"type":"Feature","id":{line},"properties":{{}},"geometry":
               {{"type":"LineString","coordinates":[{}]}}}}"#,
            coordinates.join(",")
        ));
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

fn build(max_tile_vertices: usize) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("wiggles.geojson");
    std::fs::write(&input, source(30, 300)).unwrap();
    let output = dir.path().join("wiggles.pmtiles");

    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(MAX_ZOOM),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "wiggles".to_string(),
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
        // No feature limits, so any change is the vertex budget's.
        limits: Limits {
            bytes: 0,
            features: 0,
        },
        max_tile_vertices,
        buffer: 0,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    run(&options).unwrap();
    Built { _dir: dir, options }
}

/// Vertices and distinct features at each zoom, from the pyramid.
fn levels(built: &Built) -> BTreeMap<u32, (usize, HashSet<u64>)> {
    let store = SqliteStore::open(&built.options.store).unwrap();
    let mut found = BTreeMap::new();
    for z in built.options.min_zoom..=built.options.deepest() {
        let (vertices, features) = found.entry(z).or_insert((0usize, HashSet::new()));
        store
            .for_each_at(z, &mut |_, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    *vertices += chain.points.len();
                    features.insert(chain.feature);
                }
                Ok(())
            })
            .unwrap();
    }
    found
}

#[test]
fn a_budget_nothing_breaches_changes_nothing() {
    // A budget nothing reaches: every level matches a run without one.
    let off = levels(&build(0));
    let generous = levels(&build(1_000_000));
    for z in 0..=MAX_ZOOM {
        assert_eq!(off[&z].0, generous[&z].0, "zoom {z} changed");
    }
}

#[test]
fn a_tight_budget_coarsens_the_levels_above_the_breach() {
    let loose = levels(&build(0));
    let tight = levels(&build(2_000));

    // Maxzoom is untouched.
    assert_eq!(
        loose[&MAX_ZOOM].0, tight[&MAX_ZOOM].0,
        "maxzoom should never be coarsened by the budget"
    );

    // Every level above it holds less geometry.
    let mut coarsened = 0;
    for z in 0..MAX_ZOOM {
        assert!(
            tight[&z].0 <= loose[&z].0,
            "zoom {z} gained vertices under a tighter budget: {} against {}",
            tight[&z].0,
            loose[&z].0
        );
        if tight[&z].0 < loose[&z].0 {
            coarsened += 1;
        }
    }
    assert!(
        coarsened > 0,
        "a 2,000-vertex budget coarsened nothing; the source is no longer \
         dense enough to breach it"
    );
}

#[test]
fn the_vertex_budget_drops_no_features() {
    // Simplification never reduces the feature count: every algorithm keeps
    // the ends of a line.
    let loose = levels(&build(0));
    let tight = levels(&build(2_000));
    for z in 0..=MAX_ZOOM {
        assert_eq!(
            loose[&z].1, tight[&z].1,
            "zoom {z} lost features to the vertex budget"
        );
    }
}
