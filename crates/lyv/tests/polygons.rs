//! Polygons through the real pipeline, GeoJSON to pyramid. `nested_polygons.rs`
//! in lyv-merge checks the algorithms; this checks the wiring — reading, cut,
//! close and fill in the right order, several features per tile, and area
//! surviving to zoom 0.

use std::path::PathBuf;

use lyv::{Options, run};
use lyv_core::{Local, TileCoord};
use lyv_store::{SqliteStore, TileStore};

const EXTENT: u32 = 4096;
const E: i64 = EXTENT as i64;

/// A lake with an island, and a river crossing both. The river shares tiles
/// with the lake, and as a `Chain` a line crossing a tile looks just like a
/// clipped ring, so it's the thing most likely to be closed by mistake.
fn source() -> &'static str {
    r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":1,"properties":{},"geometry":{"type":"Polygon",
         "coordinates":[
           [[174.0,-41.6],[176.0,-41.6],[176.0,-40.4],[174.0,-40.4],[174.0,-41.6]],
           [[174.6,-41.3],[175.4,-41.3],[175.4,-40.7],[174.6,-40.7],[174.6,-41.3]]
         ]}},
        {"type":"Feature","id":2,"properties":{},"geometry":{"type":"LineString",
         "coordinates":[[173.5,-41.9],[175.0,-41.0],[176.5,-40.1]]}}
    ]}"#
}

struct Built {
    _dir: tempfile::TempDir,
    options: Options,
}

fn build(max_zoom: u32, simplify: lyv_simplify::Tolerance, buffer: u32) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("water.geojson");
    std::fs::write(&input, source()).unwrap();
    let output = dir.path().join("water.pmtiles");

    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(max_zoom),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "water".to_string(),
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
        // No limits: these tests check exact geometry.
        limits: lyv::Limits {
            bytes: 0,
            features: 0,
        },
        // No vertex budget, which would coarsen the geometry.
        max_tile_vertices: 0,
        buffer,
        simplification: simplify,
    };
    run(&options).expect("the pipeline should run");
    Built { _dir: dir, options }
}

fn store_of(built: &Built) -> SqliteStore {
    SqliteStore::open(&built.options.store).unwrap()
}

/// Doubled shoelace area of a ring in global coordinates at its own zoom.
fn double_area(ring: &[(i64, i64)]) -> i128 {
    ring.windows(2)
        .map(|w| i128::from(w[0].0) * i128::from(w[1].1) - i128::from(w[1].0) * i128::from(w[0].1))
        .sum()
}

/// Every closed ring at a zoom, in global coordinates, and how many chains
/// weren't closed.
fn rings_at(store: &SqliteStore, z: u32) -> (Vec<Vec<(i64, i64)>>, usize) {
    let mut rings = Vec::new();
    let mut open = 0usize;
    store
        .for_each_at(z, &mut |coord: TileCoord, data| {
            for chain in lyv_mvt::decode_chains(&data).unwrap() {
                let global: Vec<(i64, i64)> = chain
                    .points
                    .iter()
                    .map(|p| {
                        (
                            i64::from(coord.x) * E + i64::from(p.x),
                            i64::from(coord.y) * E + i64::from(p.y),
                        )
                    })
                    .collect();
                if chain.points.first() == chain.points.last() {
                    rings.push(global);
                } else {
                    open += 1;
                }
            }
            Ok(())
        })
        .unwrap();
    (rings, open)
}

/// The lake's true doubled area at a zoom, from the source coordinates.
fn expected_double_area(z: u32) -> f64 {
    let project = |lon: f64, lat: f64| {
        let (x, y) = lyv_core::mercator::project_global(lon, lat, z, EXTENT);
        (x as f64, y as f64)
    };
    let ring = |pts: &[(f64, f64)]| -> f64 {
        let projected: Vec<(f64, f64)> = pts.iter().map(|&(a, b)| project(a, b)).collect();
        let mut total = 0.0;
        for i in 0..projected.len() {
            let (a, b) = (projected[i], projected[(i + 1) % projected.len()]);
            total += a.0 * b.1 - b.0 * a.1;
        }
        total
    };
    // Exterior clockwise on screen (positive), hole the other way.
    let exterior = ring(&[
        (174.0, -41.6),
        (176.0, -41.6),
        (176.0, -40.4),
        (174.0, -40.4),
    ]);
    let hole = ring(&[
        (174.6, -41.3),
        (175.4, -41.3),
        (175.4, -40.7),
        (174.6, -40.7),
    ]);
    exterior.abs() - hole.abs()
}

#[test]
fn a_polygon_is_read_at_all() {
    let built = build(8, lyv_simplify::Tolerance::NONE, 0);
    let store = store_of(&built);
    let (rings, _) = rings_at(&store, 8);
    assert!(!rings.is_empty(), "no closed rings reached the pyramid");
}

#[test]
fn the_river_is_not_closed_into_a_ring() {
    // A line crossing a tile looks like a clipped ring; closing it would turn
    // the river into a sliver polygon.
    let built = build(9, lyv_simplify::Tolerance::NONE, 0);
    let store = store_of(&built);

    let mut river_chains = 0;
    let mut river_rings = 0;
    for z in 0..=9 {
        store
            .for_each_at(z, &mut |_c: TileCoord, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    if chain.feature == 2 {
                        river_chains += 1;
                        if chain.points.first() == chain.points.last() {
                            river_rings += 1;
                        }
                    }
                }
                Ok(())
            })
            .unwrap();
    }
    assert!(river_chains > 0, "the river vanished");
    assert_eq!(river_rings, 0, "{river_rings} pieces of river became rings");
}

#[test]
fn no_piece_of_a_polygon_is_left_open() {
    // Every polygon chain at every zoom is a closed ring. This also pins
    // `cut_ring` over `cut_linestring`: when a ring's first vertex is mid-tile,
    // only `cut_ring` splices the two halves of that tile's run.
    let built = build(10, lyv_simplify::Tolerance::NONE, 0);
    let store = store_of(&built);

    let mut open_pieces = Vec::new();
    let mut closed = 0usize;
    for z in 0..=10 {
        store
            .for_each_at(z, &mut |coord: TileCoord, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    if chain.feature != 1 {
                        continue; // the river is meant to stay open
                    }
                    if chain.points.first() == chain.points.last() {
                        closed += 1;
                    } else {
                        open_pieces.push((coord, chain.points.len()));
                    }
                }
                Ok(())
            })
            .unwrap();
    }

    assert!(closed > 50, "only {closed} rings, so this proves little");
    assert!(
        open_pieces.is_empty(),
        "{} pieces of the lake were left open, e.g. {:?}",
        open_pieces.len(),
        &open_pieces[..open_pieces.len().min(3)]
    );
}

#[test]
fn the_lake_keeps_its_area_at_every_zoom() {
    // However the lake is cut and reassembled, its area is the same.
    // Simplification off, to measure the polygon path alone.
    let built = build(11, lyv_simplify::Tolerance::NONE, 0);
    let store = store_of(&built);

    for z in 0..=11 {
        let (rings, _) = rings_at(&store, z);
        let got: i128 = rings.iter().map(|r| double_area(r)).sum();
        let want = expected_double_area(z);

        // Halving moves a ring's area by about its perimeter; the failure
        // guarded against loses or duplicates whole tiles.
        let slack = (want.abs() * 0.02).max(4.0 * E as f64);
        assert!(
            (got as f64 - want).abs() <= slack,
            "zoom {z}: area {got} against {want:.0} (slack {slack:.0})"
        );
    }
}

#[test]
fn the_island_stays_a_hole_while_it_fits_in_one_tile() {
    // At low zoom the lake fits one tile, so the island is still its own ring
    // and must be wound against the exterior; the same way round, the areas
    // would add.
    let built = build(10, lyv_simplify::Tolerance::NONE, 0);
    let store = store_of(&built);

    // Which zooms is derived, not hard-coded (a guess was wrong once).
    let mut checked = 0;
    for z in 0..=10 {
        let mut tiles = 0;
        store
            .for_each_at(z, &mut |_c: TileCoord, _data| {
                tiles += 1;
                Ok(())
            })
            .unwrap();
        if tiles != 1 {
            continue;
        }

        let (rings, _) = rings_at(&store, z);
        assert_eq!(rings.len(), 2, "zoom {z}: expected an exterior and a hole");
        assert_eq!(
            rings.iter().filter(|r| double_area(r) < 0).count(),
            1,
            "zoom {z}: the island lost its hole winding"
        );
        checked += 1;
    }
    assert!(
        checked >= 2,
        "only {checked} zooms held the lake in a single tile, so the hole \
         winding is barely tested"
    );
}

#[test]
fn once_the_island_spans_tiles_it_becomes_part_of_the_boundary() {
    // At high zoom no ring is wound as a hole: a hole crossing a tile edge
    // becomes a notch in the exterior. The subtraction survives as reduced
    // area.
    let built = build(10, lyv_simplify::Tolerance::NONE, 0);
    let store = store_of(&built);

    let (rings, _) = rings_at(&store, 10);
    assert!(rings.len() > 2, "the island should span several tiles here");
    assert_eq!(
        rings.iter().filter(|r| double_area(r) < 0).count(),
        0,
        "a clipped hole should be folded into its tile's exterior ring"
    );

    // Still the lake minus the island.
    let total: f64 = rings.iter().map(|r| double_area(r) as f64).sum();
    let exterior_alone = {
        let project = |lon: f64, lat: f64| {
            let (x, y) = lyv_core::mercator::project_global(lon, lat, 10, EXTENT);
            (x as f64, y as f64)
        };
        let pts = [
            (174.0, -41.6),
            (176.0, -41.6),
            (176.0, -40.4),
            (174.0, -40.4),
        ];
        let projected: Vec<(f64, f64)> = pts.iter().map(|&(a, b)| project(a, b)).collect();
        let mut t = 0.0;
        for i in 0..projected.len() {
            let (a, b) = (projected[i], projected[(i + 1) % projected.len()]);
            t += a.0 * b.1 - b.0 * a.1;
        }
        t.abs()
    };
    assert!(
        total < exterior_alone * 0.95,
        "the island did not subtract: {total:.0} against {exterior_alone:.0}"
    );
}

#[test]
fn tiles_swallowed_by_the_lake_become_full_squares() {
    // At zoom 11 some tiles are wholly inside the lake, with no boundary; the
    // cut never visits them.
    let built = build(11, lyv_simplify::Tolerance::NONE, 0);
    let store = store_of(&built);

    let e = EXTENT as Local;
    let full: Vec<(Local, Local)> = vec![(0, 0), (e, 0), (e, e), (0, e), (0, 0)];

    let mut squares = 0;
    store
        .for_each_at(11, &mut |_c: TileCoord, data| {
            for chain in lyv_mvt::decode_chains(&data).unwrap() {
                let pts: Vec<(Local, Local)> = chain.points.iter().map(|p| (p.x, p.y)).collect();
                if pts == full {
                    squares += 1;
                }
            }
            Ok(())
        })
        .unwrap();

    assert!(
        squares > 10,
        "only {squares} tiles were filled by coverage; the scanline is not \
         reaching the pipeline"
    );
}

#[test]
fn every_ring_stays_closed_through_simplification() {
    // Simplification keeps each ring's start point, so rings stay closed; if
    // not, the merge would treat them as lines.
    let built = build(10, lyv_simplify::Tolerance::Distance(2.0), 0);
    let store = store_of(&built);

    for z in 0..=10 {
        let (rings, _) = rings_at(&store, z);
        assert!(!rings.is_empty(), "zoom {z} has no rings left");
        for ring in &rings {
            assert!(
                ring.len() >= 4,
                "zoom {z}: a ring shrank to {:?}",
                ring.len()
            );
            assert_eq!(ring.first(), ring.last(), "zoom {z}: a ring came unclosed");
        }
    }
}

#[test]
fn the_archive_passes_the_reference_verifier() {
    let built = build(9, lyv_simplify::Tolerance::Distance(1.0), 64);
    let Some(cli) = find_pmtiles() else {
        eprintln!("skipping: pmtiles CLI not installed");
        return;
    };
    let output = std::process::Command::new(&cli)
        .args(["verify", &built.options.output.to_string_lossy()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "pmtiles verify rejected the archive: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn find_pmtiles() -> Option<PathBuf> {
    let path = PathBuf::from("pmtiles");
    std::process::Command::new(&path)
        .arg("--help")
        .output()
        .ok()
        .map(|_| path)
}

// ---------------------------------------------------------------------------
// What the archive declares about itself
// ---------------------------------------------------------------------------

/// The PMTiles v3 bounds and centre, read from the bytes a consumer reads.
fn header_extent(path: &std::path::Path) -> ([f64; 4], (f64, f64, u8)) {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..7], b"PMTiles", "not a PMTiles archive");
    assert_eq!(bytes[7], 3, "expected spec version 3");

    let at = |offset: usize| -> i32 {
        // Four bytes, little-endian.
        i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    };
    // Offsets in the 127-byte v3 header, checked against a real archive:
    //
    //   100  min_zoom            114  max_lat_e7
    //   101  max_zoom            118  center_zoom
    //   102  min_lon_e7          119  center_lon_e7
    //   106  min_lat_e7          123  center_lat_e7
    //   110  max_lon_e7
    let e7 = |value: i32| f64::from(value) / 10_000_000.0;
    let bounds = [at(102), at(106), at(110), at(114)].map(e7);
    let centre = (e7(at(119)), e7(at(123)), bytes[118]);
    (bounds, centre)
}

#[test]
fn the_archive_declares_where_its_data_is() {
    // Unset, both default to the whole world.
    let built = build(9, lyv_simplify::Tolerance::Distance(1.0), 64);
    let (bounds, centre) = header_extent(&built.options.output);

    // The union of lake (174..176, -41.6..-40.4) and river (173.5..176.5,
    // -41.9..-40.1).
    assert!(
        (bounds[0] - 173.5).abs() < 0.01 && (bounds[2] - 176.5).abs() < 0.01,
        "longitude bounds came back as {}..{}",
        bounds[0],
        bounds[2]
    );
    assert!(
        (bounds[1] + 41.9).abs() < 0.01 && (bounds[3] + 40.1).abs() < 0.01,
        "latitude bounds came back as {}..{}",
        bounds[1],
        bounds[3]
    );

    // And not the default.
    assert!(bounds[0] > -179.0, "bounds are still the whole world");

    let (lon, lat, zoom) = centre;
    assert!(
        lon > bounds[0] && lon < bounds[2] && lat > bounds[1] && lat < bounds[3],
        "the centre {lon},{lat} is outside the bounds"
    );
    assert!(zoom > 0 && u32::from(zoom) <= 9, "centre zoom is {zoom}");
}

#[test]
fn a_hole_inside_a_single_tile_still_gets_its_exterior() {
    // A hole inside one tile deep in the polygon leaves that tile holding only
    // an interior ring; it must still get its square, or renderers fill the
    // hole and leave the water empty.
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("pond.geojson");
    std::fs::write(
        &input,
        r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":1,"properties":{},"geometry":{"type":"Polygon",
             "coordinates":[
               [[174.0,-41.6],[176.0,-41.6],[176.0,-40.4],[174.0,-40.4],[174.0,-41.6]],
               [[175.00,-41.02],[175.03,-41.02],[175.03,-41.00],[175.00,-41.00],
                [175.00,-41.02]]
             ]}}
        ]}"#,
    )
    .unwrap();
    let output = dir.path().join("pond.pmtiles");
    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(12),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "water".to_string(),
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
        // No limits: these tests check exact geometry.
        limits: lyv::Limits {
            bytes: 0,
            features: 0,
        },
        // No vertex budget, which would coarsen the geometry.
        max_tile_vertices: 0,
        buffer: 0,
        simplification: lyv_simplify::Tolerance::NONE,
    };
    run(&options).expect("the pipeline should run");
    let store = SqliteStore::open(&options.store).unwrap();

    // No tile may hold interior rings with no exterior.
    for z in 0..=12 {
        store
            .for_each_at(z, &mut |coord: TileCoord, data| {
                let chains = lyv_mvt::decode_chains(&data).unwrap();
                let rings: Vec<i128> = chains
                    .iter()
                    .filter(|c| c.points.first() == c.points.last())
                    .map(|c| {
                        let pts: Vec<(i64, i64)> = c
                            .points
                            .iter()
                            .map(|p| (i64::from(p.x), i64::from(p.y)))
                            .collect();
                        double_area(&pts)
                    })
                    .collect();
                if rings.iter().any(|a| *a < 0) {
                    assert!(
                        rings.iter().any(|a| *a > 0),
                        "tile {coord:?} holds {} hole(s) and no exterior",
                        rings.iter().filter(|a| **a < 0).count()
                    );
                }
                Ok(())
            })
            .unwrap();
    }

    // The pond must still be a hole somewhere, or this proves nothing.
    let mut holes = 0;
    store
        .for_each_at(12, &mut |_c: TileCoord, data| {
            for chain in lyv_mvt::decode_chains(&data).unwrap() {
                let pts: Vec<(i64, i64)> = chain
                    .points
                    .iter()
                    .map(|p| (i64::from(p.x), i64::from(p.y)))
                    .collect();
                if chain.points.first() == chain.points.last() && double_area(&pts) < 0 {
                    holes += 1;
                }
            }
            Ok(())
        })
        .unwrap();
    assert!(holes > 0, "the pond stopped being a hole entirely");
}

#[test]
fn a_polygon_goes_out_with_polygon_geometry_and_a_line_does_not() {
    // Polygons must go out as polygons, checked with a buffer on: unclosed
    // margin pieces once made features fall back to linestrings.
    let built = build(9, lyv_simplify::Tolerance::Distance(1.0), 64);
    let emitted = lyv::Collected::from(&built.options).unwrap();

    let mut lake_tiles = 0;
    let mut river_tiles = 0;
    for z in 0..=9 {
        for (coord, data) in emitted.at(z) {
            {
                for (id, kind) in lyv_mvt::decode_geometry_kinds(data).unwrap() {
                    match id {
                        1 => {
                            assert!(kind.is_areal(), "the lake is {kind:?} in tile {coord:?}");
                            lake_tiles += 1;
                        }
                        2 => {
                            assert!(!kind.is_areal(), "the river is {kind:?} in tile {coord:?}");
                            river_tiles += 1;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    assert!(lake_tiles > 20, "only {lake_tiles} lake tiles checked");
    assert!(river_tiles > 10, "only {river_tiles} river tiles checked");
}

#[test]
fn the_island_reaches_the_output_as_an_interior_ring() {
    // A hole encoded as a separate exterior would have the right total area
    // but render solid.
    let built = build(9, lyv_simplify::Tolerance::Distance(1.0), 64);
    let emitted = lyv::Collected::from(&built.options).unwrap();

    let mut with_interiors = 0;
    for z in 0..=9 {
        for (_c, data) in emitted.at(z) {
            {
                for (id, kind) in lyv_mvt::decode_geometry_kinds(data).unwrap() {
                    if id == 1 && kind.interior_rings() > 0 {
                        with_interiors += 1;
                    }
                }
            }
        }
    }
    assert!(
        with_interiors > 0,
        "no tile carried the island as an interior ring"
    );
}

#[test]
fn a_buffered_polygon_has_no_geometry_along_a_tile_edge() {
    // With welding, no ring runs along the tile edge (which would draw the
    // grid when stroked); the margin runs lie outside the tile.
    let buffered = build(9, lyv_simplify::Tolerance::Distance(1.0), 64);
    let plain = build(9, lyv_simplify::Tolerance::Distance(1.0), 0);

    let on_tile_edge = |built: &Built| {
        let emitted = lyv::Collected::from(&built.options).unwrap();
        let e = EXTENT as Local;
        let mut count = 0;
        for z in 0..=9 {
            for (_c, data) in emitted.at(z) {
                {
                    for chain in lyv_mvt::decode_chains(data).unwrap() {
                        if chain.feature != 1 {
                            continue; // the river is a line; it has no edges to cancel
                        }
                        for w in chain.points.windows(2) {
                            let vertical = w[0].x == w[1].x && (w[0].x == 0 || w[0].x == e);
                            let horizontal = w[0].y == w[1].y && (w[0].y == 0 || w[0].y == e);
                            if vertical || horizontal {
                                count += 1;
                            }
                        }
                    }
                }
            }
        }
        count
    };

    // Unbuffered, every clipped ring runs along the tile edge.
    assert!(
        on_tile_edge(&plain) > 0,
        "the unbuffered fixture should have tile-edge runs, or this proves nothing"
    );
    // Buffered, none should. (A real boundary along a tile edge could; this
    // fixture has none.)
    assert_eq!(
        on_tile_edge(&buffered),
        0,
        "the tile grid survived into the buffered output"
    );
}

#[test]
fn a_buffered_tile_never_covers_more_than_its_buffered_square() {
    // The margin must union with our geometry, not overlap it, or tiles claim
    // more area than they have.
    let built = build(8, lyv_simplify::Tolerance::NONE, 64);
    let emitted = lyv::Collected::from(&built.options).unwrap();
    let margin = i128::from(lyv_buffer::margin_units(5, EXTENT));
    let side = i128::from(EXTENT) + 2 * margin;

    let mut checked = 0;
    for (coord, data) in emitted.at(8) {
        {
            let total: i128 = lyv_mvt::decode_chains(data)
                .unwrap()
                .iter()
                .filter(|c| c.feature == 1)
                .map(|c| {
                    let pts: Vec<(i64, i64)> = c
                        .points
                        .iter()
                        .map(|p| (i64::from(p.x), i64::from(p.y)))
                        .collect();
                    double_area(&pts)
                })
                .sum();
            if total != 0 {
                checked += 1;
            }
            assert!(
                total <= 2 * side * side,
                "tile {coord:?} covers {total}, more than the whole buffered \
                 square — geometry was laid over itself rather than joined"
            );
        }
    }
    assert!(checked > 5, "only {checked} tiles held the lake");
}

#[test]
fn the_encode_pass_never_empties_a_tile_that_had_geometry() {
    // A lower bound: the other buffer tests would pass an encode that deleted
    // everything. A square, whose fully covered tiles have every edge on a seam,
    // is where mis-paired pieces cancelled a tile away.
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("block.geojson");
    std::fs::write(
        &input,
        r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":1,"properties":{},"geometry":{"type":"Polygon",
             "coordinates":[[[174.0,-41.6],[176.0,-41.6],[176.0,-40.4],[174.0,-40.4],
                             [174.0,-41.6]]]}}
        ]}"#,
    )
    .unwrap();
    let output = dir.path().join("block.pmtiles");
    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(10),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "water".to_string(),
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
    let store = SqliteStore::open(&options.store).unwrap();
    // The finished tiles, which go straight to the archive.
    let emitted = lyv::Collected::from(&options).unwrap();

    let mut covered = 0;
    for z in 0..=10 {
        let internal: Vec<(u32, u32)> = {
            let mut out = Vec::new();
            store
                .for_each_at(z, &mut |coord: TileCoord, data| {
                    let chains = lyv_mvt::decode_chains(&data).unwrap();
                    let area: i128 = chains
                        .iter()
                        .map(|c| {
                            let pts: Vec<(i64, i64)> = c
                                .points
                                .iter()
                                .map(|p| (i64::from(p.x), i64::from(p.y)))
                                .collect();
                            double_area(&pts)
                        })
                        .sum();
                    if area > 0 {
                        out.push((coord.x, coord.y));
                    }
                    Ok(())
                })
                .unwrap();
            out
        };

        for (x, y) in internal {
            let coord = TileCoord { z, x, y };
            let data = emitted
                .get(coord)
                .unwrap_or_else(|| panic!("{coord:?} has no encoded tile at all"));
            let chains = lyv_mvt::decode_chains(data).unwrap();
            let area: i128 = chains
                .iter()
                .map(|c| {
                    let pts: Vec<(i64, i64)> = c
                        .points
                        .iter()
                        .map(|p| (i64::from(p.x), i64::from(p.y)))
                        .collect();
                    double_area(&pts)
                })
                .sum();
            assert!(
                area > 0,
                "{coord:?} had fill internally and none after encoding"
            );
            covered += 1;
        }
    }
    assert!(covered > 40, "only {covered} tiles checked");
}

#[test]
fn a_ring_that_collapses_never_becomes_a_hole() {
    // Small rings can fold inside out when rounded and read as holes, which
    // renderers fill. A field of slivers, deep enough that several merges
    // shrink them away.
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("slivers.geojson");

    let mut features = Vec::new();
    for i in 0..200 {
        let lon = 174.0 + f64::from(i % 20) * 0.01;
        let lat = -41.0 + f64::from(i / 20) * 0.01;
        // Long and thin: what rounds itself inside out.
        features.push(format!(
            r#"{{"type":"Feature","id":{i},"properties":{{}},"geometry":{{
                 "type":"Polygon","coordinates":[[[{lon},{lat}],
                 [{a},{b}],[{c},{d}],[{lon},{lat}]]]}}}}"#,
            a = lon + 0.0012,
            b = lat + 0.0007,
            c = lon + 0.0011,
            d = lat + 0.0008,
        ));
    }
    std::fs::write(
        &input,
        format!(
            r#"{{"type":"FeatureCollection","features":[{}]}}"#,
            features.join(",")
        ),
    )
    .unwrap();

    let output = dir.path().join("slivers.pmtiles");
    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom: 0,
        max_zoom: Some(14),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "slivers".to_string(),
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
    let store = store_of_path(&options.store);

    let mut orphans = Vec::new();
    let mut zero_area = 0;
    for z in 0..=14 {
        store
            .for_each_at(z, &mut |coord: TileCoord, data| {
                let mut per: std::collections::HashMap<u64, (u32, u32)> = Default::default();
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    if chain.points.first() != chain.points.last() {
                        continue;
                    }
                    let pts: Vec<(i64, i64)> = chain
                        .points
                        .iter()
                        .map(|p| (i64::from(p.x), i64::from(p.y)))
                        .collect();
                    let area = double_area(&pts);
                    let entry = per.entry(chain.feature).or_default();
                    match area.cmp(&0) {
                        std::cmp::Ordering::Greater => entry.0 += 1,
                        std::cmp::Ordering::Less => entry.1 += 1,
                        std::cmp::Ordering::Equal => zero_area += 1,
                    }
                }
                for (feature, (exteriors, holes)) in per {
                    if holes > 0 && exteriors == 0 {
                        orphans.push((coord, feature));
                    }
                }
                Ok(())
            })
            .unwrap();
    }

    assert_eq!(zero_area, 0, "rings enclosing nothing were kept");
    assert!(
        orphans.is_empty(),
        "{} orphan hole(s), e.g. {:?}",
        orphans.len(),
        &orphans[..orphans.len().min(3)]
    );
}

fn store_of_path(path: &std::path::Path) -> SqliteStore {
    SqliteStore::open(path).unwrap()
}
