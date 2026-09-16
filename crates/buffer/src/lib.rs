//! Adds a margin of neighbouring geometry to a tile, for rendering.
//!
//! Internal tiles are clipped exactly, because the merge needs pieces to abut.
//! But a polygon clipped exactly runs along the tile edge, so stroking it draws
//! the tile grid. So on the way out, each of the eight neighbours' rings is
//! clipped into a margin around the tile and welded on. It's a copy of what the
//! neighbour itself draws, so the two agree, even after simplification.
//!
//! **Only polygons are buffered.** Two line pieces meeting at a tile edge
//! already draw as one.
//!
//! Carrying a buffer through the pyramid can't work: it halves at every merge.
//! Extruding the clipped edge outward would work for polygons only; copying is
//! exact.
//!
//! The margin is in 4096ths of a tile (`buffer * extent / 4096` units), so it's
//! the same fraction of a tile at any extent. No buffer can fix wide strokes:
//! the default 64 puts the artificial edge about 8 px outside a 512 px tile, so
//! strokes over about 16 px show the grid anyway. The real fix is a separate
//! outline layer.

use geo_types::Coord;
use lyv_core::{Chain, Local};
use lyv_seam::{double_area, is_ring};

/// Convert a margin given in 4096ths of a tile into tile units.
pub fn margin_units(buffer: u32, extent: u32) -> Local {
    (u64::from(buffer) * u64::from(extent) / 4096) as Local
}

/// A neighbouring tile's contribution: its offset from this tile in whole tile
/// steps, and its chains in its own local coordinates.
pub type Neighbour<'a> = ((Local, Local), &'a [Chain]);

/// Extend `own` with the parts of `neighbours` that fall within the margin.
///
/// `margin` is in tile units ([`margin_units`]). Zero returns `own` unchanged;
/// callers should then skip reading neighbours. Own geometry passes through.
pub fn add_margin(
    own: &[Chain],
    neighbours: &[Neighbour<'_>],
    extent: u32,
    margin: Local,
) -> Vec<Chain> {
    if margin <= 0 {
        return own.to_vec();
    }

    let extent = extent as Local;
    let mut out = own.to_vec();
    // Closed per neighbour, then gathered. Closed all at once, the perimeter
    // walk pairs one neighbour's exit with another's entry, and welding then
    // cancels the wrong rings until a fully covered tile comes out empty.
    let mut clipped_rings: Vec<Chain> = Vec::new();

    for ((dx, dy), chains) in neighbours {
        let mut from_here: Vec<Chain> = Vec::new();
        // Area each feature brought, before clipping; see `within_its_means`.
        let mut brought: std::collections::HashMap<u64, i64> = std::collections::HashMap::new();
        let offset = Coord {
            x: dx * extent,
            y: dy * extent,
        };
        for chain in *chains {
            if !is_ring(&chain.points) {
                // Lines aren't buffered.
                continue;
            }
            // Measured before the shift: translation doesn't change a closed
            // ring's shoelace sum.
            *brought.entry(chain.feature).or_default() += double_area(&chain.points).abs();

            // Most rings can't reach the margin — only a sliver along the
            // shared edge can. A bounding box rejects the rest before the copy,
            // which was the largest single cost in the run.
            if !reaches(&chain.points, offset, -margin, extent + margin) {
                continue;
            }

            let shifted: Vec<Coord<Local>> = chain
                .points
                .iter()
                .map(|p| Coord {
                    x: p.x + offset.x,
                    y: p.y + offset.y,
                })
                .collect();
            let shifted = begin_outside(&shifted, -margin, extent + margin);

            for piece in clip(&shifted, -margin, extent + margin) {
                from_here.push(Chain {
                    feature: chain.feature,
                    points: piece,
                });
            }
        }

        if !from_here.is_empty() {
            let closed = lyv_close::close_rings_within(
                &from_here,
                i64::from(-margin),
                i64::from(extent + margin),
            );
            let side = i64::from(extent + 2 * margin);
            clipped_rings.extend(closed.into_iter().filter(|ring| {
                within_its_means(ring, brought.get(&ring.feature).copied().unwrap_or(0), side)
            }));
        }
    }

    if !clipped_rings.is_empty() {
        // Weld the margin to our own rings, across the tile's four edges.
        // Without welding, each piece keeps its artificial edge along the
        // shared boundary, and stroking draws the grid. After it, what's left
        // is the real boundary plus runs along the margin box, outside the
        // tile, where the renderer clips them. Rings only: `weld` walks chains
        // cyclically, so an open line would be mangled.
        let mut all: Vec<Chain> = own.iter().filter(|c| is_ring(&c.points)).cloned().collect();
        all.extend(clipped_rings);
        out.retain(|chain| !is_ring(&chain.points));

        let seams = [0, extent];
        for welded in lyv_seam::weld(&all, &seams, &seams) {
            if let Some(points) = lyv_seam::tidy(welded.points) {
                out.push(Chain {
                    feature: welded.feature,
                    points,
                });
            }
        }
    }
    out
}

/// Whether a closed fragment encloses no more than the ring it came from, as
/// it must: clipping can't create area.
///
/// Closing always produces an exterior. Given a fragment of a *hole*, it walks
/// the long way round and returns the box's complement, covering the whole
/// tile. Small rings can invert into holes on the way up the pyramid. It
/// happened: a vacant lot in the next tile, two units into this tile's margin,
/// filled all of tile 7/33/51.
///
/// Checked by area because closing has already made the winding positive. The
/// slack is a unit of rounding around the box; a complement is off by millions.
fn within_its_means(ring: &Chain, brought: i64, side: i64) -> bool {
    double_area(&ring.points).abs() <= brought + 4 * side
}

/// Whether any part of this ring could land inside the box once shifted. A
/// bounding-box test before the copy; it only rejects, the clip still decides.
fn reaches(points: &[Coord<Local>], offset: Coord<Local>, low: Local, high: Local) -> bool {
    let (mut min_x, mut max_x) = (Local::MAX, Local::MIN);
    let (mut min_y, mut max_y) = (Local::MAX, Local::MIN);
    for point in points {
        min_x = min_x.min(point.x);
        max_x = max_x.max(point.x);
        min_y = min_y.min(point.y);
        max_y = max_y.max(point.y);
    }
    // Saturating: a rejection is the safe answer to nonsense coordinates.
    min_x.saturating_add(offset.x) <= high
        && max_x.saturating_add(offset.x) >= low
        && min_y.saturating_add(offset.y) <= high
        && max_y.saturating_add(offset.y) >= low
}

/// Rotate a closed ring to start at a vertex outside the box, so its run
/// through the box isn't split at the list's start — the same reason
/// `cut_ring` splices. Unchanged if every vertex is inside.
fn begin_outside(closed: &[Coord<Local>], low: Local, high: Local) -> Vec<Coord<Local>> {
    let inside = |p: &Coord<Local>| (low..=high).contains(&p.x) && (low..=high).contains(&p.y);
    let points = &closed[..closed.len() - 1]; // drop the repeated closing point
    let Some(start) = points.iter().position(|p| !inside(p)) else {
        return closed.to_vec();
    };

    let mut rotated: Vec<Coord<Local>> = points[start..]
        .iter()
        .chain(&points[..start])
        .copied()
        .collect();
    rotated.push(rotated[0]);
    rotated
}

/// The parts of a chain inside the square `[low, high]`, in order. A chain that
/// leaves and re-enters comes back as separate pieces, not one with a jump.
fn clip(points: &[Coord<Local>], low: Local, high: Local) -> Vec<Vec<Coord<Local>>> {
    let mut pieces: Vec<Vec<Coord<Local>>> = Vec::new();
    let mut current: Vec<Coord<Local>> = Vec::new();

    for pair in points.windows(2) {
        let Some((from, to)) = clip_segment(pair[0], pair[1], low, high) else {
            // Wholly outside, so whatever we were building ends here.
            if current.len() >= 2 {
                pieces.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            continue;
        };

        if current.last() == Some(&from) {
            current.push(to);
        } else {
            if current.len() >= 2 {
                pieces.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            current.push(from);
            current.push(to);
        }
    }

    if current.len() >= 2 {
        pieces.push(current);
    }
    pieces
}

/// The part of one segment inside the square, or `None`.
///
/// Liang-Barsky, with clipped endpoints computed exactly, so a clipped point
/// lands exactly on its boundary.
fn clip_segment(
    p: Coord<Local>,
    q: Coord<Local>,
    low: Local,
    high: Local,
) -> Option<(Coord<Local>, Coord<Local>)> {
    let (dx, dy) = (
        i64::from(q.x) - i64::from(p.x),
        i64::from(q.y) - i64::from(p.y),
    );

    // The inside interval as a fraction of the segment, as exact rationals.
    let mut enter = (0i64, 1i64);
    let mut leave = (1i64, 1i64);

    for (delta, start) in [(dx, p.x), (dy, p.y)] {
        for (sign, bound) in [(-1i64, low), (1i64, high)] {
            // Half-plane sign * (start + t*delta) <= sign * bound.
            let numerator = sign * (i64::from(bound) - i64::from(start));
            let denominator = sign * delta;

            if denominator == 0 {
                if numerator < 0 {
                    return None; // parallel and outside
                }
                continue;
            }
            if denominator > 0 {
                // Bounds `leave` from above.
                if compare(numerator, denominator, leave.0, leave.1).is_lt() {
                    leave = (numerator, denominator);
                }
            } else {
                // Bounds `enter` from below; flip the sign to keep it positive.
                let (numerator, denominator) = (-numerator, -denominator);
                if compare(numerator, denominator, enter.0, enter.1).is_gt() {
                    enter = (numerator, denominator);
                }
            }
            if compare(enter.0, enter.1, leave.0, leave.1).is_gt() {
                return None;
            }
        }
    }

    Some((
        at(p, dx, dy, enter, low, high),
        at(p, dx, dy, leave, low, high),
    ))
}

fn compare(a_num: i64, a_den: i64, b_num: i64, b_den: i64) -> std::cmp::Ordering {
    (i128::from(a_num) * i128::from(b_den)).cmp(&(i128::from(b_num) * i128::from(a_den)))
}

/// The point at fraction `t` along the segment.
///
/// No clamping needed. On the clipped axis the division is exact and lands on
/// the bound; on the other, the true value is inside the bound and rounding
/// moves it at most half a unit, so it can reach the bound but not pass it.
/// The `debug_assert` checks this.
fn at(p: Coord<Local>, dx: i64, dy: i64, t: (i64, i64), low: Local, high: Local) -> Coord<Local> {
    let (num, den) = t;
    let coordinate = |start: Local, delta: i64| -> Local {
        if num == 0 {
            return start;
        }
        let value =
            i128::from(start) + div_round(i128::from(num) * i128::from(delta), i128::from(den));
        debug_assert!(
            value >= i128::from(low) && value <= i128::from(high),
            "clipped coordinate {value} escaped [{low}, {high}]"
        );
        value as Local
    };
    Coord {
        x: coordinate(p.x, dx),
        y: coordinate(p.y, dy),
    }
}

/// Division rounding to nearest, halves away from zero, like the cut.
fn div_round(numerator: i128, denominator: i128) -> i128 {
    let (numerator, denominator) = if denominator < 0 {
        (-numerator, -denominator)
    } else {
        (numerator, denominator)
    };
    let half = denominator / 2;
    if numerator >= 0 {
        (numerator + half) / denominator
    } else {
        (numerator - half) / denominator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owned neighbours for tests; `add_margin` borrows them.
    fn borrowed(owned: &[((Local, Local), Vec<Chain>)]) -> Vec<Neighbour<'_>> {
        owned
            .iter()
            .map(|(at, chains)| (*at, chains.as_slice()))
            .collect()
    }

    const EXTENT: u32 = 4096;
    const E: Local = EXTENT as Local;
    const M: Local = 80;

    fn chain(feature: u64, points: &[(Local, Local)]) -> Chain {
        Chain {
            feature,
            points: points.iter().map(|&(x, y)| Coord { x, y }).collect(),
        }
    }

    /// A closed ring.
    fn ring_chain(feature: u64, points: &[(Local, Local)]) -> Chain {
        let mut pts: Vec<Coord<Local>> = points.iter().map(|&(x, y)| Coord { x, y }).collect();
        pts.push(pts[0]);
        Chain {
            feature,
            points: pts,
        }
    }

    fn local(points: &[(Local, Local)]) -> Vec<Coord<Local>> {
        points.iter().map(|&(x, y)| Coord { x, y }).collect()
    }

    fn points_of(chains: &[Chain]) -> Vec<Vec<(Local, Local)>> {
        chains
            .iter()
            .map(|c| c.points.iter().map(|p| (p.x, p.y)).collect())
            .collect()
    }

    #[test]
    fn the_margin_is_the_same_fraction_of_a_tile_at_any_extent() {
        // At the default extent a buffer unit is a tile unit.
        assert_eq!(margin_units(64, 4096), 64);
        // Twice the extent, twice the units, same fraction of the tile.
        assert_eq!(margin_units(64, 8192), 128);
        assert_eq!(margin_units(0, 4096), 0);
    }

    #[test]
    fn a_margin_of_zero_changes_nothing() {
        let own = vec![chain(1, &[(100, 100), (200, 200)])];
        let east = vec![((1, 0), vec![chain(1, &[(0, 200), (500, 200)])])];
        assert_eq!(add_margin(&own, &borrowed(&east), EXTENT, 0), own);
    }

    #[test]
    fn own_geometry_passes_through_untouched() {
        let own = vec![
            chain(1, &[(0, 0), (E, E)]),
            chain(2, &[(5, 4090), (10, 10)]),
        ];
        let out = add_margin(&own, &[], EXTENT, M);
        assert_eq!(out, own);
    }

    #[test]
    fn the_eastern_neighbours_polygon_appears_beyond_the_right_edge() {
        // Our ring stops at the shared edge and the neighbour's continues, so
        // they weld and reach into the margin.
        let own = vec![ring_chain(
            1,
            &[(E - 500, 100), (E, 100), (E, 300), (E - 500, 300)],
        )];
        let east = vec![(
            (1, 0),
            vec![ring_chain(1, &[(0, 100), (500, 100), (500, 300), (0, 300)])],
        )];

        let out = add_margin(&own, &borrowed(&east), EXTENT, M);
        assert_eq!(out.len(), 1, "the two should weld into one ring: {out:?}");
        let xs: Vec<Local> = out[0].points.iter().map(|p| p.x).collect();
        assert_eq!(
            xs.iter().copied().max(),
            Some(E + M),
            "did not reach the margin"
        );
        assert!(
            !out[0].points.windows(2).any(|w| w[0].x == E && w[1].x == E),
            "the shared edge survived: {:?}",
            out[0].points
        );
    }

    #[test]
    fn a_neighbours_ring_never_buffers_into_more_area_than_it_had() {
        // Clipping can't create area, so a neighbour's contribution is never
        // larger than its ring. A thin fragment whose two clip points land a
        // unit apart can read as a hole (double area -2), and closing then
        // returns the tile's complement, as happened to a parcel in tile
        // 7/33/51. Swept over positions, since which way the rounding falls is
        // the point.
        for tip in 0..8 {
            for rise in 1..8 {
                let east = vec![(
                    (1, 0),
                    vec![ring_chain(
                        2,
                        // One vertex in the margin, the rest clear, wound as a
                        // hole — what a small ring can become after rounding.
                        &[(M - tip, 495), (250, 600 + rise), (200, 400 - rise)],
                    )],
                )];
                let source = double_area(&east[0].1[0].points).abs();
                let own = vec![ring_chain(1, &[(100, 100), (200, 100), (200, 200)])];

                for ring in add_margin(&own, &borrowed(&east), EXTENT, M) {
                    if ring.feature != 2 {
                        continue;
                    }
                    // A unit of rounding round the box; a complement is off by
                    // millions.
                    let slack = 4 * i64::from(E + 2 * M);
                    assert!(
                        double_area(&ring.points).abs() <= source + slack,
                        "tip {tip} rise {rise}: a ring of area {} came out of one of area {source}: {:?}",
                        double_area(&ring.points).abs(),
                        ring.points
                    );
                }
            }
        }
    }

    #[test]
    fn the_western_neighbours_polygon_appears_before_the_left_edge() {
        let own = vec![ring_chain(1, &[(0, 100), (500, 100), (500, 300), (0, 300)])];
        let west = vec![(
            (-1, 0),
            vec![ring_chain(
                1,
                &[(E - 500, 100), (E, 100), (E, 300), (E - 500, 300)],
            )],
        )];

        let out = add_margin(&own, &borrowed(&west), EXTENT, M);
        assert_eq!(out.len(), 1, "got {out:?}");
        let xs: Vec<Local> = out[0].points.iter().map(|p| p.x).collect();
        assert_eq!(xs.iter().copied().min(), Some(-M));
    }

    #[test]
    fn the_southern_neighbours_polygon_appears_below_the_bottom_edge() {
        // y grows downwards, so south is +y.
        let own = vec![ring_chain(
            1,
            &[(100, E - 500), (300, E - 500), (300, E), (100, E)],
        )];
        let south = vec![(
            (0, 1),
            vec![ring_chain(1, &[(100, 0), (300, 0), (300, 500), (100, 500)])],
        )];

        let out = add_margin(&own, &borrowed(&south), EXTENT, M);
        assert_eq!(out.len(), 1, "got {out:?}");
        let ys: Vec<Local> = out[0].points.iter().map(|p| p.y).collect();
        assert_eq!(ys.iter().copied().max(), Some(E + M));
    }

    #[test]
    fn a_diagonal_neighbours_polygon_fills_the_corner() {
        let southeast = vec![(
            (1, 1),
            vec![ring_chain(1, &[(0, 0), (60, 0), (60, 60), (0, 60)])],
        )];
        let out = add_margin(&[], &borrowed(&southeast), EXTENT, M);

        // As a set: the ring is rotated before clipping, so its first vertex
        // is arbitrary.
        assert_eq!(out.len(), 1);
        let mut got: Vec<(Local, Local)> = out[0].points.iter().map(|p| (p.x, p.y)).collect();
        got.pop(); // the repeated closing point
        got.sort_unstable();
        assert_eq!(
            got,
            vec![(E, E), (E, E + 60), (E + 60, E), (E + 60, E + 60)]
        );
        assert_eq!(
            double_area(&out[0].points),
            2 * 60 * 60,
            "the corner square should survive whole"
        );
    }

    #[test]
    fn a_neighbour_beyond_the_margin_contributes_nothing() {
        let own = vec![ring_chain(1, &[(0, 0), (100, 0), (100, 100), (0, 100)])];
        // Starts 200 units into the eastern neighbour.
        let east = vec![(
            (1, 0),
            vec![ring_chain(
                1,
                &[(200, 200), (900, 200), (900, 400), (200, 400)],
            )],
        )];

        assert_eq!(add_margin(&own, &borrowed(&east), EXTENT, M), own);
    }

    #[test]
    fn clipping_cuts_a_chain_at_the_margin() {
        // The clip itself, tested directly: through `add_margin` the result is
        // closed again and the cut isn't visible.
        let points = local(&[(E, 300), (E + 400, 300)]);
        assert_eq!(
            clip(&points, -M, E + M),
            vec![local(&[(E, 300), (E + M, 300)])]
        );
    }

    #[test]
    fn clipping_a_chain_that_leaves_and_re_enters_gives_two_pieces() {
        // Two pieces, not one with a jump in it.
        let points = local(&[(E, 100), (E + 400, 100), (E + 400, 900), (E, 900)]);
        assert_eq!(
            clip(&points, -M, E + M),
            vec![
                local(&[(E, 100), (E + M, 100)]),
                local(&[(E + M, 900), (E, 900)]),
            ]
        );
    }

    #[test]
    fn feature_ids_come_across_with_the_geometry() {
        // The margin keeps feature ids, so a renderer knows it's the same
        // feature.
        let east = vec![(
            (1, 0),
            vec![ring_chain(77, &[(0, 300), (40, 300), (40, 500), (0, 500)])],
        )];
        let out = add_margin(&[], &borrowed(&east), EXTENT, M);
        assert_eq!(out.iter().map(|c| c.feature).collect::<Vec<_>>(), vec![77]);
    }

    #[test]
    fn a_neighbours_ring_wholly_inside_the_margin_comes_across_whole() {
        let east = vec![(
            (1, 0),
            vec![ring_chain(1, &[(10, 100), (50, 100), (50, 900), (10, 900)])],
        )];
        let out = add_margin(&[], &borrowed(&east), EXTENT, M);
        assert_eq!(out.len(), 1);
        assert_eq!(
            points_of(&out),
            vec![vec![
                (E + 10, 100),
                (E + 50, 100),
                (E + 50, 900),
                (E + 10, 900),
                (E + 10, 100),
            ]]
        );
    }

    #[test]
    fn a_fully_covered_tile_survives_being_buffered() {
        // Every piece has all four edges on a seam. Closing the neighbours
        // together would pair the wrong exits and entries, and welding would
        // cancel everything. The tile used to come out empty.
        let full = || ring_chain(1, &[(0, 0), (E, 0), (E, E), (0, E)]);
        let neighbours: Vec<((Local, Local), Vec<Chain>)> = [
            (-1, -1),
            (0, -1),
            (1, -1),
            (-1, 0),
            (1, 0),
            (-1, 1),
            (0, 1),
            (1, 1),
        ]
        .into_iter()
        .map(|offset| (offset, vec![full()]))
        .collect();

        let out = add_margin(&[full()], &borrowed(&neighbours), EXTENT, M);

        assert_eq!(out.len(), 1, "got {} rings", out.len());
        let side = i64::from(E + 2 * M);
        assert_eq!(
            double_area(&out[0].points),
            2 * side * side,
            "the buffered tile should be one solid square"
        );
    }

    #[test]
    fn buffering_never_removes_a_tiles_own_geometry() {
        // A lower bound on what survives: the other tests would pass an
        // implementation that deleted everything, and one did.
        let own = vec![ring_chain(
            1,
            &[(100, 100), (900, 100), (900, 900), (100, 900)],
        )];
        let before = double_area(&own[0].points);

        for neighbours in [
            Vec::new(),
            vec![(
                (1, 0),
                vec![ring_chain(1, &[(0, 100), (500, 100), (500, 900), (0, 900)])],
            )],
            vec![
                (
                    (1, 0),
                    vec![ring_chain(1, &[(0, 100), (500, 100), (500, 900), (0, 900)])],
                ),
                (
                    (0, 1),
                    vec![ring_chain(1, &[(100, 0), (900, 0), (900, 500), (100, 500)])],
                ),
            ],
        ] {
            let out = add_margin(&own, &borrowed(&neighbours), EXTENT, M);
            let after: i64 = out.iter().map(|c| double_area(&c.points)).sum();
            assert!(
                after >= before,
                "buffering lost area: {after} against {before} with {} neighbour(s)",
                neighbours.len()
            );
        }
    }

    #[test]
    fn a_neighbours_ring_arrives_closed_rather_than_as_loose_lines() {
        // Margin pieces must be closed against the margin box, or the feature
        // stops being all rings and is encoded as a linestring.
        let neighbour = vec![Chain {
            feature: 1,
            points: ring_chain(1, &[(0, 0), (E, 0), (E, E), (0, E)]).points,
        }];
        let out = add_margin(&[], &[((0, -1), neighbour.as_slice())], EXTENT, M);

        assert!(!out.is_empty(), "the margin gained nothing at all");
        for chain in &out {
            assert_eq!(
                chain.points.first(),
                chain.points.last(),
                "a clipped ring came back open: {:?}",
                chain.points
            );
        }
    }

    #[test]
    fn a_ring_whose_start_vertex_is_inside_the_box_is_not_split() {
        // A ring starting inside the box must not have its run split at the
        // start. This one starts at (0, 0), inside the margin box after the
        // northern shift.
        let neighbour = vec![Chain {
            feature: 1,
            points: ring_chain(1, &[(0, 0), (E, 0), (E, E), (0, E)]).points,
        }];
        let out = add_margin(&[], &[((0, -1), neighbour.as_slice())], EXTENT, M);

        for chain in &out {
            for point in &chain.points {
                assert!(
                    (-M..=E + M).contains(&point.x) && (-M..=E + M).contains(&point.y),
                    "{point:?} escaped the margin box"
                );
            }
        }
        assert!(
            out.iter().all(|c| c.points.first() == c.points.last()),
            "the ring came back in pieces: {:?}",
            out.iter().map(|c| c.points.len()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_neighbours_line_is_not_brought_across_at_all() {
        // Lines aren't buffered.
        let neighbour = vec![chain(1, &[(0, 300), (400, 300)])];
        assert!(add_margin(&[], &[((1, 0), neighbour.as_slice())], EXTENT, M).is_empty());
    }

    #[test]
    fn a_line_in_the_tile_survives_a_neighbours_polygon_being_welded() {
        // An open line handed to `weld` would be mangled into a ring.
        let river = chain(7, &[(0, 200), (E, 200)]);
        let own = vec![
            river.clone(),
            Chain {
                feature: 1,
                points: {
                    let mut p = vec![
                        Coord { x: 0, y: 0 },
                        Coord { x: E, y: 0 },
                        Coord { x: E, y: 100 },
                        Coord { x: 0, y: 100 },
                    ];
                    p.push(p[0]);
                    p
                },
            },
        ];
        let neighbour = vec![Chain {
            feature: 1,
            points: {
                let mut p = vec![
                    Coord { x: 0, y: E - 100 },
                    Coord { x: E, y: E - 100 },
                    Coord { x: E, y: E },
                    Coord { x: 0, y: E },
                ];
                p.push(p[0]);
                p
            },
        }];

        let out = add_margin(&own, &[((0, -1), neighbour.as_slice())], EXTENT, M);
        assert!(
            out.contains(&river),
            "the river was altered by welding: {:?}",
            out.iter().filter(|c| c.feature == 7).collect::<Vec<_>>()
        );
    }

    #[test]
    fn our_own_lines_still_pass_through_untouched() {
        // The tile's own lines survive exactly.
        let river = chain(7, &[(0, 200), (E, 200)]);
        let own = vec![river.clone()];
        let east = vec![((1, 0), vec![chain(1, &[(0, 300), (400, 300)])])];

        assert_eq!(add_margin(&own, &borrowed(&east), EXTENT, M), own);
    }

    #[test]
    fn nothing_in_the_result_lies_outside_the_margin() {
        // Whatever comes in, the output stays within the tile plus margin.
        let mut seed = 0xBADA55E1FBABEu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for case in 0..2000 {
            let neighbours: Vec<((Local, Local), Vec<Chain>)> =
                [(-1, 0), (1, 0), (0, -1), (0, 1), (1, 1), (-1, -1)]
                    .into_iter()
                    .map(|offset| {
                        let chains = (0..1 + next() % 3)
                            .map(|_| Chain {
                                feature: 1,
                                points: (0..2 + (next() % 6) as usize)
                                    .map(|_| Coord {
                                        x: (next() % EXTENT as u64) as Local,
                                        y: (next() % EXTENT as u64) as Local,
                                    })
                                    .collect(),
                            })
                            .collect();
                        (offset, chains)
                    })
                    .collect();

            let out = add_margin(&[], &borrowed(&neighbours), EXTENT, M);
            for c in &out {
                assert!(c.points.len() >= 2, "case {case}: a one-point chain");
                for p in &c.points {
                    assert!(
                        (-M..=E + M).contains(&p.x) && (-M..=E + M).contains(&p.y),
                        "case {case}: {p:?} is outside the tile plus margin"
                    );
                }
            }
        }
    }
}
