//! Unions adjacent pieces of one polygon by cancelling their shared edges.
//!
//! Two abutting pieces each carry an artificial edge along the shared line,
//! traversed in opposite directions. Cancel those and the survivors bound the
//! union — exact on integers, no tolerances.
//!
//! Used by `lyv-merge` (four children, across two internal seams) and by
//! `lyv-buffer` (a tile and its margin, across the tile's four edges).
//!
//! Cancellation is a signed 1-D sweep per line rather than a hash lookup,
//! because shared edges can partly overlap: a real boundary running along a
//! seam for part of its length gives the two sides edges of different lengths.
//!
//! Assumes a coincident opposite pair can only be a seam. That holds because
//! `lyv-coverage` never emits two coincident squares of opposite winding
//! (`docs/coverage.py`); it isn't true of tile geometry in general.

use std::collections::{BTreeMap, HashMap};

use geo_types::Coord;
use lyv_core::Local;

pub use lyv_core::Chain;

/// Weld one tile's rings across the given lines: `vertical` are `x` values,
/// `horizontal` are `y` values. Edges elsewhere are untouched, which is most
/// of them.
///
/// Welds within a feature, never across features.
///
/// Returns raw closed rings; callers apply [`tidy`] when ready, since the merge
/// halves first and halving is what creates collinear points.
pub fn weld(chains: &[Chain], vertical: &[Local], horizontal: &[Local]) -> Vec<Chain> {
    // A map, not a scan per feature, which is quadratic in features per tile
    // and stalled zoom 0 on 368k parcels. Sorted so the output is reproducible.
    let mut grouped: BTreeMap<u64, Vec<&Chain>> = BTreeMap::new();
    for chain in chains {
        grouped.entry(chain.feature).or_default().push(chain);
    }

    let mut out = Vec::new();
    for (feature, group) in grouped {
        for points in weld_feature(&group, vertical, horizontal) {
            out.push(Chain { feature, points });
        }
    }
    out
}

/// A closed ring: at least four points, the last repeating the first.
pub fn is_ring(points: &[Coord<Local>]) -> bool {
    points.len() >= 4 && points.first() == points.last()
}

/// Doubled shoelace area. Positive is clockwise on screen, consistent with MVT.
pub fn double_area(ring: &[Coord<Local>]) -> i64 {
    ring.windows(2)
        .map(|w| i64::from(w[0].x) * i64::from(w[1].y) - i64::from(w[1].x) * i64::from(w[0].y))
        .sum()
}

/// Drop collinear vertices and reject what is no longer a ring.
///
/// Required, not tidiness: a merged full-tile square gains two points per side
/// at every level, on the tile boundary where simplification won't touch them.
/// Both sides of a shared edge drop the same points, so the shared-edge
/// invariant survives.
pub fn tidy(closed: Vec<Coord<Local>>) -> Option<Vec<Coord<Local>>> {
    tidy_keeping_winding(closed, None)
}

/// As [`tidy`], but also rejecting a ring whose winding differs from `was`,
/// its doubled area before the transform (for the merge, before halving).
///
/// Halving can't reverse an orientation; only rounding can, by folding the ring
/// through itself. Kept, the result would be a hole with nothing around it,
/// which renderers fill.
pub fn tidy_keeping_winding(
    closed: Vec<Coord<Local>>,
    was: Option<i64>,
) -> Option<Vec<Coord<Local>>> {
    let mut points = closed;
    points.dedup();
    if points.first() != points.last() {
        points.push(points[0]);
    }

    let cleaned = drop_collinear(&points);
    if cleaned.len() < 4 {
        return None;
    }
    let area = double_area(&cleaned);
    if area == 0 {
        // Can't be drawn, and would accumulate as a sliver.
        return None;
    }
    if let Some(was) = was
        && was.signum() != area.signum()
    {
        return None;
    }
    Some(cleaned)
}

fn weld_feature(
    rings: &[&Chain],
    vertical: &[Local],
    horizontal: &[Local],
) -> Vec<Vec<Coord<Local>>> {
    let mut runs: Vec<Vec<Coord<Local>>> = Vec::new();
    let mut closed: Vec<Vec<Coord<Local>>> = Vec::new();
    // Ordered, not hashed: `reassemble` walks `runs` in this order, so the
    // order decides how the edges partition into rings, and that is encoded.
    // A hash map made 6 in 5,148 buffered tiles differ between runs.
    let mut on_vertical: BTreeMap<Local, Vec<(Local, Local)>> = BTreeMap::new();
    let mut on_horizontal: BTreeMap<Local, Vec<(Local, Local)>> = BTreeMap::new();

    for ring in rings {
        split_ring(
            &ring.points,
            vertical,
            horizontal,
            &mut on_vertical,
            &mut on_horizontal,
            &mut runs,
            &mut closed,
        );
    }

    // Surviving seam segments become two-point runs, like everything else.
    for (line, intervals) in &on_vertical {
        for (from, to) in sweep(intervals) {
            runs.push(vec![Coord { x: *line, y: from }, Coord { x: *line, y: to }]);
        }
    }
    for (line, intervals) in &on_horizontal {
        for (from, to) in sweep(intervals) {
            runs.push(vec![Coord { x: from, y: *line }, Coord { x: to, y: *line }]);
        }
    }

    closed.extend(reassemble(runs));
    closed
}

/// Split a ring into its seam edges and the runs between them. A ring with no
/// seam edge goes straight to `closed`.
#[allow(clippy::too_many_arguments)]
fn split_ring(
    points: &[Coord<Local>],
    vertical: &[Local],
    horizontal: &[Local],
    on_vertical: &mut BTreeMap<Local, Vec<(Local, Local)>>,
    on_horizontal: &mut BTreeMap<Local, Vec<(Local, Local)>>,
    runs: &mut Vec<Vec<Coord<Local>>>,
    closed: &mut Vec<Vec<Coord<Local>>>,
) {
    let edges: Vec<(Coord<Local>, Coord<Local>)> =
        points.windows(2).map(|w| (w[0], w[1])).collect();

    let seam = |edge: (Coord<Local>, Coord<Local>)| seam_of(edge, vertical, horizontal);
    let Some(first_seam) = edges.iter().position(|&e| seam(e).is_some()) else {
        closed.push(points.to_vec());
        return;
    };

    // Start at a seam edge so that no run wraps around the end of the list.
    let mut run: Vec<Coord<Local>> = Vec::new();
    for step in 0..edges.len() {
        let edge = edges[(first_seam + step) % edges.len()];
        match seam(edge) {
            Some(Seam::Vertical(line)) => {
                flush(&mut run, runs);
                on_vertical
                    .entry(line)
                    .or_default()
                    .push((edge.0.y, edge.1.y));
            }
            Some(Seam::Horizontal(line)) => {
                flush(&mut run, runs);
                on_horizontal
                    .entry(line)
                    .or_default()
                    .push((edge.0.x, edge.1.x));
            }
            None => {
                if run.is_empty() {
                    run.push(edge.0);
                }
                run.push(edge.1);
            }
        }
    }
    flush(&mut run, runs);
}

fn flush(run: &mut Vec<Coord<Local>>, runs: &mut Vec<Vec<Coord<Local>>>) {
    if run.len() >= 2 {
        runs.push(std::mem::take(run));
    } else {
        run.clear();
    }
}

enum Seam {
    Vertical(Local),
    Horizontal(Local),
}

/// Which seam line this edge lies on, if any.
fn seam_of(
    edge: (Coord<Local>, Coord<Local>),
    vertical: &[Local],
    horizontal: &[Local],
) -> Option<Seam> {
    let (a, b) = edge;
    if a.x == b.x && vertical.contains(&a.x) {
        return Some(Seam::Vertical(a.x));
    }
    if a.y == b.y && horizontal.contains(&a.y) {
        return Some(Seam::Horizontal(a.y));
    }
    None
}

/// The signed 1-D sweep: what's left of directed, overlapping intervals on one
/// line. Each `(from, to)` weighs ±1 by direction; each elementary interval is
/// emitted at its net multiplicity, pointing the way the net does.
fn sweep(intervals: &[(Local, Local)]) -> Vec<(Local, Local)> {
    if intervals.is_empty() {
        return Vec::new();
    }

    // `(position, weight)`; the weight is a winding count, not a coordinate.
    let mut events: Vec<(Local, i32)> = Vec::new();
    for &(from, to) in intervals {
        let weight = if to > from { 1 } else { -1 };
        let (low, high) = (from.min(to), from.max(to));
        if low == high {
            continue; // a zero-length edge covers nothing
        }
        events.push((low, weight));
        events.push((high, -weight));
    }
    events.sort_unstable();

    let mut out = Vec::new();
    let mut running = 0i32;
    let mut index = 0usize;
    while index < events.len() {
        let position = events[index].0;
        while index < events.len() && events[index].0 == position {
            running += events[index].1;
            index += 1;
        }
        if running != 0 && index < events.len() {
            let next = events[index].0;
            for _ in 0..running.abs() {
                if running > 0 {
                    out.push((position, next));
                } else {
                    out.push((next, position));
                }
            }
        }
    }
    out
}

/// Walk directed runs back into closed rings.
///
/// Where a point has several outgoing runs the choice is arbitrary: it can
/// change how the edges partition into rings, never the total area.
fn reassemble(runs: Vec<Vec<Coord<Local>>>) -> Vec<Vec<Coord<Local>>> {
    let mut outgoing: HashMap<(Local, Local), Vec<usize>> = HashMap::new();
    for (index, run) in runs.iter().enumerate() {
        outgoing
            .entry((run[0].x, run[0].y))
            .or_default()
            .push(index);
    }

    let mut used = vec![false; runs.len()];
    let mut rings = Vec::new();

    for start in 0..runs.len() {
        if used[start] {
            continue;
        }
        let mut ring: Vec<Coord<Local>> = Vec::new();
        let mut current = start;
        loop {
            used[current] = true;
            let run = &runs[current];
            if ring.is_empty() {
                ring.extend_from_slice(run);
            } else {
                ring.extend_from_slice(&run[1..]);
            }

            let tail = *ring.last().expect("a run has at least two points");
            if tail == ring[0] {
                break; // closed
            }
            let Some(next) = outgoing
                .get(&(tail.x, tail.y))
                .and_then(|candidates| candidates.iter().find(|&&i| !used[i]).copied())
            else {
                break; // a dead end: the edge set was not a union of rings
            };
            current = next;
        }
        rings.push(ring);
    }
    rings
}

/// Drop every vertex strictly collinear with its neighbours, cyclically, in
/// one pass.
fn drop_collinear(closed: &[Coord<Local>]) -> Vec<Coord<Local>> {
    let points = &closed[..closed.len() - 1]; // drop the repeated closing point
    let n = points.len();
    if n < 3 {
        return Vec::new();
    }

    let mut kept: Vec<Coord<Local>> = Vec::new();
    for i in 0..n {
        let (a, b, c) = (points[(i + n - 1) % n], points[i], points[(i + 1) % n]);
        // Widen before subtracting: `i64::from(b.x - a.x)` would subtract in
        // `i16` and wrap silently in release builds.
        let (ax, ay) = (i64::from(a.x), i64::from(a.y));
        let (bx, by) = (i64::from(b.x), i64::from(b.y));
        let (cx, cy) = (i64::from(c.x), i64::from(c.y));
        let cross = (bx - ax) * (cy - ay) - (by - ay) * (cx - ax);
        if cross != 0 {
            kept.push(b);
        }
    }
    if kept.len() < 3 {
        return Vec::new();
    }
    kept.push(kept[0]);
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(feature: u64, points: &[(Local, Local)]) -> Chain {
        let mut pts: Vec<Coord<Local>> = points.iter().map(|&(x, y)| Coord { x, y }).collect();
        if pts.first() != pts.last() {
            pts.push(pts[0]);
        }
        Chain {
            feature,
            points: pts,
        }
    }

    fn areas(chains: &[Chain]) -> Vec<i64> {
        let mut out: Vec<i64> = chains.iter().map(|c| double_area(&c.points)).collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn two_squares_sharing_an_edge_become_one() {
        // Each has an artificial edge along x = 100, the opposite way round.
        let left = chain(1, &[(0, 0), (100, 0), (100, 100), (0, 100)]);
        let right = chain(1, &[(100, 0), (200, 0), (200, 100), (100, 100)]);

        let welded = weld(&[left, right], &[100], &[]);
        assert_eq!(welded.len(), 1, "got {welded:?}");

        let tidied = tidy(welded[0].points.clone()).expect("a real ring");
        assert_eq!(double_area(&tidied), 2 * 200 * 100);
        assert_eq!(tidied.len(), 5, "a rectangle, with no leftover midpoints");
        assert!(
            !tidied.iter().any(|p| p.x == 100 && p.y > 0 && p.y < 100),
            "the shared edge survived: {tidied:?}"
        );
    }

    #[test]
    fn a_shared_edge_is_only_cancelled_on_a_line_we_named() {
        let left = chain(1, &[(0, 0), (100, 0), (100, 100), (0, 100)]);
        let right = chain(1, &[(100, 0), (200, 0), (200, 100), (100, 100)]);

        // x = 100 is not offered as a seam, so nothing cancels.
        let welded = weld(&[left, right], &[999], &[]);
        assert_eq!(welded.len(), 2, "nothing should have been welded");
    }

    #[test]
    fn different_features_are_never_welded() {
        let left = chain(1, &[(0, 0), (100, 0), (100, 100), (0, 100)]);
        let right = chain(2, &[(100, 0), (200, 0), (200, 100), (100, 100)]);

        let welded = weld(&[left, right], &[100], &[]);
        assert_eq!(welded.len(), 2, "two features abutting is not one feature");
        assert_eq!(areas(&welded), vec![2 * 100 * 100, 2 * 100 * 100]);
    }

    #[test]
    fn partly_overlapping_shared_edges_cancel_only_where_they_overlap() {
        // The right piece shares only the lower half of the edge, so the upper
        // half must survive.
        let left = chain(1, &[(0, 0), (100, 0), (100, 100), (0, 100)]);
        let right = chain(1, &[(100, 0), (200, 0), (200, 50), (100, 50)]);

        let welded = weld(&[left, right], &[100], &[]);
        let total: i64 = welded.iter().map(|c| double_area(&c.points)).sum();
        assert_eq!(total, 2 * (100 * 100 + 100 * 50), "area changed");

        let survives: bool = welded.iter().any(|c| {
            c.points
                .windows(2)
                .any(|w| w[0].x == 100 && w[1].x == 100 && w[0].y.max(w[1].y) > 50)
        });
        assert!(survives, "the unshared upper half of the edge was lost");
    }

    #[test]
    fn welding_across_four_lines_at_once_joins_a_three_by_three_block() {
        // What the buffer does: a tile and its eight neighbours.
        let mut pieces = Vec::new();
        for row in -1..=1 {
            for column in -1..=1 {
                let (x, y) = (column * 100, row * 100);
                pieces.push(chain(
                    1,
                    &[(x, y), (x + 100, y), (x + 100, y + 100), (x, y + 100)],
                ));
            }
        }
        let welded = weld(&pieces, &[0, 100], &[0, 100]);
        assert_eq!(welded.len(), 1, "got {} rings", welded.len());

        let tidied = tidy(welded[0].points.clone()).expect("a real ring");
        assert_eq!(double_area(&tidied), 2 * 300 * 300);
        assert_eq!(tidied.len(), 5, "one square, no leftover midpoints");
    }

    #[test]
    fn a_hole_straddling_a_seam_comes_back_as_a_hole() {
        // Each side folded the hole into its exterior when clipped.
        let left = chain(
            1,
            &[
                (0, 0),
                (100, 0),
                (100, 30),
                (50, 30),
                (50, 70),
                (100, 70),
                (100, 100),
                (0, 100),
            ],
        );
        let right = chain(
            1,
            &[
                (100, 0),
                (200, 0),
                (200, 100),
                (100, 100),
                (100, 70),
                (150, 70),
                (150, 30),
                (100, 30),
            ],
        );

        let welded = weld(&[left, right], &[100], &[]);
        let signed: Vec<i64> = welded.iter().map(|c| double_area(&c.points)).collect();
        assert_eq!(welded.len(), 2, "an exterior and a hole, got {signed:?}");
        assert!(signed.iter().any(|a| *a > 0), "no exterior");
        assert!(signed.iter().any(|a| *a < 0), "the hole did not come back");
        assert_eq!(
            signed.iter().sum::<i64>(),
            2 * (200 * 100 - 100 * 40),
            "the hole should subtract"
        );
    }

    #[test]
    fn a_ring_touching_no_seam_passes_through_untouched() {
        let island = chain(1, &[(10, 10), (20, 10), (20, 20), (10, 20)]);
        let welded = weld(std::slice::from_ref(&island), &[100], &[100]);
        assert_eq!(welded, vec![island]);
    }

    #[test]
    fn reassembly_closes_a_ring_rather_than_welding_two_that_share_a_start() {
        // Two loops starting at the same point must close separately rather
        // than join into a figure-eight.
        let shared = Coord { x: 100, y: 100 };
        let runs = vec![
            vec![shared, Coord { x: 200, y: 100 }, Coord { x: 200, y: 200 }],
            vec![Coord { x: 200, y: 200 }, shared],
            vec![shared, Coord { x: 0, y: 100 }, Coord { x: 0, y: 0 }],
            vec![Coord { x: 0, y: 0 }, shared],
        ];

        let rings = reassemble(runs);
        assert_eq!(rings.len(), 2, "got {rings:?}");
        for ring in &rings {
            assert_eq!(ring.first(), ring.last(), "each loop should close");
            assert_eq!(ring.len(), 4, "and hold only its own points");
        }
    }

    #[test]
    fn a_self_intersecting_ring_with_no_net_area_is_dropped() {
        // A bowtie: nothing collinear, so only the area test catches that its
        // halves cancel.
        let bowtie = vec![
            Coord { x: 0, y: 0 },
            Coord { x: 20, y: 0 },
            Coord { x: 0, y: 20 },
            Coord { x: 20, y: 20 },
            Coord { x: 0, y: 0 },
        ];
        assert_eq!(double_area(&bowtie), 0, "the fixture should net to nothing");
        assert_eq!(
            drop_collinear(&bowtie).len(),
            5,
            "the fixture should survive the collinear pass, or it proves \
             nothing about the area test"
        );

        assert!(
            tidy(bowtie).is_none(),
            "a zero-area ring should not reach the output"
        );
    }

    #[test]
    fn tidy_rejects_a_ring_that_rounded_itself_inside_out() {
        // Positive before the transform, negative after: it collapsed.
        let sliver = vec![
            Coord { x: 0, y: 0 },
            Coord { x: 10, y: 4 },
            Coord { x: 3, y: 1 },
            Coord { x: 0, y: 0 },
        ];
        let flipped = double_area(&sliver);
        assert!(flipped < 0, "the fixture should be negatively wound");

        // Offered as though it had been positive before the transform.
        assert_eq!(tidy_keeping_winding(sliver.clone(), Some(100)), None);
        // And kept when the sign agrees.
        assert!(tidy_keeping_winding(sliver, Some(-100)).is_some());
    }

    #[test]
    fn tidy_without_a_previous_winding_keeps_either_sign() {
        // No transform to compare against, so a hole stays a hole.
        let hole = vec![
            Coord { x: 0, y: 0 },
            Coord { x: 0, y: 10 },
            Coord { x: 10, y: 10 },
            Coord { x: 10, y: 0 },
            Coord { x: 0, y: 0 },
        ];
        assert!(double_area(&hole) < 0);
        assert!(tidy(hole).is_some());
    }

    #[test]
    fn tidy_rejects_a_ring_with_no_area() {
        let flat = vec![
            Coord { x: 0, y: 0 },
            Coord { x: 10, y: 0 },
            Coord { x: 20, y: 0 },
            Coord { x: 0, y: 0 },
        ];
        assert_eq!(tidy(flat), None);
    }
}
