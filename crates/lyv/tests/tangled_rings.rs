//! A feature whose pieces won't close back to its own area is dropped, rather
//! than drawn as a tile-sized rectangle.
//!
//! A self-intersecting ring can leave a piece whose ends are ordered as a
//! hole's while the ring is an exterior; closing then returns the tile's
//! complement. The check is area conservation during the cut: a ring's closed
//! pieces must sum to its own area.
//!
//! Both fixtures come from a US floodplains layer. 3256752's complement came
//! out *over* a tile's area; 722822's came out one unit *under*, which is why
//! the check is conservation, not a size limit on pieces.

use std::collections::HashSet;

use lyv::{Limits, Options, run};
use lyv_store::{SqliteStore, TileStore};

const Z: u32 = 14;
const EXTENT: u32 = 4096;

/// A lon/lat that projects onto the maxzoom grid at `(x, y)`. The fixtures are
/// positioned in grid units — a few units at zoom 14 is the eighth decimal of a
/// degree.
fn at(x: i64, y: i64) -> String {
    let (lon, lat) = lyv_core::mercator::unproject_global(x, y, Z, EXTENT);
    format!("[{lon:.12},{lat:.12}]")
}

/// A plain square, and a tall thin ring whose left side crosses past its right
/// side, straddling a maxzoom tile boundary. (Inside a tile the crossover would
/// be harmless.)
fn source() -> String {
    // A tile boundary near 172E, 44S.
    let origin = lyv_core::mercator::project_global(172.0, -44.0, Z, EXTENT);
    let boundary = (origin.0 / i64::from(EXTENT) + 1) * i64::from(EXTENT);
    let top = origin.1;

    // 180 units wide, right side 20 inside the boundary, the crossover 60
    // past it: plenty of margin for the degree round trip.
    let (left, right, over) = (boundary - 200, boundary - 20, boundary + 60);

    // A row boundary, for the needle to cross.
    let row = (origin.1 / i64::from(EXTENT) + 1) * i64::from(EXTENT);
    let nx = boundary - 3000;

    format!(
        r#"{{"type":"FeatureCollection","features":[
          {{"type":"Feature","id":1,"properties":{{}},"geometry":
            {{"type":"Polygon","coordinates":[[{s0},{s1},{s2},{s3},{s0}]]}}}},
          {{"type":"Feature","id":2,"properties":{{}},"geometry":
            {{"type":"Polygon","coordinates":[[{t0},{t1},{t2},{t3},{t4},{t5},{t6},{t0}]]}}}},
          {{"type":"Feature","id":3,"properties":{{}},"geometry":
            {{"type":"Polygon","coordinates":[[{n0},{n1},{n2},{n3},{n4},{n0}]]}}}}
        ]}}"#,
        // Feature 3, shaped like 722822: a needle enclosing 12 that crosses a
        // row boundary and doubles back. Its pieces close to two tiles, the
        // larger one unit *under* a tile's area.
        n0 = at(nx + 8, row - 3),
        n1 = at(nx + 4, row + 13),
        n2 = at(nx + 11, row - 7),
        n3 = at(nx + 1, row + 1),
        n4 = at(nx - 14, row + 1),
        // A plain square elsewhere, which must survive.
        s0 = at(boundary - 6000, top),
        s1 = at(boundary - 5000, top),
        s2 = at(boundary - 5000, top + 1000),
        s3 = at(boundary - 6000, top + 1000),
        // Down the right side, along the bottom, back up the left — with one
        // left-side vertex crossed past the right side and the boundary.
        t0 = at(left, top + 2000),
        t1 = at(right, top + 2000),
        t2 = at(right, top + 4000),
        t3 = at(left, top + 4000),
        t4 = at(left, top + 3600),
        t5 = at(over, top + 3000),
        t6 = at(left, top + 2400),
    )
}

struct Built {
    _dir: tempfile::TempDir,
    options: Options,
}

fn build() -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.geojson");
    std::fs::write(&input, source()).unwrap();
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
        input_crs: None,
        extent: 4096,
        maxzoom_extent: None,
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        threads: 1,
        dust: true,
        // No limits: nothing survives by fitting.
        limits: Limits {
            bytes: 0,
            features: 0,
        },
        max_tile_vertices: 0,
        buffer: 0,
        keep_store: true,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        // Off: the fault is in cutting and closing. On, a tuning change could
        // move the fixture.
        simplification: lyv_simplify::Tolerance::NONE,
    };
    run(&options).unwrap();
    Built { _dir: dir, options }
}

/// Doubled shoelace area. Positive is clockwise on screen, consistent with MVT.
fn double_area(ring: &[geo_types::Coord<lyv_core::Local>]) -> i64 {
    ring.windows(2)
        .map(|w| i64::from(w[0].x) * i64::from(w[1].y) - i64::from(w[1].x) * i64::from(w[0].y))
        .sum()
}

#[test]
fn no_tile_holds_a_ring_enclosing_more_than_the_tile() {
    // Checked at every zoom: a bad maxzoom ring would be merged upward.
    let built = build();
    let store = SqliteStore::open(&built.options.store).unwrap();

    for z in 0..=14 {
        let extent = i64::from(built.options.extent_at(z));
        let tile = 2 * extent * extent;
        store
            .for_each_at(z, &mut |coord, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    if !lyv_seam::is_ring(&chain.points) {
                        continue;
                    }
                    let area = double_area(&chain.points).abs();
                    assert!(
                        area <= tile,
                        "z{z} ({},{}) feature {} encloses {area}, more than the tile's {tile}",
                        coord.x,
                        coord.y,
                        chain.feature,
                    );
                }
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn the_tangled_feature_is_dropped_and_its_neighbour_is_not() {
    // Dropped everywhere: its other pieces were closed by the same untrusted
    // rule.
    let built = build();
    let store = SqliteStore::open(&built.options.store).unwrap();

    let mut found = HashSet::new();
    store
        .for_each_at(14, &mut |_, data| {
            for chain in lyv_mvt::decode_chains(&data).unwrap() {
                found.insert(chain.feature);
            }
            Ok(())
        })
        .unwrap();

    assert!(
        found.contains(&1),
        "the well-formed square should be untouched, found {found:?}"
    );
    assert!(
        !found.contains(&2),
        "the ring whose left side crossed over should be gone, found {found:?}"
    );
    assert!(
        !found.contains(&3),
        "the needle should be gone too, found {found:?}"
    );
}

#[test]
fn the_complement_is_caught_even_when_it_comes_out_under_a_tile() {
    // The needle closes to tile minus sliver, the other ring to tile plus
    // sliver: a size limit on pieces would catch only the second.
    let needle = [(0i64, 9i64), (6, -5), (9, -7), (10, -8), (10, -8), (7, -5)];
    let mut ring: Vec<geo_types::Coord<i64>> = needle
        .iter()
        .map(|&(x, y)| geo_types::Coord { x, y })
        .collect();
    ring.push(ring[0]);

    let doubled: i64 = ring
        .windows(2)
        .map(|w| w[0].x * w[1].y - w[1].x * w[0].y)
        .sum();
    assert_eq!(doubled, 16, "the needle encloses almost nothing");

    let built = build();
    let store = SqliteStore::open(&built.options.store).unwrap();
    let extent = i64::from(built.options.extent_at(14));
    let tile = 2 * extent * extent;

    // Nothing near a tile's area survived.
    store
        .for_each_at(14, &mut |coord, data| {
            for chain in lyv_mvt::decode_chains(&data).unwrap() {
                let area = double_area(&chain.points).abs();
                assert!(
                    area < tile / 2,
                    "z14 ({},{}) feature {} encloses {area}, near the tile's {tile}",
                    coord.x,
                    coord.y,
                    chain.feature,
                );
            }
            Ok(())
        })
        .unwrap();
}
