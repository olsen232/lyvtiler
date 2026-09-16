//! Full-tile squares for tiles a polygon swallows whole.
//!
//! A tile wholly inside a polygon holds no boundary, so the cut gives it no
//! chains and closing never sees it. It is represented as the tile square, an
//! ordinary ring, and merges by cancellation like anything else — so only
//! maxzoom needs this.
//!
//! **Scanline, not point-in-polygon**, which would be O(V × T). Along a row of
//! tiles, coverage can only change across a tile that contains boundary, so one
//! winding count at the row's centre line decides each run of empty tiles.
//! Because only boundary-free tiles are decided, the winding is constant across
//! each one and the exact line or half-open convention doesn't matter.
//!
//! **Net winding per feature, at most one square.** Asking each ring separately
//! would stack coincident squares of opposite winding wherever rings nest, and
//! the merge can't cancel those because they aren't on a seam
//! (`docs/coverage.py`). Summing crossings across all a feature's rings costs
//! nothing extra. Relies on rings being consistently oriented, like `lyv-close`.

use geo_types::Coord;

pub use lyv_core::{Chain, Local, Pieces, TileCoord};

/// Add a full-tile ring for every tile this feature covers entirely.
///
/// `rings` are all of one feature's rings, in global coordinates at zoom `z`.
/// Tiles already holding this feature's boundary are left alone.
pub fn fill_covered(
    feature: u64,
    rings: &[Vec<Coord<i64>>],
    z: u32,
    extent: u32,
    pieces: &mut Pieces,
) {
    let mut covered = Vec::new();
    for_each_covered(feature, rings, z, extent, pieces, &mut |coord| {
        covered.push(coord)
    });
    let square = full_tile(extent as Local);
    for coord in covered {
        pieces.entry(coord).or_default().push(Chain {
            feature,
            points: square.clone(),
        });
    }
}

/// Which tiles this feature swallows whole, without filling them.
///
/// Separate from filling because ingest decides during the cut but must append
/// the squares after closing, to keep each tile's chain order (and so its
/// bytes) unchanged. Reads only this feature's chains in `pieces`.
pub fn for_each_covered(
    feature: u64,
    rings: &[Vec<Coord<i64>>],
    z: u32,
    extent: u32,
    pieces: &Pieces,
    visit: &mut dyn FnMut(TileCoord),
) {
    let extent = i64::from(extent);
    let span = 1i64 << z;

    let Some((low, high)) = bounds(rings) else {
        return;
    };
    let first_row = (low.y.div_euclid(extent)).clamp(0, span - 1);
    let last_row = (high.y.div_euclid(extent)).clamp(0, span - 1);
    // Columns aren't clamped: longitude wraps, as in the cut, so a polygon
    // past the antimeridian fills the columns it wraps to. At most once round.
    let first_column = low.x.div_euclid(extent);
    let last_column = high.x.div_euclid(extent).min(first_column + span - 1);

    for row in first_row..=last_row {
        let line = row * extent + extent / 2;

        // Every crossing of this row's centre line: exact x, and direction.
        let mut crossings: Vec<Crossing> = Vec::new();
        for ring in rings {
            for pair in ring.windows(2) {
                if let Some(crossing) = cross(pair[0], pair[1], line) {
                    crossings.push(crossing);
                }
            }
        }
        if crossings.is_empty() {
            // Nothing crosses this line, so the whole row is outside.
            continue;
        }

        // Sweep westward, accumulating crossings to the right of each centre.
        crossings.sort_unstable_by(|a, b| b.cmp_x(a));
        let mut taken = 0usize;
        let mut winding = 0i32;

        for column in (first_column..=last_column).rev() {
            let centre = column * extent + extent / 2;
            while taken < crossings.len() && crossings[taken].is_right_of(centre) {
                winding += crossings[taken].direction;
                taken += 1;
            }
            let coord = TileCoord {
                z,
                x: column.rem_euclid(span) as u32,
                y: row as u32,
            };
            if already_bounded(pieces, coord, feature) {
                continue;
            }

            // A tile holding only holes needs its square even if its centre is
            // inside a hole and reads as outside.
            let holds_holes = pieces.get(&coord).is_some_and(|chains| {
                chains
                    .iter()
                    .any(|c| c.feature == feature && is_hole(&c.points))
            });
            if winding == 0 && !holds_holes {
                continue;
            }

            visit(coord);
        }
    }
}

/// Does this tile already hold this feature's boundary, which settles it?
///
/// Anything but a hole does. A small hole sitting in an otherwise covered tile
/// leaves that tile holding only an interior ring; skipped, a renderer would
/// fill the hole. And a hole can only be there if the exterior covers the tile.
/// Open chains (before closing) count as boundary too.
fn already_bounded(pieces: &Pieces, coord: TileCoord, feature: u64) -> bool {
    pieces.get(&coord).is_some_and(|chains| {
        chains
            .iter()
            .filter(|c| c.feature == feature)
            .any(|c| !is_hole(&c.points))
    })
}

/// A closed ring wound the other way: an interior ring.
fn is_hole(points: &[Coord<Local>]) -> bool {
    points.len() >= 4 && points.first() == points.last() && double_area(points) < 0
}

/// Doubled shoelace area. Positive is clockwise on screen, consistent with MVT.
fn double_area(ring: &[Coord<Local>]) -> i64 {
    ring.windows(2)
        .map(|w| i64::from(w[0].x) * i64::from(w[1].y) - i64::from(w[1].x) * i64::from(w[0].y))
        .sum()
}

/// The tile square, clockwise on screen: an exterior. Used whatever the
/// winding's sign, since non-zero means filled.
pub fn full_tile(extent: Local) -> Vec<Coord<Local>> {
    vec![
        Coord { x: 0, y: 0 },
        Coord { x: extent, y: 0 },
        Coord {
            x: extent,
            y: extent,
        },
        Coord { x: 0, y: extent },
        Coord { x: 0, y: 0 },
    ]
}

/// The bounding box of every ring, or `None` if there is no geometry.
fn bounds(rings: &[Vec<Coord<i64>>]) -> Option<(Coord<i64>, Coord<i64>)> {
    let mut points = rings.iter().flatten();
    let first = *points.next()?;
    let mut low = first;
    let mut high = first;
    for point in points {
        low.x = low.x.min(point.x);
        low.y = low.y.min(point.y);
        high.x = high.x.max(point.x);
        high.y = high.y.max(point.y);
    }
    Some((low, high))
}

/// Where an edge crosses the scanline, and which way it was going.
///
/// The x is an exact rational. Rounding couldn't actually misplace it — a
/// crossing near a tile's centre is inside that tile, which is then skipped as
/// boundary — but exactness costs one multiplication and removes the argument.
struct Crossing {
    numerator: i128,
    denominator: i128,
    /// `+1` where the boundary runs up the screen, `-1` down, so a clockwise
    /// exterior has winding `+1` inside.
    direction: i32,
}

impl Crossing {
    fn is_right_of(&self, x: i64) -> bool {
        self.numerator > i128::from(x) * self.denominator
    }

    fn cmp_x(&self, other: &Self) -> std::cmp::Ordering {
        (self.numerator * other.denominator).cmp(&(other.numerator * self.denominator))
    }
}

/// Where this edge crosses the scanline, if it does.
///
/// Half-open in y, so a vertex exactly on the line is counted by one of its two
/// edges, never both or neither.
fn cross(a: Coord<i64>, b: Coord<i64>, line: i64) -> Option<Crossing> {
    if (a.y <= line) == (b.y <= line) {
        return None;
    }
    let (dx, dy) = (i128::from(b.x - a.x), i128::from(b.y - a.y));
    let numerator = i128::from(a.x) * dy + i128::from(line - a.y) * dx;
    // Keep the denominator positive so comparisons keep their sense.
    let (numerator, denominator) = if dy < 0 {
        (-numerator, -dy)
    } else {
        (numerator, dy)
    };
    Some(Crossing {
        numerator,
        denominator,
        direction: if b.y < a.y { 1 } else { -1 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXTENT: u32 = 4096;
    const E: i64 = EXTENT as i64;
    const Z: u32 = 2; // a 4x4 grid of tiles

    /// A closed axis-aligned square; clockwise is an exterior, anticlockwise a
    /// hole.
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

    /// Cut, then fill, as ingest does. Without the cut, coverage would fill
    /// boundary tiles too.
    fn cut_then_fill(rings: &[Vec<Coord<i64>>]) -> Pieces {
        let mut pieces = Pieces::new();
        for ring in rings {
            lyv_cut::cut_ring(1, ring, Z, EXTENT, &mut pieces);
        }
        fill_covered(1, rings, Z, EXTENT, &mut pieces);
        pieces
    }

    /// Which tiles gained a full square, sorted.
    fn filled(pieces: &Pieces, extent: u32) -> Vec<(u32, u32)> {
        let e = extent as Local;
        let full = [
            Coord { x: 0, y: 0 },
            Coord { x: e, y: 0 },
            Coord { x: e, y: e },
            Coord { x: 0, y: e },
            Coord { x: 0, y: 0 },
        ];
        let mut out: Vec<(u32, u32)> = pieces
            .iter()
            .filter(|(_, chains)| chains.iter().any(|c| c.points == full))
            .map(|(coord, _)| (coord.x, coord.y))
            .collect();
        out.sort_unstable();
        out
    }

    /// Brute-force winding number at a point: the oracle.
    fn winding_at(rings: &[Vec<Coord<i64>>], point: Coord<i64>) -> i32 {
        let mut total = 0;
        for ring in rings {
            for pair in ring.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                if (a.y <= point.y) == (b.y <= point.y) {
                    continue;
                }
                // Exact: is the crossing to the right of the point?
                let num = i128::from(a.x - point.x) * i128::from(b.y - a.y)
                    + i128::from(point.y - a.y) * i128::from(b.x - a.x);
                let right = if b.y > a.y { num > 0 } else { num < 0 };
                if right {
                    total += if b.y < a.y { 1 } else { -1 };
                }
            }
        }
        total
    }

    #[test]
    fn a_square_covering_four_inner_tiles_fills_exactly_those() {
        // Swallows the four middle tiles of a 4x4 grid and clips the rest.
        let ring = square(E / 2, 7 * E / 2, true);
        let pieces = cut_then_fill(std::slice::from_ref(&ring));

        assert_eq!(
            filled(&pieces, EXTENT),
            vec![(1, 1), (1, 2), (2, 1), (2, 2)]
        );
    }

    #[test]
    fn a_hole_swallowing_a_tile_leaves_it_empty() {
        // The hole-covered tile gains nothing (not even a coincident pair),
        // while tiles away from the hole still fill. Zoom 3 leaves room between
        // the hole's boundary tiles and the ones being checked.
        const Z3: u32 = 3;
        let rings = vec![
            square(E / 2, 15 * E / 2, true),
            // Swallows tile (2,2) whole, boundary staying clear of it.
            square(2 * E - 100, 3 * E + 100, false),
        ];

        let mut pieces = Pieces::new();
        for ring in &rings {
            lyv_cut::cut_ring(1, ring, Z3, EXTENT, &mut pieces);
        }
        let boundary: Vec<TileCoord> = pieces.keys().copied().collect();
        fill_covered(1, &rings, Z3, EXTENT, &mut pieces);

        let swallowed = TileCoord { z: Z3, x: 2, y: 2 };
        assert!(
            !boundary.contains(&swallowed),
            "the fixture is wrong: the hole's boundary reaches the tile"
        );
        assert!(
            !pieces.contains_key(&swallowed),
            "the hole-covered tile gained {:?}",
            pieces.get(&swallowed)
        );

        // A tile well clear of the hole, inside the exterior only, still fills.
        let clear = TileCoord { z: Z3, x: 5, y: 5 };
        assert!(!boundary.contains(&clear), "the fixture is wrong");
        assert_eq!(
            pieces.get(&clear).map(Vec::len),
            Some(1),
            "a tile inside the exterior only should gain exactly one square"
        );

        // And nowhere gained two.
        for (coord, chains) in &pieces {
            if boundary.contains(coord) {
                continue;
            }
            assert_eq!(
                chains.len(),
                1,
                "tile {coord:?} gained {} rings",
                chains.len()
            );
        }
    }

    #[test]
    fn a_tile_holding_only_a_hole_is_filled_even_when_its_centre_is_in_the_hole() {
        // A hole inside one tile, over its centre: the winding there is zero,
        // but the tile still needs its square. Built directly rather than via
        // the cut, to place the hole precisely.
        let exterior = square(E / 2, 7 * E / 2, true);
        let tile = TileCoord { z: Z, x: 1, y: 1 };

        // A hole around the centre of tile (1,1), comfortably inside it.
        let centre = (E + E / 2, E + E / 2);
        let hole = square(centre.0 - 500, centre.0 + 500, false);
        let rings = vec![exterior, hole.clone()];

        let mut pieces = Pieces::new();
        pieces.insert(
            tile,
            vec![Chain {
                feature: 1,
                // The hole in that tile's local coordinates, still a hole.
                points: hole
                    .iter()
                    .map(|p| Coord {
                        x: (p.x - E) as Local,
                        y: (p.y - E) as Local,
                    })
                    .collect(),
            }],
        );

        // Otherwise this proves nothing.
        assert_eq!(
            winding_at(
                &rings,
                Coord {
                    x: centre.0,
                    y: centre.1
                }
            ),
            0,
            "the fixture should make the winding at the tile centre zero"
        );

        fill_covered(1, &rings, Z, EXTENT, &mut pieces);

        assert!(
            filled(&pieces, EXTENT).contains(&(1, 1)),
            "the tile was left holding a lone hole: {:?}",
            pieces.get(&tile)
        );
    }

    #[test]
    fn a_ring_inside_a_hole_inside_a_ring_fills_again() {
        // Winding +1 - 1 + 1: one square, not three coincident ones.
        let rings = vec![
            square(E / 2, 7 * E / 2, true),
            square(E - 200, 2 * E + 200, false),
            square(E - 100, 2 * E + 100, true),
        ];
        let pieces = cut_then_fill(&rings);

        let tile = pieces
            .get(&TileCoord { z: Z, x: 1, y: 1 })
            .expect("the thrice-nested tile should fill");
        assert_eq!(tile.len(), 1, "one square, not three coincident ones");
        assert!(filled(&pieces, EXTENT).contains(&(1, 1)));
    }

    #[test]
    fn four_deep_nesting_cancels_again() {
        let rings = vec![
            square(E / 2, 7 * E / 2, true),
            square(E - 300, 2 * E + 300, false),
            square(E - 200, 2 * E + 200, true),
            square(E - 100, 2 * E + 100, false),
        ];
        let pieces = cut_then_fill(&rings);
        assert!(
            !filled(&pieces, EXTENT).contains(&(1, 1)),
            "winding is zero four deep, so the tile stays empty"
        );
    }

    #[test]
    fn a_tile_that_already_holds_boundary_is_left_alone() {
        let ring = square(E / 2, 7 * E / 2, true);
        let mut pieces = Pieces::new();
        let existing = Chain {
            feature: 1,
            points: vec![Coord { x: 10, y: 10 }, Coord { x: 20, y: 20 }],
        };
        pieces.insert(TileCoord { z: Z, x: 1, y: 1 }, vec![existing.clone()]);

        fill_covered(1, std::slice::from_ref(&ring), Z, EXTENT, &mut pieces);
        assert_eq!(pieces[&TileCoord { z: Z, x: 1, y: 1 }], vec![existing]);
    }

    #[test]
    fn another_features_boundary_does_not_block_coverage() {
        let ring = square(E / 2, 7 * E / 2, true);
        let mut pieces = Pieces::new();
        pieces.insert(
            TileCoord { z: Z, x: 1, y: 1 },
            vec![Chain {
                feature: 99,
                points: vec![Coord { x: 10, y: 10 }, Coord { x: 20, y: 20 }],
            }],
        );

        fill_covered(1, std::slice::from_ref(&ring), Z, EXTENT, &mut pieces);
        assert!(filled(&pieces, EXTENT).contains(&(1, 1)));
    }

    #[test]
    fn a_polygon_touching_no_tile_wholly_fills_nothing() {
        let ring = square(100, 200, true);
        let mut pieces = Pieces::new();
        fill_covered(1, std::slice::from_ref(&ring), Z, EXTENT, &mut pieces);
        assert!(pieces.is_empty());
        assert_eq!(
            filled(&cut_then_fill(std::slice::from_ref(&ring)), EXTENT),
            vec![]
        );
    }

    /// A square turned 45°, so every edge crosses a scanline at a non-integer x.
    fn diamond(cx: i64, cy: i64, r: i64, clockwise: bool) -> Vec<Coord<i64>> {
        let mut ring = vec![
            Coord { x: cx, y: cy - r },
            Coord { x: cx + r, y: cy },
            Coord { x: cx, y: cy + r },
            Coord { x: cx - r, y: cy },
            Coord { x: cx, y: cy - r },
        ];
        if !clockwise {
            ring.reverse();
        }
        ring
    }

    /// Check the scanline against brute force; returns (tiles checked, filled).
    fn agrees_with_brute_force(rings: &[Vec<Coord<i64>>], z: u32) -> (usize, usize) {
        let span = 1i64 << z;
        let mut pieces = Pieces::new();
        for ring in rings {
            lyv_cut::cut_ring(1, ring, z, EXTENT, &mut pieces);
        }
        let boundary: Vec<TileCoord> = pieces.keys().copied().collect();
        fill_covered(1, rings, z, EXTENT, &mut pieces);
        let got_filled = filled(&pieces, EXTENT);

        let (mut checked, mut count) = (0, 0);
        for ty in 0..span {
            for tx in 0..span {
                let coord = TileCoord {
                    z,
                    x: tx as u32,
                    y: ty as u32,
                };
                if boundary.contains(&coord) {
                    continue;
                }
                let centre = Coord {
                    x: tx * E + E / 2,
                    y: ty * E + E / 2,
                };
                let want = winding_at(rings, centre) != 0;
                let got = got_filled.contains(&(tx as u32, ty as u32));
                assert_eq!(
                    got, want,
                    "tile {tx},{ty} at zoom {z} filled={got}, winding says {want}"
                );
                checked += 1;
                count += usize::from(got);
            }
        }
        (checked, count)
    }

    #[test]
    fn diagonal_boundaries_agree_with_brute_force() {
        // Nested diamonds cross scanlines at non-integer x, exercising the
        // exact comparison. Zoom 4 leaves tiles wholly inside the sloping sides.
        const Z4: u32 = 4;
        let world = (1i64 << Z4) * E;
        let mut checked = 0;
        let mut count = 0;
        for step in 0..6 {
            let r = world / 2 - step * 137;
            let rings = vec![
                diamond(world / 2, world / 2, r, true),
                diamond(world / 2, world / 2, r / 2 - 331, false),
                diamond(world / 2, world / 2, r / 4 - 517, true),
            ];
            let (c, f) = agrees_with_brute_force(&rings, Z4);
            checked += c;
            count += f;
        }
        println!("diamonds: checked {checked}, filled {count}");
        assert!(count > 20, "only {count} tiles filled by diamonds");
    }

    #[test]
    fn boundaries_lying_exactly_on_tile_edges_agree_with_brute_force() {
        // Grid-aligned data puts vertices exactly on scanlines, which is what
        // the half-open rule is for.
        const Z4: u32 = 4;
        let mut checked = 0;
        let mut count = 0;
        for (lo, hi) in [(1i64, 15i64), (2, 14), (0, 16), (4, 12)] {
            let rings = vec![
                square(lo * E, hi * E, true),
                square((lo + 1) * E, (hi - 1) * E, false),
                square((lo + 2) * E, (hi - 2) * E, true),
            ];
            let valid: Vec<Vec<Coord<i64>>> = rings
                .into_iter()
                .filter(|r| r[0].x < r[2].x && r[0].y < r[2].y)
                .collect();
            if valid.is_empty() {
                continue;
            }
            let (c, f) = agrees_with_brute_force(&valid, Z4);
            checked += c;
            count += f;
        }
        println!("grid-aligned: checked {checked}, filled {count}");
        assert!(count > 5, "only {count} tiles filled");
    }

    #[test]
    fn a_vertex_sitting_exactly_on_the_scanline_is_counted_once() {
        // An apex touching a scanline must not change the winding.
        let apex = E / 2; // row 0's centre line
        let rings = vec![vec![
            Coord { x: 0, y: apex },
            Coord { x: E, y: 0 },
            Coord { x: 2 * E, y: apex },
            Coord { x: 2 * E, y: 4 * E },
            Coord { x: 0, y: 4 * E },
            Coord { x: 0, y: apex },
        ]];
        agrees_with_brute_force(&rings, Z);
    }

    #[test]
    fn every_decision_matches_a_brute_force_winding_at_the_tile_centre() {
        // Random nested rings: a boundary-free tile fills exactly when the
        // winding at its centre is non-zero.
        let mut seed = 0xBADA55E1FBABEu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let span = 1i64 << Z;
        let world = span * E;

        let mut checked = 0usize;
        let mut filled_total = 0usize;

        for case in 0..400 {
            // Nested squares of alternating winding.
            let depth = 1 + (next() % 4) as usize;
            let mut low = (next() % (E as u64)) as i64;
            let mut high = world - (next() % (E as u64)) as i64;
            let mut rings = Vec::new();
            for level in 0..depth {
                // Too small to swallow a tile whole.
                if high - low <= 2 * E {
                    break;
                }
                rings.push(square(low, high, level % 2 == 0));
                let inset = E / 2 + (next() % (E as u64)) as i64;
                low += inset;
                high -= inset;
            }
            if rings.is_empty() {
                continue;
            }

            let mut pieces = Pieces::new();
            for ring in &rings {
                lyv_cut::cut_ring(1, ring, Z, EXTENT, &mut pieces);
            }
            let boundary: Vec<TileCoord> = pieces.keys().copied().collect();

            fill_covered(1, &rings, Z, EXTENT, &mut pieces);
            let got_filled = filled(&pieces, EXTENT);

            for ty in 0..span {
                for tx in 0..span {
                    let coord = TileCoord {
                        z: Z,
                        x: tx as u32,
                        y: ty as u32,
                    };
                    if boundary.contains(&coord) {
                        continue; // close's job, not coverage's
                    }
                    let centre = Coord {
                        x: tx * E + E / 2,
                        y: ty * E + E / 2,
                    };
                    let want = winding_at(&rings, centre) != 0;
                    let got = got_filled.contains(&(tx as u32, ty as u32));
                    assert_eq!(
                        got, want,
                        "case {case}: tile {tx},{ty} filled={got} but winding \
                         says {want}"
                    );
                    checked += 1;
                    filled_total += usize::from(got);
                }
            }
        }

        println!("checked {checked} empty tiles, {filled_total} of them filled");
        assert!(checked > 1000, "only {checked} tiles checked");
        assert!(
            filled_total > 100,
            "only {filled_total} tiles filled, so filling is barely exercised"
        );
    }
}
