//! Features too small for a zoom become a speck rather than nothing, so a
//! layer of small parcels thins out instead of vanishing as you zoom out.
//!
//! A collapsed ring becomes the smallest square the grid can hold. A collapsed
//! line becomes the shortest segment, still pointing the way it ran — sliding
//! back a unit rather than reversing if it would leave the tile. Points can't
//! collapse.

use std::collections::HashSet;

use lyv::{Limits, Options, run};
use lyv_store::{SqliteStore, TileStore};

/// Squares small enough to collapse a few levels up, spread so they don't
/// merge.
fn source(across: i64) -> String {
    let mut features = Vec::new();
    for row in 0..across {
        for column in 0..across {
            let id = row * across + column;
            let x = 172.0 + (column as f64) * 0.02;
            let y = -44.0 + (row as f64) * 0.02;
            // About 20 m: fine at zoom 14, gone before zoom 6.
            let w = 0.0002;
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

fn build(dust: bool, geometry: &str) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.geojson");
    std::fs::write(&input, geometry).unwrap();
    let output = dir.path().join("out.pmtiles");
    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(14),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "l".to_string(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        extent: 4096,
        maxzoom_extent: None,
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        threads: 1,
        dust,
        // No limits, so survivors survived on their own.
        limits: Limits {
            bytes: 0,
            features: 0,
        },
        // No vertex budget, which would coarsen the geometry.
        max_tile_vertices: 0,
        buffer: 0,
        keep_store: true,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        simplification: lyv_simplify::Tolerance::Distance(1.0),
    };
    run(&options).unwrap();
    Built { _dir: dir, options }
}

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

#[test]
fn without_dust_a_layer_of_small_polygons_evaporates() {
    // Without dust, pinned as the baseline.
    let built = build(false, &source(8));
    assert_eq!(survivors(&built, 14).len(), 64, "all present at maxzoom");
    assert!(
        survivors(&built, 6).len() < 8,
        "expected almost nothing to survive to zoom 6 without dust, got {}",
        survivors(&built, 6).len()
    );
}

#[test]
fn with_dust_they_survive_as_specks() {
    let built = build(true, &source(8));
    assert_eq!(survivors(&built, 14).len(), 64);
    // All of them, down to one world tile.
    for z in 0..=14 {
        assert_eq!(survivors(&built, z).len(), 64, "zoom {z} lost some of them");
    }
}

#[test]
fn a_speck_is_a_real_ring_with_area() {
    // A real square: a speck with no area reads as a hole.
    let built = build(true, &source(4));
    let store = SqliteStore::open(&built.options.store).unwrap();
    let mut checked = 0;
    store
        .for_each_at(6, &mut |_, data| {
            for chain in lyv_mvt::decode_chains(&data).unwrap() {
                let points = &chain.points;
                assert!(points.len() >= 4, "a ring needs four points: {points:?}");
                assert_eq!(points.first(), points.last(), "not closed: {points:?}");
                assert!(
                    lyv_seam::double_area(points) > 0,
                    "a speck must be a positively wound exterior: {points:?}"
                );
                checked += 1;
            }
            Ok(())
        })
        .unwrap();
    assert!(checked > 0);
}

#[test]
fn a_speck_stays_inside_its_tile() {
    // A speck on the tile's far edge stays inside the tile.
    let built = build(true, &source(6));
    let store = SqliteStore::open(&built.options.store).unwrap();
    for z in 0..=14 {
        store
            .for_each_at(z, &mut |coord, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    for p in &chain.points {
                        assert!(
                            (0..=4096).contains(&p.x) && (0..=4096).contains(&p.y),
                            "tile {}/{}/{} holds {p:?}",
                            coord.z,
                            coord.x,
                            coord.y
                        );
                    }
                }
                Ok(())
            })
            .unwrap();
    }
}

/// A line short enough to collapse well before zoom 0.
const SHORT_LINE: &str = r#"{"type":"FeatureCollection","features":[
    {"type":"Feature","id":1,"properties":{},"geometry":{"type":"LineString",
     "coordinates":[[172.0,-44.0],[172.0006,-44.0006]]}}]}"#;

#[test]
fn without_dust_a_collapsing_line_disappears() {
    let built = build(false, SHORT_LINE);
    assert!(
        survivors(&built, 0).is_empty(),
        "the line should be gone by zoom 0 with dust off"
    );
    assert!(
        survivors(&built, 14).contains(&1),
        "and present at maxzoom, or the fixture proves nothing"
    );
}

#[test]
fn with_dust_a_collapsing_line_survives_as_a_stub() {
    let built = build(true, SHORT_LINE);
    for z in 0..=14 {
        assert!(
            survivors(&built, z).contains(&1),
            "the line is missing at zoom {z}"
        );
    }
}

#[test]
fn a_stub_is_a_real_segment_with_length() {
    // Two distinct points: a one-point stub would cost bytes and draw nothing.
    let built = build(true, SHORT_LINE);
    let store = SqliteStore::open(&built.options.store).unwrap();
    let mut checked = 0;
    store
        .for_each_at(0, &mut |_, data| {
            for chain in lyv_mvt::decode_chains(&data).unwrap() {
                assert!(chain.points.len() >= 2, "{:?}", chain.points);
                assert_ne!(
                    chain.points.first(),
                    chain.points.last(),
                    "the stub has no length: {:?}",
                    chain.points
                );
                checked += 1;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(checked, 1);
}

#[test]
fn a_stub_still_points_the_way_the_line_ran() {
    // The source runs south-east (tile y grows southward); the stub must too.
    let built = build(true, SHORT_LINE);
    let store = SqliteStore::open(&built.options.store).unwrap();
    for z in 0..=14 {
        store
            .for_each_at(z, &mut |coord, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    let (a, b) = (chain.points[0], chain.points[chain.points.len() - 1]);
                    assert!(
                        b.x >= a.x && b.y >= a.y,
                        "zoom {} tile {}/{} runs the wrong way: {:?}",
                        coord.z,
                        coord.x,
                        coord.y,
                        chain.points
                    );
                }
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn lines_are_not_given_area() {
    // A line has no area; however short, it stays a line.
    let line = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":1,"properties":{},"geometry":{"type":"LineString",
         "coordinates":[[172.0,-44.0],[172.0001,-44.0001]]}}]}"#;
    let built = build(true, line);
    let store = SqliteStore::open(&built.options.store).unwrap();
    for z in 0..=14 {
        store
            .for_each_at(z, &mut |_, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    assert!(
                        chain.points.first() != chain.points.last() || chain.points.len() < 4,
                        "a line became a ring: {:?}",
                        chain.points
                    );
                }
                Ok(())
            })
            .unwrap();
    }
}
