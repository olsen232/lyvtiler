//! The whole polygon path on arbitrarily nested rings: cut, close and fill at
//! maxzoom, then merge to zoom 0, checking every tile at every level against a
//! direct clip of the original polygon.
//!
//! The oracle is Sutherland-Hodgman against each tile box, summed over rings
//! with their signs — independent of everything under test.

use geo_types::Coord;
use lyv_close::close_rings;
use lyv_core::{Chain, Local, Pieces, TileCoord};
use lyv_coverage::fill_covered;
use lyv_cut::cut_ring;
use lyv_merge::merge_level;

const EXTENT: u32 = 4096;
const E: i64 = EXTENT as i64;
const MAXZOOM: u32 = 3; // an 8x8 grid, so four merges to reach zoom 0
const WORLD: i64 = (1 << MAXZOOM) * E;
const FEATURE: u64 = 1;

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

/// Doubled shoelace area. Positive is clockwise on screen, consistent with MVT.
fn double_area(ring: &[Coord<i64>]) -> i128 {
    let mut total = 0i128;
    let closed = ring.first() == ring.last();
    let n = if closed { ring.len() - 1 } else { ring.len() };
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        total += i128::from(a.x) * i128::from(b.y) - i128::from(b.x) * i128::from(a.y);
    }
    total
}

fn square(low: i64, high: i64, clockwise: bool) -> Vec<Coord<i64>> {
    let mut ring = vec![
        Coord { x: low, y: low },
        Coord { x: high, y: low },
        Coord { x: high, y: high },
        Coord { x: low, y: high },
        Coord { x: low, y: low },
    ];
    if !clockwise {
        ring.reverse();
    }
    ring
}

/// Clip a ring to an axis-aligned box. Correct for any convex clip region.
fn clip_to_box(ring: &[Coord<i64>], low: Coord<i64>, high: Coord<i64>) -> Vec<Coord<f64>> {
    let mut output: Vec<Coord<f64>> = ring
        .iter()
        .map(|p| Coord {
            x: p.x as f64,
            y: p.y as f64,
        })
        .collect();
    if output.first() == output.last() && output.len() > 1 {
        output.pop();
    }

    for (axis, sign, bound) in [
        (0usize, 1.0f64, low.x as f64),
        (0, -1.0, high.x as f64),
        (1, 1.0, low.y as f64),
        (1, -1.0, high.y as f64),
    ] {
        let at = |p: &Coord<f64>| if axis == 0 { p.x } else { p.y };
        let side = |p: &Coord<f64>| sign * (at(p) - bound);

        let input = std::mem::take(&mut output);
        if input.is_empty() {
            return Vec::new();
        }
        for i in 0..input.len() {
            let (a, b) = (input[i], input[(i + 1) % input.len()]);
            let (sa, sb) = (side(&a), side(&b));
            if sa >= 0.0 {
                output.push(a);
            }
            if (sa >= 0.0) != (sb >= 0.0) {
                let t = sa / (sa - sb);
                output.push(Coord {
                    x: a.x + (b.x - a.x) * t,
                    y: a.y + (b.y - a.y) * t,
                });
            }
        }
    }
    output
}

fn double_area_f(ring: &[Coord<f64>]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut total = 0.0;
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
        total += a.x * b.y - b.x * a.y;
    }
    total
}

/// The doubled area the polygon covers inside one tile, in maxzoom units.
fn oracle(rings: &[Vec<Coord<i64>>], low: Coord<i64>, high: Coord<i64>) -> f64 {
    rings
        .iter()
        .map(|r| double_area_f(&clip_to_box(r, low, high)))
        .sum()
}

// ---------------------------------------------------------------------------
// The pipeline under test
// ---------------------------------------------------------------------------

/// Cut, close and fill: maxzoom as ingest builds it.
fn build_maxzoom(rings: &[Vec<Coord<i64>>]) -> Pieces {
    let mut pieces = Pieces::new();
    for ring in rings {
        cut_ring(FEATURE, ring, MAXZOOM, EXTENT, &mut pieces);
    }
    let addresses: Vec<TileCoord> = pieces.keys().copied().collect();
    for coord in addresses {
        let closed = close_rings(&pieces[&coord], EXTENT);
        pieces.insert(coord, closed);
    }
    fill_covered(FEATURE, rings, MAXZOOM, EXTENT, &mut pieces);
    pieces
}

/// How many maxzoom global units one tile unit is worth, at a given zoom.
fn scale(zoom: u32) -> i64 {
    1 << (MAXZOOM - zoom)
}

/// Check every tile of one level against the oracle. Returns the level's
/// doubled area in maxzoom units and the worst per-tile discrepancy.
fn check_level(rings: &[Vec<Coord<i64>>], pieces: &Pieces, zoom: u32) -> (f64, f64) {
    let step = scale(zoom);
    let side = E * step; // ground covered by one tile, in maxzoom units
    let mut total = 0.0;
    let mut worst = 0.0f64;

    for (coord, chains) in pieces {
        assert_eq!(coord.z, zoom, "a tile at the wrong zoom");

        let origin = Coord {
            x: i64::from(coord.x) * side,
            y: i64::from(coord.y) * side,
        };

        let mut tile_area = 0.0;
        for chain in chains {
            assert!(
                chain.points.len() >= 4 && chain.points.first() == chain.points.last(),
                "tile {coord:?} holds an unclosed ring: {:?}",
                chain.points
            );
            for p in &chain.points {
                assert!(
                    (0..=EXTENT as Local).contains(&p.x) && (0..=EXTENT as Local).contains(&p.y),
                    "{p:?} escaped tile {coord:?}"
                );
            }
            let global: Vec<Coord<i64>> = chain
                .points
                .iter()
                .map(|p| Coord {
                    x: origin.x + i64::from(p.x) * step,
                    y: origin.y + i64::from(p.y) * step,
                })
                .collect();
            tile_area += double_area(&global) as f64;
        }

        let want = oracle(
            rings,
            origin,
            Coord {
                x: origin.x + side,
                y: origin.y + side,
            },
        );
        // Rounding moves each coordinate up to half a unit per level, so a
        // ring's area can move by about its perimeter times that.
        let slack = 8.0 * side as f64 * step as f64 * (1 + chains.len()) as f64;
        let off = (tile_area - want).abs();
        worst = worst.max(off / slack.max(1.0));
        assert!(
            off <= slack,
            "zoom {zoom} tile {coord:?}: covers {tile_area:.0} but the oracle \
             says {want:.0} (off by {off:.0}, slack {slack:.0})"
        );
        total += tile_area;
    }
    (total, worst)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Nested squares of alternating winding, each inside the last. Each ring must
/// still span more than two tiles, so it swallows one whole and exercises
/// coverage.
fn nested(depth: usize, inset: i64) -> Vec<Vec<Coord<i64>>> {
    let mut out = Vec::new();
    let (mut low, mut high) = (E / 2, WORLD - E / 2);
    for level in 0..depth {
        assert!(
            high - low > 2 * E,
            "fixture: ring {level} spans {} and could not swallow a tile",
            high - low
        );
        out.push(square(low, high, level % 2 == 0));
        low += inset;
        high -= inset;
    }
    out
}

/// The tile square, as coverage emits it.
fn full_tile() -> Vec<Coord<Local>> {
    let e = EXTENT as Local;
    vec![
        Coord { x: 0, y: 0 },
        Coord { x: e, y: 0 },
        Coord { x: e, y: e },
        Coord { x: 0, y: e },
        Coord { x: 0, y: 0 },
    ]
}

/// How many tiles are wholly inside the given number of rings: the nesting the
/// fixture actually reaches.
fn deepest_nesting(rings: &[Vec<Coord<i64>>]) -> usize {
    let span = 1i64 << MAXZOOM;
    let mut deepest = 0;
    for ty in 0..span {
        for tx in 0..span {
            let (low, high) = (
                Coord {
                    x: tx * E,
                    y: ty * E,
                },
                Coord {
                    x: (tx + 1) * E,
                    y: (ty + 1) * E,
                },
            );
            // No boundary passes through the tile.
            let enclosing = rings
                .iter()
                .filter(|r| {
                    let clipped = clip_to_box(r, low, high);
                    clipped.len() == 4 && double_area_f(&clipped).abs() == (E * E * 2) as f64
                })
                .count();
            deepest = deepest.max(enclosing);
        }
    }
    deepest
}

#[test]
fn a_plain_polygon_survives_every_merge() {
    let rings = nested(1, 0);
    let mut pieces = build_maxzoom(&rings);
    let expected = double_area(&rings[0]) as f64;

    for zoom in (0..=MAXZOOM).rev() {
        let (total, _) = check_level(&rings, &pieces, zoom);
        assert!(
            (total - expected).abs() <= 0.02 * expected.abs(),
            "zoom {zoom}: total {total:.0} against {expected:.0}"
        );
        if zoom > 0 {
            pieces = merge_level(&pieces, EXTENT);
        }
    }
}

#[test]
fn a_polygon_with_a_hole_survives_every_merge() {
    let rings = nested(2, 2 * E);
    let expected = (double_area(&rings[0]) + double_area(&rings[1])) as f64;
    assert!(expected > 0.0, "the hole should not swallow the exterior");

    let mut pieces = build_maxzoom(&rings);
    for zoom in (0..=MAXZOOM).rev() {
        let (total, _) = check_level(&rings, &pieces, zoom);
        assert!(
            (total - expected).abs() <= 0.02 * expected.abs(),
            "zoom {zoom}: total {total:.0} against {expected:.0}"
        );
        if zoom > 0 {
            pieces = merge_level(&pieces, EXTENT);
        }
    }
}

#[test]
fn arbitrarily_nested_rings_and_holes_survive_every_merge() {
    // Depth 2 to 5, with tiles swallowed whole by several rings at once.
    for depth in 2..=5 {
        let rings = nested(depth, E / 2);
        let expected: f64 = rings.iter().map(|r| double_area(r) as f64).sum();

        let mut pieces = build_maxzoom(&rings);

        // Some tile must be inside every ring at once, or the fixture misses.
        assert_eq!(
            deepest_nesting(&rings),
            depth,
            "depth {depth}: no tile is enclosed by all {depth} rings"
        );
        let squares = pieces
            .values()
            .flatten()
            .filter(|c| c.points == full_tile())
            .count();
        let expect_fill = depth % 2 == 1;
        assert_eq!(
            squares > 0,
            expect_fill,
            "depth {depth}: {squares} filled tiles, but odd nesting should \
             fill and even nesting should not"
        );
        let mut worst_overall = 0.0f64;

        for zoom in (0..=MAXZOOM).rev() {
            let (total, worst) = check_level(&rings, &pieces, zoom);
            worst_overall = worst_overall.max(worst);
            assert!(
                (total - expected).abs() <= 0.02 * expected.abs().max(1.0),
                "depth {depth} zoom {zoom}: total {total:.0} against \
                 {expected:.0}"
            );
            if zoom > 0 {
                pieces = merge_level(&pieces, EXTENT);
            }
        }
        println!(
            "depth {depth}: net area {expected:.0}, worst per-tile \
             discrepancy {:.1}% of slack",
            worst_overall * 100.0
        );
    }
}

#[test]
fn a_tile_swallowed_by_a_hole_stays_empty_all_the_way_up() {
    // Tiles swallowed by the hole must stay empty all the way up.
    let rings = vec![
        square(E / 2, WORLD - E / 2, true),
        square(2 * E, 6 * E, false),
    ];

    let pieces = build_maxzoom(&rings);

    for (x, y) in [(3, 3), (3, 4), (4, 3), (4, 4)] {
        let coord = TileCoord { z: MAXZOOM, x, y };
        assert!(
            pieces.get(&coord).is_none_or(Vec::is_empty),
            "tile {coord:?} is inside the hole but holds {:?}",
            pieces.get(&coord)
        );
    }
    let full: Vec<Coord<Local>> = vec![
        Coord { x: 0, y: 0 },
        Coord {
            x: EXTENT as Local,
            y: 0,
        },
        Coord {
            x: EXTENT as Local,
            y: EXTENT as Local,
        },
        Coord {
            x: 0,
            y: EXTENT as Local,
        },
        Coord { x: 0, y: 0 },
    ];
    let coord = TileCoord {
        z: MAXZOOM,
        x: 1,
        y: 4,
    };
    assert_eq!(
        pieces.get(&coord).map(|c| c[0].points.clone()),
        Some(full),
        "a tile inside the exterior only should be a full square"
    );
}

#[test]
fn four_full_tiles_merge_into_one_full_tile() {
    // Two of each child's edges cancel; the eight survivors collapse into one
    // square.
    let mut pieces = Pieces::new();
    let full = vec![
        Coord { x: 0, y: 0 },
        Coord {
            x: EXTENT as Local,
            y: 0,
        },
        Coord {
            x: EXTENT as Local,
            y: EXTENT as Local,
        },
        Coord {
            x: 0,
            y: EXTENT as Local,
        },
        Coord { x: 0, y: 0 },
    ];
    for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        pieces.insert(
            TileCoord { z: 1, x, y },
            vec![Chain {
                feature: FEATURE,
                points: full.clone(),
            }],
        );
    }

    let parent = merge_level(&pieces, EXTENT);
    assert_eq!(parent.len(), 1);
    let chains = &parent[&TileCoord { z: 0, x: 0, y: 0 }];
    assert_eq!(chains.len(), 1, "one ring, not four");
    assert_eq!(
        chains[0].points, full,
        "the parent should be its own full square, with no leftover midpoints"
    );
}

#[test]
fn a_full_square_does_not_accumulate_vertices_up_the_pyramid() {
    // Four full tiles give a parent whose sides arrive in halves. Kept, those
    // midpoints would double at every level, on the boundary where
    // simplification can't remove them.
    let full = vec![
        Coord { x: 0, y: 0 },
        Coord {
            x: EXTENT as Local,
            y: 0,
        },
        Coord {
            x: EXTENT as Local,
            y: EXTENT as Local,
        },
        Coord {
            x: 0,
            y: EXTENT as Local,
        },
        Coord { x: 0, y: 0 },
    ];

    let mut pieces = Pieces::new();
    let span = 1u32 << 4;
    for y in 0..span {
        for x in 0..span {
            pieces.insert(
                TileCoord { z: 4, x, y },
                vec![Chain {
                    feature: FEATURE,
                    points: full.clone(),
                }],
            );
        }
    }

    for zoom in (0..4).rev() {
        pieces = merge_level(&pieces, EXTENT);
        for (coord, chains) in &pieces {
            for chain in chains {
                assert_eq!(
                    chain.points.len(),
                    5,
                    "tile {coord:?} at zoom {zoom} grew to {} points",
                    chain.points.len()
                );
            }
        }
    }
    assert_eq!(pieces.len(), 1, "should end at the single root tile");
}

// ---------------------------------------------------------------------------
// Cases the nested fixtures do not reach
// ---------------------------------------------------------------------------

fn full_chain(feature: u64) -> Chain {
    Chain {
        feature,
        points: full_tile(),
    }
}

#[test]
fn a_sliver_that_collapses_when_halved_is_dropped() {
    // A ring with no width after halving must be dropped, not kept as a
    // zero-area ring. x = 101 and 102 both round to 51.
    let mut pieces = Pieces::new();
    pieces.insert(
        TileCoord { z: 1, x: 0, y: 0 },
        vec![Chain {
            feature: FEATURE,
            points: vec![
                Coord { x: 101, y: 100 },
                Coord { x: 102, y: 100 },
                Coord { x: 102, y: 200 },
                Coord { x: 101, y: 200 },
                Coord { x: 101, y: 100 },
            ],
        }],
    );

    let parent = merge_level(&pieces, EXTENT);
    let rings: usize = parent.values().map(Vec::len).sum();
    assert_eq!(
        rings, 0,
        "the collapsed sliver should be dropped: {parent:?}"
    );
}

#[test]
fn two_diagonal_full_tiles_pinch_at_the_parent_centre() {
    // Diagonal children share only the centre point, giving reassembly a real
    // choice. Either way the area must be right and every ring closed.
    let mut pieces = Pieces::new();
    for (x, y) in [(0, 0), (1, 1)] {
        pieces.insert(TileCoord { z: 1, x, y }, vec![full_chain(FEATURE)]);
    }

    let parent = merge_level(&pieces, EXTENT);
    let chains = &parent[&TileCoord { z: 0, x: 0, y: 0 }];

    let half_side = f64::from(EXTENT / 2);
    let want = 2.0 * half_side * half_side * 2.0; // two quadrants, doubled
    let got: f64 = chains
        .iter()
        .map(|c| {
            let global: Vec<Coord<i64>> = c
                .points
                .iter()
                .map(|p| Coord {
                    x: i64::from(p.x),
                    y: i64::from(p.y),
                })
                .collect();
            double_area(&global) as f64
        })
        .sum();
    assert_eq!(got, want, "the two quadrants should survive intact");

    for chain in chains {
        assert!(
            chain.points.len() >= 4 && chain.points.first() == chain.points.last(),
            "a pinched reassembly left an unclosed ring: {:?}",
            chain.points
        );
    }
}

#[test]
fn rings_of_different_features_never_cancel_against_each_other() {
    // Two features filling adjacent children: their shared edges must not
    // cancel across features.
    let mut pieces = Pieces::new();
    pieces.insert(TileCoord { z: 1, x: 0, y: 0 }, vec![full_chain(1)]);
    pieces.insert(TileCoord { z: 1, x: 1, y: 0 }, vec![full_chain(2)]);

    let parent = merge_level(&pieces, EXTENT);
    let chains = &parent[&TileCoord { z: 0, x: 0, y: 0 }];

    let mut features: Vec<u64> = chains.iter().map(|c| c.feature).collect();
    features.sort_unstable();
    assert_eq!(
        features,
        vec![1, 2],
        "each feature should keep its own quadrant, got {chains:?}"
    );

    for chain in chains {
        let global: Vec<Coord<i64>> = chain
            .points
            .iter()
            .map(|p| Coord {
                x: i64::from(p.x),
                y: i64::from(p.y),
            })
            .collect();
        let quarter = f64::from(EXTENT / 2) * f64::from(EXTENT) * 2.0 / 2.0;
        assert_eq!(
            double_area(&global) as f64,
            quarter,
            "feature {} has the wrong area",
            chain.feature
        );
    }
}
