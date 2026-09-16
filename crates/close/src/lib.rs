//! Rejoins a polygon's clipped chains into closed rings along the tile edge.
//!
//! The second half of
//! [Weiler-Atherton](https://en.wikipedia.org/wiki/Weiler%E2%80%93Atherton_clipping_algorithm):
//! the cut has already produced the runs of boundary inside the tile, so from
//! each chain's exit, walk forward along the tile perimeter to the next entry.
//! Sutherland-Hodgman gives the same answer but needs the whole ring per tile,
//! so it is only the test oracle (and `docs/close.py`).
//!
//! Easy to get wrong:
//!
//! - **The walk emits the corners it passes.** A straight line from exit to
//!   entry is only right when both are on the same edge.
//! - **Closing isn't per chain.** A band crossing a tile joins one chain's exit
//!   to the *other* chain's entry.
//! - **The walk direction is fixed**: clockwise on screen, like an MVT exterior.
//!
//! **Rings must be consistently oriented** — exteriors clockwise on screen,
//! holes anticlockwise — or the result is inside out. Given that, holes need no
//! special handling: a hole biting the tile edge just becomes part of the
//! exterior's boundary. `lyv-source` imposes the orientation.
//!
//! A tile wholly inside a polygon has no chains, so it is `lyv-coverage`'s job
//! (design notes §5).

use geo_types::Coord;
use lyv_core::Local;

pub use lyv_core::Chain;

/// Close one tile's open chains into rings, per feature. Chains that are
/// already rings pass through.
///
/// Rings come back explicitly closed (the last point repeats the first).
pub fn close_rings(chains: &[Chain], extent: u32) -> Vec<Chain> {
    close_rings_within(chains, 0, i64::from(extent))
}

/// As [`close_rings`], but against any square box. The buffer closes its
/// margin pieces against `[-margin, extent + margin]`.
pub fn close_rings_within(chains: &[Chain], low: i64, high: i64) -> Vec<Chain> {
    let mut out = Vec::new();

    // A map, not a scan per feature, which is quadratic near zoom 0. Sorted so
    // the output is reproducible.
    let mut grouped: std::collections::BTreeMap<u64, Vec<&Chain>> = Default::default();
    for chain in chains {
        grouped.entry(chain.feature).or_default().push(chain);
    }

    for (feature, group) in grouped {
        close_feature(&group, low, high, &mut |ring| {
            out.push(Chain {
                feature,
                points: ring.to_vec(),
            })
        });
    }
    out
}

/// The doubled area that [`close_rings_within`] would produce, without keeping
/// the rings. Same walk, same code.
///
/// Lets the cut check a feature's pieces will close back to its own area while
/// the feature is still in hand.
pub fn closed_area_within(chains: &[Chain], low: i64, high: i64) -> i128 {
    let mut grouped: std::collections::BTreeMap<u64, Vec<&Chain>> = Default::default();
    for chain in chains {
        grouped.entry(chain.feature).or_default().push(chain);
    }

    let mut total = 0i128;
    for (_, group) in grouped {
        close_feature(&group, low, high, &mut |ring| total += double_area(ring));
    }
    total
}

/// Doubled shoelace area. Positive is clockwise on screen, consistent with MVT.
fn double_area(ring: &[Coord<Local>]) -> i128 {
    ring.windows(2)
        .map(|w| i128::from(w[0].x) * i128::from(w[1].y) - i128::from(w[1].x) * i128::from(w[0].y))
        .sum()
}

/// One chain waiting to be joined, with where it meets the tile boundary.
struct Open<'a> {
    points: &'a [Coord<Local>],
    entry: i64,
    exit: i64,
}

/// Walk one feature's chains into closed rings, lending each to `emit`.
fn close_feature(chains: &[&Chain], low: i64, high: i64, emit: &mut dyn FnMut(&[Coord<Local>])) {
    let mut open: Vec<Open> = Vec::new();

    for chain in chains {
        let points = &chain.points[..];
        if points.len() >= 4 && points.first() == points.last() {
            // Already a ring, lying wholly inside the tile.
            emit(points);
            continue;
        }
        match (
            points.first().and_then(|&p| perimeter_t(p, low, high)),
            points.last().and_then(|&p| perimeter_t(p, low, high)),
        ) {
            (Some(entry), Some(exit)) if points.len() >= 2 => open.push(Open {
                points,
                entry,
                exit,
            }),
            // Not a clipped ring. Passed through so bad input stays visible.
            _ => emit(points),
        }
    }

    let perimeter = 4 * (high - low);
    let mut used = vec![false; open.len()];

    for start in 0..open.len() {
        if used[start] {
            continue;
        }
        let mut ring: Vec<Coord<Local>> = Vec::new();
        let mut current = start;

        loop {
            used[current] = true;
            let points = open[current].points;
            if ring.last() == points.first() {
                ring.extend_from_slice(&points[1..]);
            } else {
                ring.extend_from_slice(points);
            }

            // The next entry walking forward. `start` stays a candidate, which
            // is how the ring closes. A distance of zero counts, so two chains
            // meeting at one boundary point join there.
            let exit = open[current].exit;
            let next = (0..open.len())
                .filter(|&i| !used[i] || i == start)
                .min_by_key(|&i| (open[i].entry - exit).rem_euclid(perimeter))
                .expect("start is always a candidate");

            // No dedup needed: a corner strictly between exit and entry can't
            // equal either end.
            for corner in corners_between(exit, open[next].entry, low, high) {
                debug_assert_ne!(
                    ring.last(),
                    Some(&corner),
                    "a corner strictly between the ends repeated one of them"
                );
                ring.push(corner);
            }

            if next == start {
                break;
            }
            current = next;
        }

        if ring.first() != ring.last() {
            ring.push(ring[0]);
        }
        if ring.len() >= 4 {
            emit(&ring);
        }
    }
}

/// Where a point sits along the perimeter, or `None` if it isn't on it.
/// Clockwise on screen from the top-left corner:
///
/// ```text
///   0 .. E    top edge,    y = 0, x rising
///   E .. 2E   right edge,  x = E, y rising
///   2E .. 3E  bottom edge, y = E, x falling
///   3E .. 4E  left edge,   x = 0, y falling
/// ```
///
/// Corners are matched first, so the top-left is `0` and never `4E`.
fn perimeter_t(p: Coord<Local>, low: i64, high: i64) -> Option<i64> {
    let (x, y) = (i64::from(p.x), i64::from(p.y));
    let side = high - low;
    let on_axis = |v: i64| (low..=high).contains(&v);

    if x == low && y == low {
        return Some(0);
    }
    if x == high && y == low {
        return Some(side);
    }
    if x == high && y == high {
        return Some(2 * side);
    }
    if x == low && y == high {
        return Some(3 * side);
    }
    if y == low && on_axis(x) {
        return Some(x - low);
    }
    if x == high && on_axis(y) {
        return Some(side + (y - low));
    }
    if y == high && on_axis(x) {
        return Some(2 * side + (high - x));
    }
    if x == low && on_axis(y) {
        return Some(3 * side + (high - y));
    }
    None
}

/// The corners strictly between two perimeter positions, in walking order.
fn corners_between(from: i64, to: i64, low: i64, high: i64) -> Vec<Coord<Local>> {
    let side = high - low;
    let perimeter = 4 * side;
    let span = (to - from).rem_euclid(perimeter);

    let mut found: Vec<(i64, Coord<Local>)> = (0..4)
        .map(|k| {
            let t = k * side;
            let corner = match k {
                0 => (low, low),
                1 => (high, low),
                2 => (high, high),
                _ => (low, high),
            };
            (
                (t - from).rem_euclid(perimeter),
                Coord {
                    x: corner.0 as Local,
                    y: corner.1 as Local,
                },
            )
        })
        .filter(|&(distance, _)| 0 < distance && distance < span)
        .collect();

    found.sort_unstable_by_key(|&(distance, _)| distance);
    found.into_iter().map(|(_, corner)| corner).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXTENT: u32 = 4096;
    const E: Local = EXTENT as Local;

    fn chain(feature: u64, points: &[(Local, Local)]) -> Chain {
        Chain {
            feature,
            points: points.iter().map(|&(x, y)| Coord { x, y }).collect(),
        }
    }

    fn points_of(chains: &[Chain]) -> Vec<Vec<(Local, Local)>> {
        chains
            .iter()
            .map(|c| c.points.iter().map(|p| (p.x, p.y)).collect())
            .collect()
    }

    /// Shoelace area. Positive is clockwise on screen, consistent with MVT.
    fn area(ring: &[(Local, Local)]) -> f64 {
        let mut total = 0i64;
        for pair in ring.windows(2) {
            let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
            total += i64::from(x0) * i64::from(y1) - i64::from(x1) * i64::from(y0);
        }
        total as f64 / 2.0
    }

    fn assert_closed(chains: &[Chain]) {
        for c in chains {
            assert!(c.points.len() >= 4, "a ring needs three distinct points");
            assert_eq!(
                c.points.first(),
                c.points.last(),
                "a ring must be explicitly closed"
            );
            for p in &c.points {
                assert!(
                    (0..=E).contains(&p.x) && (0..=E).contains(&p.y),
                    "{p:?} escaped the tile"
                );
            }
        }
    }

    #[test]
    fn a_ring_wholly_inside_the_tile_passes_through_untouched() {
        let ring = chain(1, &[(100, 100), (900, 100), (900, 900), (100, 100)]);
        let out = close_rings(std::slice::from_ref(&ring), EXTENT);
        assert_eq!(out, vec![ring]);
    }

    #[test]
    fn a_band_crossing_the_tile_closes_along_the_edge_it_left() {
        // The left half of the tile: one chain, top edge to bottom edge.
        let band = chain(1, &[(2048, 0), (2048, E)]);
        let out = close_rings(&[band], EXTENT);

        assert_eq!(
            points_of(&out),
            vec![vec![(2048, 0), (2048, E), (0, E), (0, 0), (2048, 0),]],
            "the walk should pass both left-hand corners"
        );
        assert_closed(&out);
    }

    #[test]
    fn the_closed_ring_has_an_exterior_winding() {
        // The wrong sign would decode as a hole with no polygon around it.
        let band = chain(1, &[(2048, 0), (2048, E)]);
        let out = close_rings(&[band], EXTENT);
        assert!(
            area(&points_of(&out)[0]) > 0.0,
            "an exterior ring must have positive area"
        );
        assert_eq!(area(&points_of(&out)[0]), 2048.0 * f64::from(E));
    }

    #[test]
    fn a_corner_is_emitted_when_the_walk_passes_it() {
        // Top edge to left edge. Without the top-left corner the quadrant
        // would collapse to a triangle.
        let quadrant = chain(1, &[(2048, 0), (2048, 2048), (0, 2048)]);
        let out = close_rings(&[quadrant], EXTENT);
        assert_eq!(
            points_of(&out),
            vec![vec![(2048, 0), (2048, 2048), (0, 2048), (0, 0), (2048, 0),]]
        );
        assert_eq!(
            area(&points_of(&out)[0]),
            2048.0 * 2048.0,
            "the top-left quadrant"
        );
    }

    #[test]
    fn the_same_chain_reversed_closes_to_the_complementary_region() {
        // The chain's direction says which side is filled, so reversed it
        // gives the tile minus the quadrant, walking the long way round.
        let inverted = chain(1, &[(0, 2048), (2048, 2048), (2048, 0)]);
        let out = close_rings(&[inverted], EXTENT);
        assert_eq!(
            points_of(&out),
            vec![vec![
                (0, 2048),
                (2048, 2048),
                (2048, 0),
                (E, 0),
                (E, E),
                (0, E),
                (0, 2048),
            ]]
        );
        assert_eq!(
            area(&points_of(&out)[0]),
            (E as f64 * E as f64) - 2048.0 * 2048.0,
            "the tile minus the top-left quadrant"
        );
    }

    #[test]
    fn two_chains_of_one_feature_join_to_each_other() {
        // A band across the tile is two chains; each exit joins the other's
        // entry. Closing each onto itself would give two slivers.
        let top = chain(1, &[(0, 1000), (E, 1000)]);
        let bottom = chain(1, &[(E, 2000), (0, 2000)]);
        let out = close_rings(&[top, bottom], EXTENT);

        assert_eq!(out.len(), 1, "the two chains form one ring, not two");
        assert_closed(&out);
        assert_eq!(
            area(&points_of(&out)[0]),
            f64::from(E) * 1000.0,
            "the ring should be the 1000-unit band across the tile"
        );
    }

    #[test]
    fn chains_of_different_features_are_never_joined() {
        let a = chain(1, &[(0, 1000), (E, 1000)]);
        let b = chain(2, &[(E, 2000), (0, 2000)]);
        let out = close_rings(&[a, b], EXTENT);

        assert_eq!(out.len(), 2);
        assert_closed(&out);
        for ring in &out {
            assert_eq!(ring.points.len(), 5, "each closes onto itself via corners");
        }
        assert_eq!(out[0].feature, 1);
        assert_eq!(out[1].feature, 2);
    }

    #[test]
    fn a_hole_biting_the_edge_becomes_part_of_the_boundary() {
        // An anticlockwise hole crossing the left edge: the result is one ring,
        // the tile with a notch in it.
        let hole = chain(1, &[(0, 3072), (2048, 3072), (2048, 1024), (0, 1024)]);
        let out = close_rings(&[hole], EXTENT);

        assert_eq!(out.len(), 1);
        assert_closed(&out);
        // The tile minus a 2048 x 2048 notch.
        assert_eq!(
            area(&points_of(&out)[0]),
            (E as f64 * E as f64) - (2048.0 * 2048.0)
        );
    }

    #[test]
    fn two_separate_lobes_pair_the_right_entry_with_the_right_exit() {
        // Both lobes enter and leave by the left edge; each exit must pair with
        // its own lobe's entry.
        let upper = chain(1, &[(0, 500), (1000, 500), (1000, 1500), (0, 1500)]);
        let lower = chain(1, &[(0, 2500), (1000, 2500), (1000, 3500), (0, 3500)]);
        let out = close_rings(&[upper, lower], EXTENT);

        assert_eq!(out.len(), 2, "the lobes are separate rings");
        assert_closed(&out);
        let mut areas: Vec<f64> = points_of(&out).iter().map(|r| area(r)).collect();
        areas.sort_by(f64::total_cmp);
        assert_eq!(areas, vec![1_000_000.0, 1_000_000.0]);
    }

    #[test]
    fn a_chain_that_does_not_reach_the_boundary_is_kept_rather_than_dropped() {
        // An open chain ending inside the tile can't be closed.
        let stray = chain(1, &[(100, 100), (200, 200)]);
        let out = close_rings(std::slice::from_ref(&stray), EXTENT);
        assert_eq!(out, vec![stray]);
    }

    #[test]
    fn two_chains_meeting_at_one_boundary_point_join_there() {
        // Two teeth off the top edge sharing the point (1000, 0): one chain's
        // exit is the other's entry, at distance zero. Pins the tie-break, and
        // that the shared point isn't repeated.
        let right = chain(1, &[(2000, 0), (1500, 800), (1000, 0)]);
        let left = chain(1, &[(1000, 0), (500, 800), (0, 0)]);
        let out = close_rings(&[right, left], EXTENT);

        assert_eq!(out.len(), 1, "the pinch joins them into one ring");
        assert_closed(&out);
        assert_eq!(
            points_of(&out),
            vec![vec![
                (2000, 0),
                (1500, 800),
                (1000, 0),
                (500, 800),
                (0, 0),
                (2000, 0),
            ]],
            "the shared point should appear once, not twice"
        );
        assert_eq!(area(&points_of(&out)[0]), 800_000.0, "two 400000 teeth");
    }

    #[test]
    fn an_entry_at_the_exit_point_pairs_with_itself() {
        let lobe = chain(1, &[(1000, 0), (1500, 500), (2000, 0), (1000, 0)]);
        let out = close_rings(std::slice::from_ref(&lobe), EXTENT);
        assert_eq!(out, vec![lobe], "already closed, so nothing to walk");
    }
}
