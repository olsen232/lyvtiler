//! Removes vertices a zoom can't express.
//!
//! Halving coordinates doesn't reduce the vertex count, so without this a
//! parent keeps four times a child's vertices; with it, about twice.
//!
//! **Never remove a vertex on the tile boundary.** Neighbouring tiles hold
//! bit-identical geometry along their shared edge, and the merge relies on it.
//! Both tiles apply the rule to the same vertices, so it needs no coordination.
//! It's implemented by splitting each chain at its boundary vertices and
//! simplifying each run separately, since every algorithm keeps the ends of
//! what it's given. A seam vertex becomes interior after the merge and is
//! simplified then.
//!
//! The tolerance is in tile units, the same number at every zoom, which scales
//! with the ground automatically. Douglas-Peucker's is a distance, Visvalingam's
//! an area; they don't convert (a vertex `d` from a chord `b` has area `b·d/2`),
//! so [`Tolerance`] says which it is.

use geo::{Simplify, SimplifyVw};
use geo_types::{Coord, LineString};
use lyv_core::{Chain, Local};

/// An integer coordinate a run can be simplified in: `Local` inside a tile,
/// `i64` on the world grid before the cut. The `f64` round trip is exact for
/// both; the deepest grid is `8192 << 22` units, far inside 2^53.
trait Unit: geo_types::CoordNum + Ord {
    fn to_f64(self) -> f64;
    fn from_f64(value: f64) -> Self;
}

impl Unit for Local {
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn from_f64(value: f64) -> Self {
        value as Self
    }
}

impl Unit for i64 {
    fn to_f64(self) -> f64 {
        self as f64
    }
    fn from_f64(value: f64) -> Self {
        value as Self
    }
}

/// How much detail to discard, in tile units, in the unit of its algorithm.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tolerance {
    /// Remove a vertex closer than this to the chord (Douglas-Peucker).
    Distance(f64),
    /// Remove a vertex whose triangle is smaller than this, in square tile
    /// units (Visvalingam).
    Area(f64),
}

impl Tolerance {
    /// Nothing is removed except zero-length segments.
    pub const NONE: Self = Self::Distance(0.0);

    /// Whether this discards nothing but zero-length segments. Either unit can
    /// express that, so compare with this rather than [`Tolerance::NONE`].
    pub fn is_none(self) -> bool {
        match self {
            Self::Distance(d) => d <= 0.0,
            Self::Area(a) => a <= 0.0,
        }
    }

    /// The tolerance `levels` zooms further up would apply, in this level's
    /// units: how a level is simplified a zoom ahead to cut its vertex count
    /// without dropping features.
    ///
    /// Merging halves coordinates, so distance doubles and area quadruples.
    pub fn coarsened(self, levels: u32) -> Self {
        // An exponent for `powi`, not a coordinate.
        let levels = levels as i32;
        match self {
            Self::Distance(d) => Self::Distance(d * 2f64.powi(levels)),
            Self::Area(a) => Self::Area(a * 4f64.powi(levels)),
        }
    }

    /// This tolerance as a fraction of the whole world at `scale`: the only way
    /// to compare tolerances taken at different zooms or extents. Only against
    /// a tolerance of the same kind.
    fn across_the_world(self, scale: Scale) -> f64 {
        // An exponent for `powi`, not a coordinate.
        let halvings = scale.get() as i32;
        match self {
            Self::Distance(d) => d / 2f64.powi(halvings),
            Self::Area(a) => a / 4f64.powi(halvings),
        }
    }
}

/// How fine a grid is, as a power of two: `zoom + log2(extent)`, the halvings
/// from the whole world to one grid unit. Extent 8192 at z16 is scale 29.
///
/// Levels at different zooms and extents compare directly: the ranking's
/// `z + k` with `extent >> k` is the same scale. Extent must be a power of two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Scale(u32);

impl Scale {
    /// The scale of a level at `zoom` whose tiles have extent `extent`.
    pub fn new(zoom: u32, extent: u32) -> Self {
        Self(zoom + extent.trailing_zeros())
    }

    /// The power of two itself.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// What simplification a level has already had, so a stage can tell whether
/// its own pass would add anything.
///
/// Running a pass twice isn't harmless: Douglas-Peucker isn't nested, so a
/// second pass keeps *more* vertices than one would.
///
/// A tolerance alone can't answer, because every level applies the same number
/// in its own units. Paired with its [`Scale`], a pass is redundant only when
/// what was already discarded is at least as coarse, as a fraction of the
/// world. So the climb (one scale coarser per level) and `--maxzoom-extent`
/// (two) both come out right.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Simplified {
    tolerance: Tolerance,
    scale: Scale,
}

impl Simplified {
    /// Simplified at `tolerance`, on a grid of scale `scale`.
    pub fn at(tolerance: Tolerance, scale: Scale) -> Self {
        Self { tolerance, scale }
    }

    /// The tolerance to actually apply: `tolerance`, or [`Tolerance::NONE`] if a
    /// pass at least as coarse has already run. Both must be the same kind.
    ///
    /// `NONE` rather than skipping, because zero-length segments still need
    /// collapsing: the cut can put two crossings on one integer.
    pub fn still_wants(self, tolerance: Tolerance, scale: Scale) -> Tolerance {
        debug_assert_eq!(
            std::mem::discriminant(&self.tolerance),
            std::mem::discriminant(&tolerance),
            "a distance and an area don't compare"
        );
        if self.tolerance.across_the_world(self.scale) >= tolerance.across_the_world(scale) {
            Tolerance::NONE
        } else {
            tolerance
        }
    }
}

/// Simplify a tile's chains, keeping every vertex on the tile boundary.
///
/// A run leaves as a run or not at all, and a point unchanged: a run
/// shortened to one point would turn a line into a point. Deduplication after
/// halving can collapse a run onto one coordinate, so it's enforced in
/// `simplify_points`.
pub fn simplify_chains(chains: &[Chain], extent: u32, tolerance: Tolerance) -> Vec<Chain> {
    chains
        .iter()
        .filter_map(|chain| {
            let points = simplify_points(&chain.points, extent as Local, tolerance);
            (!points.is_empty()).then_some(Chain {
                feature: chain.feature,
                points,
            })
        })
        .collect()
}

/// Simplify one part of a feature on the world grid, before it's cut.
///
/// Once per feature rather than once per clipped piece, and the discarded
/// vertices are never allocated — 61% of them on floodplains. No boundary
/// protection is needed: there are no tiles yet, and both sides of any later
/// seam are clipped from the same simplified line.
///
/// A part that would drop below its minimum (two points, or four for a ring)
/// is returned unchanged; it's dust, and `drop_collapsed` deals with it later.
pub fn simplify_part(points: &[Coord<i64>], tolerance: Tolerance, closed: bool) -> Vec<Coord<i64>> {
    if points.len() < 2 {
        return points.to_vec();
    }

    // Zero-length segments first. Here they're common: a source finer than the
    // grid puts consecutive vertices on one integer.
    let mut deduplicated: Vec<Coord<i64>> = Vec::with_capacity(points.len());
    for point in points {
        if deduplicated.last() != Some(point) {
            deduplicated.push(*point);
        }
    }

    let simplified = simplify_run(&deduplicated, tolerance);
    // A ring stays closed: both ends are the same point, and dedup and every
    // algorithm keep them.
    let floor = if closed { 4 } else { 2 };
    if simplified.len() < floor {
        return points.to_vec();
    }
    simplified
}

fn simplify_points(
    points: &[Coord<Local>],
    extent: Local,
    tolerance: Tolerance,
) -> Vec<Coord<Local>> {
    if points.len() == 1 {
        return points.to_vec();
    }

    // Zero-length segments first; halving puts crossings a unit apart onto one
    // point.
    let mut deduplicated: Vec<Coord<Local>> = Vec::with_capacity(points.len());
    for point in points {
        if deduplicated.last() != Some(point) {
            deduplicated.push(*point);
        }
    }
    // A run collapsed onto one coordinate is gone, not a point.
    if deduplicated.len() < 2 {
        return Vec::new();
    }
    if deduplicated.len() < 3 {
        return deduplicated;
    }

    // Split at interior boundary vertices; the ends are kept anyway.
    let mut out: Vec<Coord<Local>> = Vec::new();
    let mut run: Vec<Coord<Local>> = vec![deduplicated[0]];

    for (index, point) in deduplicated.iter().enumerate().skip(1) {
        run.push(*point);
        let is_last = index + 1 == deduplicated.len();
        if !is_last && on_boundary(*point, extent) {
            extend(&mut out, simplify_run(&run, tolerance));
            run = vec![*point];
        }
    }
    extend(&mut out, simplify_run(&run, tolerance));
    out
}

/// Append a run, dropping the point it shares with what is already there.
fn extend(out: &mut Vec<Coord<Local>>, run: Vec<Coord<Local>>) {
    let mut run = run.into_iter();
    if out.is_empty() {
        out.extend(run);
    } else {
        run.next();
        out.extend(run);
    }
}

fn simplify_run<T: Unit>(run: &[Coord<T>], tolerance: Tolerance) -> Vec<Coord<T>> {
    let threshold = match tolerance {
        Tolerance::Distance(d) => d,
        Tolerance::Area(a) => a,
    };
    if run.len() < 3 || threshold <= 0.0 {
        return run.to_vec();
    }

    // Simplify in a canonical direction, then restore it. Both algorithms
    // break ties by iteration order, and exact ties are common on integers,
    // so reversing the input can change the result. Two features sharing a
    // border traverse it in opposite directions, so without this the border
    // simplifies two ways and a gap opens.
    let reversed: Vec<Coord<T>> = run.iter().rev().copied().collect();
    let flip = key(&reversed) < key(run);
    let oriented = if flip { &reversed[..] } else { run };

    let simplified = simplify_oriented(oriented, tolerance);
    if flip {
        simplified.into_iter().rev().collect()
    } else {
        simplified
    }
}

/// A run and its reverse are the same arc; whichever reads smaller is
/// canonical.
fn key<T: Unit>(run: &[Coord<T>]) -> Vec<(T, T)> {
    run.iter().map(|p| (p.x, p.y)).collect()
}

fn simplify_oriented<T: Unit>(run: &[Coord<T>], tolerance: Tolerance) -> Vec<Coord<T>> {
    let line = LineString::from(
        run.iter()
            .map(|p| (p.x.to_f64(), p.y.to_f64()))
            .collect::<Vec<_>>(),
    );

    let simplified = match tolerance {
        Tolerance::Distance(d) => line.simplify(d),
        Tolerance::Area(a) => line.simplify_vw(a),
    };

    simplified
        .0
        .iter()
        // Exact: the input was integral and no vertex moves.
        .map(|c| Coord {
            x: T::from_f64(c.x),
            y: T::from_f64(c.y),
        })
        .collect()
}

fn on_boundary(point: Coord<Local>, extent: Local) -> bool {
    point.x == 0 || point.y == 0 || point.x == extent || point.y == extent
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXTENT: u32 = 4096;
    const E: Local = EXTENT as Local;

    fn chain(points: &[(Local, Local)]) -> Chain {
        Chain {
            feature: 1,
            points: points.iter().map(|&(x, y)| Coord { x, y }).collect(),
        }
    }

    fn points_of(chains: &[Chain]) -> Vec<(Local, Local)> {
        chains
            .iter()
            .flat_map(|c| c.points.iter().map(|p| (p.x, p.y)))
            .collect()
    }

    fn simplify(chains: &[Chain], distance: f64) -> Vec<Chain> {
        simplify_chains(chains, EXTENT, Tolerance::Distance(distance))
    }

    #[test]
    fn coarsening_by_a_level_reaches_as_far_as_the_parent_will() {
        // A sawtooth deviating 10 from its chord, and the same after a merge
        // halved it (5). Even coordinates so halving is exact, and clear of the
        // tile edges so nothing is protected.
        let child = chain(&[
            (1000, 1000),
            (1100, 1010),
            (1200, 1000),
            (1300, 1010),
            (1400, 1000),
        ]);
        let parent = chain(&[(500, 500), (550, 505), (600, 500), (650, 505), (700, 500)]);

        assert_eq!(points_of(&simplify(&[parent], 6.0)).len(), 2);
        // The child, at the same number, doesn't: its detail is twice as large.
        assert_eq!(
            points_of(&simplify(std::slice::from_ref(&child), 6.0)).len(),
            5
        );
        // Coarsened by one level, it is.
        let ahead = Tolerance::Distance(6.0).coarsened(1);
        assert_eq!(
            points_of(&simplify_chains(&[child], EXTENT, ahead)).len(),
            2
        );
    }

    #[test]
    fn an_area_tolerance_coarsens_four_times_as_fast_as_a_distance_one() {
        // Both are the same halving; an area is two lengths multiplied.
        assert_eq!(
            Tolerance::Distance(1.5).coarsened(2),
            Tolerance::Distance(6.0)
        );
        assert_eq!(Tolerance::Area(1.5).coarsened(2), Tolerance::Area(24.0));
        // And no lead at all changes nothing.
        assert_eq!(Tolerance::Area(1.5).coarsened(0), Tolerance::Area(1.5));
    }

    #[test]
    fn a_collinear_interior_point_is_removed() {
        // A seam crossing is exactly collinear, so it goes first.
        let input = vec![chain(&[(100, 100), (200, 200), (300, 300)])];
        let out = simplify(&input, 1.0);
        assert_eq!(points_of(&out), vec![(100, 100), (300, 300)]);
    }

    #[test]
    fn a_point_off_the_chord_by_more_than_the_tolerance_stays() {
        let input = vec![chain(&[(0, 0), (100, 50), (200, 0)])];
        let out = simplify(&input, 1.0);
        assert_eq!(out[0].points.len(), 3, "50 units off the chord must stay");
    }

    #[test]
    fn the_ends_of_a_chain_are_never_removed() {
        let input = vec![chain(&[(100, 100), (150, 150), (200, 200)])];
        let out = simplify(&input, 10_000.0);
        assert_eq!(points_of(&out), vec![(100, 100), (200, 200)]);
    }

    #[test]
    fn a_vertex_on_the_tile_boundary_is_never_removed() {
        // The middle point is on the boundary and exactly collinear, so an
        // unprotected simplifier would remove it.
        let input = vec![chain(&[(100, 0), (E, 0), (E, 100)])];
        let out = simplify(&input, 10_000.0);
        assert!(
            points_of(&out).contains(&(E, 0)),
            "the boundary vertex was removed; got {:?}",
            points_of(&out)
        );
    }

    #[test]
    fn two_tiles_still_agree_about_their_shared_edge() {
        // Two tiles meet at x = E and x = 0. The approach is nearly collinear on
        // one side and sharp on the other, so unprotected, one tile would drop
        // the point and the other keep it, and the merge wouldn't rejoin them.
        let meeting_y = 1234;
        let left = vec![chain(&[(0, 1200), (2000, 1220), (E, meeting_y)])];
        let right = vec![chain(&[(0, meeting_y), (50, 3000), (E, 3900)])];

        let left_out = simplify(&left, 10_000.0);
        let right_out = simplify(&right, 10_000.0);

        assert_eq!(
            left_out[0].points.last(),
            Some(&Coord { x: E, y: meeting_y }),
            "the left tile must still end on its right edge at the same point"
        );
        assert_eq!(
            right_out[0].points.first(),
            Some(&Coord { x: 0, y: meeting_y }),
            "and the right tile must still start there"
        );
    }

    #[test]
    fn a_boundary_vertex_in_the_middle_of_a_chain_splits_the_run() {
        // Touches the boundary and comes back: the rest goes, the boundary
        // vertex stays.
        let input = vec![chain(&[
            (500, 500),
            (250, 250),
            (0, 0),
            (250, 250),
            (500, 500),
        ])];
        let out = simplify(&input, 10_000.0);
        assert!(
            points_of(&out).contains(&(0, 0)),
            "got {:?}",
            points_of(&out)
        );
    }

    #[test]
    fn coincident_points_are_removed_even_at_zero_tolerance() {
        // Crossings a unit apart become one point after halving.
        let input = vec![chain(&[(10, 10), (20, 20), (20, 20), (30, 40)])];
        let out = simplify(&input, 0.0);
        assert_eq!(points_of(&out), vec![(10, 10), (20, 20), (30, 40)]);
    }

    #[test]
    fn a_zero_tolerance_keeps_everything_else() {
        let input = vec![chain(&[(0, 0), (10, 3), (20, 0), (30, 7)])];
        let out = simplify(&input, 0.0);
        assert_eq!(out[0].points.len(), 4);
    }

    #[test]
    fn a_chain_reduced_below_two_points_is_dropped() {
        // Every point coincident, so nothing survives deduplication.
        let input = vec![chain(&[(5, 5), (5, 5), (5, 5)])];
        assert_eq!(simplify(&input, 0.0), Vec::new());
    }

    #[test]
    fn feature_ids_survive() {
        let input = vec![
            Chain {
                feature: 7,
                points: vec![
                    Coord { x: 0, y: 0 },
                    Coord { x: 50, y: 50 },
                    Coord { x: 100, y: 100 },
                ],
            },
            Chain {
                feature: 9,
                points: vec![
                    Coord { x: 200, y: 0 },
                    Coord { x: 250, y: 50 },
                    Coord { x: 300, y: 100 },
                ],
            },
        ];
        let ids: Vec<u64> = simplify(&input, 1.0).iter().map(|c| c.feature).collect();
        assert_eq!(ids, vec![7, 9]);
    }

    #[test]
    fn the_two_criteria_are_genuinely_different_units() {
        // A point 3 off a 200-unit chord has triangle area 300: distance 4
        // removes it, area 4 doesn't.
        let input = vec![chain(&[(0, 0), (100, 3), (200, 0)])];

        assert_eq!(
            simplify_chains(&input, EXTENT, Tolerance::Distance(4.0))[0]
                .points
                .len(),
            2,
            "4 units of distance should remove a point 3 units off the chord"
        );
        assert_eq!(
            simplify_chains(&input, EXTENT, Tolerance::Area(4.0))[0]
                .points
                .len(),
            3,
            "4 square units of area should not: the triangle is 300"
        );
        assert_eq!(
            simplify_chains(&input, EXTENT, Tolerance::Area(400.0))[0]
                .points
                .len(),
            2,
            "400 square units should, since the triangle is 300"
        );
    }

    #[test]
    fn both_algorithms_keep_a_point_well_off_the_chord() {
        let input = vec![chain(&[(0, 0), (100, 60), (200, 0)])];
        for tolerance in [Tolerance::Distance(4.0), Tolerance::Area(4.0)] {
            let out = simplify_chains(&input, EXTENT, tolerance);
            assert_eq!(out[0].points.len(), 3, "{tolerance:?} over-simplified");
        }
    }

    #[test]
    fn a_run_simplifies_the_same_in_either_direction() {
        // Two features sharing a border traverse it in opposite directions, so
        // an arc must simplify the same either way. Before canonicalising, 2 to
        // 21 vertices in 81,000 differed.
        let mut seed = 0xBADA55E1FBABEu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for case in 0..3000 {
            let (mut x, mut y) = (600, 600);
            let points: Vec<Coord<Local>> = (0..8 + (next() % 25) as usize)
                .map(|_| {
                    x += (next() % 160) as Local - 60;
                    y += (next() % 160) as Local - 60;
                    Coord { x, y }
                })
                .collect();

            let forwards = Chain {
                feature: 1,
                points: points.clone(),
            };
            let backwards = Chain {
                feature: 1,
                points: points.iter().rev().copied().collect(),
            };

            for tolerance in [
                Tolerance::Distance(4.0),
                Tolerance::Distance(64.0),
                Tolerance::Area(500.0),
                Tolerance::Area(50_000.0),
            ] {
                let f = &simplify_chains(std::slice::from_ref(&forwards), EXTENT, tolerance)[0];
                let b = &simplify_chains(std::slice::from_ref(&backwards), EXTENT, tolerance)[0];
                let b_forwards: Vec<Coord<Local>> = b.points.iter().rev().copied().collect();
                assert_eq!(
                    f.points, b_forwards,
                    "case {case} {tolerance:?}: direction changed the result"
                );
            }
        }
    }

    #[test]
    fn every_boundary_vertex_survives_any_input() {
        // The invariant, as a property rather than a case.
        let mut seed = 0xBADA55E1FBABEu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for case in 0..2000 {
            // Some coordinates on the boundary, so they need protecting.
            let points: Vec<Coord<Local>> = (0..2 + (next() % 10) as usize)
                .map(|_| {
                    let snap = next() % 4;
                    let (x, y) = (
                        (next() % EXTENT as u64) as Local,
                        (next() % EXTENT as u64) as Local,
                    );
                    match snap {
                        0 => Coord { x: 0, y },
                        1 => Coord { x: E, y },
                        2 => Coord { x, y: 0 },
                        _ => Coord { x, y },
                    }
                })
                .collect();
            let input = vec![Chain {
                feature: 1,
                points: points.clone(),
            }];

            for tolerance in [Tolerance::Distance(500.0), Tolerance::Area(250_000.0)] {
                let out = simplify_chains(&input, EXTENT, tolerance);
                let kept = points_of(&out);
                for point in &points {
                    let on_boundary = point.x == 0 || point.y == 0 || point.x == E || point.y == E;
                    if on_boundary {
                        assert!(
                            kept.contains(&(point.x, point.y)),
                            "case {case} {tolerance:?}: lost boundary vertex {point:?}"
                        );
                    }
                }
            }
        }
    }

    fn part(points: &[(i64, i64)]) -> Vec<Coord<i64>> {
        points.iter().map(|&(x, y)| Coord { x, y }).collect()
    }

    fn back(points: &[Coord<i64>]) -> Vec<(i64, i64)> {
        points.iter().map(|p| (p.x, p.y)).collect()
    }

    #[test]
    fn a_part_has_no_boundary_vertices_to_protect() {
        // A one-unit detour: protected as a chain with its middle vertex on
        // the tile edge, removed mid-tile, and removed as a part (no tiles
        // yet).
        let points: [(Local, Local); 3] = [(-30, 0), (0, 1), (30, 0)];
        let as_chain = |offset: Local| chain(&points.map(|(x, y)| (x + offset, y)));
        let as_part = part(&points.map(|(x, y)| (i64::from(x), i64::from(y))));

        let protected = simplify(&[as_chain(0)], 10.0);
        assert_eq!(points_of(&protected).len(), 3, "the boundary vertex stays");

        let interior = simplify(&[as_chain(E / 2)], 10.0);
        assert_eq!(points_of(&interior).len(), 2, "nothing protects this one");

        let uncut = simplify_part(&as_part, Tolerance::Distance(10.0), false);
        assert_eq!(back(&uncut), vec![(-30, 0), (30, 0)], "nothing to protect");
    }

    #[test]
    fn repeated_vertices_go_whatever_the_tolerance() {
        // No tolerance needed to see two vertices on one integer are one.
        let repeated = part(&[(0, 0), (0, 0), (0, 0), (50, 80), (50, 80), (900, 30)]);
        let out = simplify_part(&repeated, Tolerance::NONE, false);
        assert_eq!(back(&out), vec![(0, 0), (50, 80), (900, 30)]);
    }

    #[test]
    fn a_run_that_would_collapse_is_handed_back_as_it_came() {
        // All on one coordinate. `simplify_points` would return nothing; as a
        // part it comes back unchanged for `drop_collapsed` to handle.
        let dust = part(&[(7, 7), (7, 7), (7, 7)]);
        let out = simplify_part(&dust, Tolerance::Distance(1.0), false);
        assert_eq!(back(&out), vec![(7, 7), (7, 7), (7, 7)]);
    }

    #[test]
    fn a_ring_stays_closed_and_never_drops_below_four() {
        let ring = part(&[(0, 0), (1000, 2), (2000, 0), (1000, 900), (0, 0)]);
        let out = simplify_part(&ring, Tolerance::Distance(10.0), true);
        assert_eq!(
            back(&out),
            vec![(0, 0), (2000, 0), (1000, 900), (0, 0)],
            "the near-collinear vertex goes, the closure stays"
        );

        // Simplified, the sliver isn't a ring, so the original comes back.
        let sliver = part(&[(0, 0), (500, 1), (1000, 0), (0, 0)]);
        let out = simplify_part(&sliver, Tolerance::Distance(10.0), true);
        assert_eq!(back(&out), back(&sliver));
    }

    #[test]
    fn a_point_is_untouched() {
        let one = part(&[(12, 34)]);
        assert_eq!(
            back(&simplify_part(&one, Tolerance::Distance(100.0), false)),
            vec![(12, 34)]
        );
    }

    #[test]
    fn a_part_simplifies_the_same_in_either_direction() {
        // Canonical direction, as inside a tile: shared borders run opposite
        // ways.
        let forward = part(&[(0, 0), (100, 9), (200, 4), (300, 9), (400, 0)]);
        let backward: Vec<Coord<i64>> = forward.iter().rev().copied().collect();
        for tolerance in [Tolerance::Distance(6.0), Tolerance::Area(500.0)] {
            let out = simplify_part(&forward, tolerance, false);
            let other = simplify_part(&backward, tolerance, false);
            let other: Vec<Coord<i64>> = other.into_iter().rev().collect();
            assert_eq!(back(&out), back(&other), "{tolerance:?}");
        }
    }

    #[test]
    fn a_part_survives_coordinates_too_large_for_an_i32() {
        // The world grid at the deepest zoom, too big for `Local`.
        let far = 8192i64 << 22;
        let points = part(&[(far, far), (far + 100, far + 3), (far + 200, far)]);
        let out = simplify_part(&points, Tolerance::Distance(10.0), false);
        assert_eq!(back(&out), vec![(far, far), (far + 200, far)]);
    }

    #[test]
    fn a_scale_is_the_zoom_plus_the_log_of_the_extent() {
        assert_eq!(Scale::new(0, 4096).get(), 12, "the world is 4096 across");
        assert_eq!(Scale::new(1, 4096).get(), 13);
        assert_eq!(Scale::new(16, 8192).get(), 29);
        // Different names, same grid, equal scales.
        assert_eq!(Scale::new(12, 8192), Scale::new(13, 4096));
    }

    #[test]
    fn a_pass_already_run_is_asked_for_only_its_residue() {
        let t = Tolerance::Distance(1.0);
        let z12 = Scale::new(12, 8192);
        assert_eq!(
            Simplified::at(t, z12).still_wants(t, z12),
            Tolerance::NONE,
            "the same pass on the same grid"
        );
    }

    #[test]
    fn the_climb_is_never_told_its_pass_is_done() {
        // Every level applies the same number, so comparing tolerances alone
        // would call the whole pyramid simplified.
        let t = Tolerance::Distance(1.0);
        let maxzoom = Scale::new(12, 8192);
        let done = Simplified::at(t, maxzoom);

        // z11 at the ordinary extent is two scales coarser than maxzoom with
        // `--maxzoom-extent 8192`.
        for (zoom, extent) in [(11, 4096), (10, 4096), (9, 4096), (0, 4096)] {
            let scale = Scale::new(zoom, extent);
            assert!(scale < maxzoom, "z{zoom} is coarser than maxzoom");
            assert_eq!(
                done.still_wants(t, scale),
                t,
                "z{zoom} still wants the pass"
            );
        }
    }

    #[test]
    fn a_pass_run_on_a_finer_grid_than_is_asked_for_is_not_redundant() {
        // Skipping maxzoom's pass is safe because the cut and maxzoom share a
        // grid; this notices if they stop.
        let t = Tolerance::Distance(1.0);

        // As things are: same grid, so the second pass has nothing to add.
        let together = Simplified::at(t, Scale::new(12, 8192));
        assert_eq!(
            together.still_wants(t, Scale::new(12, 8192)),
            Tolerance::NONE
        );

        // If the cut quantized finer than maxzoom, it would have discarded less
        // than maxzoom wants, so the pass must run.
        let cut_finer = Simplified::at(t, Scale::new(12, 16384));
        assert_eq!(cut_finer.still_wants(t, Scale::new(12, 8192)), t);
    }

    #[test]
    fn simplifying_ahead_covers_the_level_it_was_run_ahead_for() {
        // The climb's lead: a level simplified at its parent's reach, at its own
        // scale.
        let a = Tolerance::Area(4.0);
        let child = Scale::new(12, 4096);
        let parent = Scale::new(11, 4096);

        let ahead = Simplified::at(a.coarsened(1), child);
        assert_eq!(
            ahead.still_wants(a, parent),
            Tolerance::NONE,
            "the parent's reach was already spent on the child"
        );

        let ordinary = Simplified::at(a, child);
        assert_eq!(
            ordinary.still_wants(a, parent),
            a,
            "the same number at the parent's scale is a coarser ask"
        );
    }

    #[test]
    fn the_residue_still_collapses_zero_length_segments() {
        // Already done still isn't nothing: two crossings on one integer must
        // go.
        let repeated = chain(&[(100, 100), (100, 100), (200, 140), (300, 180), (300, 180)]);
        let out = simplify(&[repeated], 0.0);
        assert_eq!(
            points_of(&out),
            vec![(100, 100), (200, 140), (300, 180)],
            "duplicates go"
        );
        assert_eq!(out.len(), 1, "and the near-collinear vertex is kept");
    }

    #[test]
    fn simplification_never_adds_points() {
        let mut seed = 0xBADA55E1FBABEu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for case in 0..1000 {
            let points: Vec<Coord<Local>> = (0..2 + (next() % 12) as usize)
                .map(|_| Coord {
                    x: (next() % EXTENT as u64) as Local,
                    y: (next() % EXTENT as u64) as Local,
                })
                .collect();
            let input = vec![Chain {
                feature: 1,
                points: points.clone(),
            }];
            for tolerance in [Tolerance::Distance(20.0), Tolerance::Area(400.0)] {
                let out = simplify_chains(&input, EXTENT, tolerance);
                let total: usize = out.iter().map(|c| c.points.len()).sum();
                assert!(
                    total <= points.len(),
                    "case {case} {tolerance:?}: {} points became {total}",
                    points.len()
                );
            }
        }
    }
}
