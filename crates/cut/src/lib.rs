//! Cuts source geometry into per-tile pieces, in one sweep.
//!
//! The only place source geometry is read: everything above maxzoom is built
//! by merging tiles.
//!
//! Each feature's vertices are walked once, splitting segments where they cross
//! tile boundaries: O(vertices + crossings), where clipping against every tile
//! would be O(vertices × tiles). Each crossing is computed once and written
//! into both neighbouring tiles, so the two pieces meet at bit-identical points
//! — which the merge depends on.
//!
//! Coordinates are global `i64` in, tile-local `Local` (`i16`, `0..=extent`)
//! out. Integers throughout, because the merge compares them exactly.
//!
//! **Longitude wraps.** Geometry past the world's east or west edge lands in
//! the column it wraps to, so data written in the 0–360 convention, or running
//! continuously past 180, keeps every piece. Latitude doesn't wrap: anything
//! north or south of the world is dropped.

use geo_types::Coord;
use lyv_core::Local;

pub use lyv_core::{Chain, Pieces, TileCoord};

/// Cut a linestring into per-tile chains. `points` are global coordinates at
/// zoom `z`, on a grid of `extent` units per tile.
pub fn cut_linestring(
    feature: u64,
    points: &[Coord<i64>],
    z: u32,
    extent: u32,
    pieces: &mut Pieces,
) {
    sweep(points, z, extent, &mut |tile, chain| {
        emit(feature, tile, chain, pieces)
    });
}

/// Place a point into the one tile that holds it, assigned the same way a
/// line's crossing is. A point north or south of the world is dropped.
pub fn cut_point(feature: u64, at: Coord<i64>, z: u32, extent: u32, pieces: &mut Pieces) {
    let extent = i64::from(extent);
    let tiles_per_axis = 1i64 << z;

    // A point on a tile's east or south edge belongs to the next tile, at
    // local 0: MapLibre draws no point at `extent`. Longitude 180 wraps to the
    // first column. The world's southern limit has no row after it, so a point
    // there stays in the last row, a unit inside.
    let world = tiles_per_axis * extent;
    let tile_x = at.x.div_euclid(extent);
    let tile_y = if at.y == world {
        tiles_per_axis - 1
    } else {
        at.y.div_euclid(extent)
    };
    if !(0..tiles_per_axis).contains(&tile_y) {
        return;
    }
    let local = Coord {
        x: (at.x - tile_x * extent) as Local,
        y: ((at.y - tile_y * extent).min(extent - 1)) as Local,
    };
    pieces
        .entry(TileCoord {
            z,
            x: tile_x.rem_euclid(tiles_per_axis) as u32,
            y: tile_y as u32,
        })
        .or_default()
        .push(Chain {
            feature,
            points: vec![local],
        });
}

/// Cut a closed ring into per-tile chains. It may be given open or closed.
///
/// If the ring's first vertex is mid-tile, the sweep splits that tile's run
/// into its first and last chains; they are spliced back together here. So
/// every chain either is already closed or starts and ends on the tile
/// boundary, which is what `lyv-close` needs.
pub fn cut_ring(feature: u64, ring: &[Coord<i64>], z: u32, extent: u32, pieces: &mut Pieces) {
    if ring.len() < 3 {
        return;
    }
    let mut closed = ring.to_vec();
    if closed.first() != closed.last() {
        closed.push(closed[0]);
    }

    let mut chains: Vec<(TileCoord, Vec<Coord<Local>>)> = Vec::new();
    sweep(&closed, z, extent, &mut |tile, chain| {
        chains.push((tile, chain))
    });

    // Splice the last chain onto the first when both are in the same tile.
    // That is enough: if the first vertex is on an edge the ring really
    // crosses, the first and last chains are in different tiles.
    if chains.len() >= 2 && chains[0].0 == chains[chains.len() - 1].0 {
        let (_, head) = chains.remove(0);
        let tail = &mut chains.last_mut().expect("at least one chain remains").1;
        tail.extend_from_slice(&head[1..]);
    }

    for (tile, chain) in chains {
        emit(feature, tile, chain, pieces);
    }
}

/// Which tiles a feature touches, without building the chains.
///
/// For the drop ranking, which buckets several levels below maxzoom (§8).
/// Cutting at `z + k` with `extent >> k` addresses the same grid, so this takes
/// the pyramid's coordinates unscaled. A ring must arrive closed.
pub fn for_each_tile(
    points: &[Coord<i64>],
    z: u32,
    extent: u32,
    visit: &mut impl FnMut(TileCoord),
) {
    sweep(points, z, extent, &mut |tile, _chain| visit(tile));
}

/// Walk a polyline once, handing each maximal run within one tile to
/// `emit_chain` in travel order.
fn sweep(
    points: &[Coord<i64>],
    z: u32,
    extent: u32,
    emit_chain: &mut impl FnMut(TileCoord, Vec<Coord<Local>>),
) {
    let extent = i64::from(extent);
    let tiles_per_axis = 1i64 << z;
    let world = tiles_per_axis * extent;

    // Sub-segments each lie in one tile; consecutive ones in the same tile form
    // a chain.
    let mut current: Option<(TileCoord, Vec<Coord<Local>>)> = None;

    for pair in points.windows(2) {
        for (tile_x, tile_y, from, to) in split_segment(pair[0], pair[1], extent) {
            // A segment along the world's east edge belongs to the last column.
            // Its midpoint says the first, where a polygon spanning the whole
            // world would put both its sides. (Points differ: see `cut_point`.)
            let on_east_edge = from.x == to.x && from.x != 0 && from.x.rem_euclid(world) == 0;
            let tile_x = if on_east_edge { tile_x - 1 } else { tile_x };
            if !(0..tiles_per_axis).contains(&tile_y) {
                // North or south of the world: dropped. The open chain is
                // flushed, or a line leaving the world and returning to the
                // same tile would be joined across the gap.
                if let Some((open, chain)) = current.take() {
                    emit_chain(open, chain);
                }
                continue;
            }
            // Addressed by the column it wraps to; `to_local` measures from
            // the unwrapped one, which is the same offset.
            let tile = TileCoord {
                z,
                x: tile_x.rem_euclid(tiles_per_axis) as u32,
                y: tile_y as u32,
            };
            let to_local = |point: Coord<i64>| Coord {
                x: (point.x - tile_x * extent) as Local,
                y: (point.y - tile_y * extent) as Local,
            };

            match &mut current {
                Some((open, chain)) if *open == tile => chain.push(to_local(to)),
                _ => {
                    if let Some((open, chain)) = current.take() {
                        emit_chain(open, chain);
                    }
                    current = Some((tile, vec![to_local(from), to_local(to)]));
                }
            }
        }
    }

    if let Some((tile, chain)) = current.take() {
        emit_chain(tile, chain);
    }
}

fn emit(feature: u64, tile: TileCoord, mut points: Vec<Coord<Local>>, pieces: &mut Pieces) {
    if points.len() >= 2 {
        // Finished: nothing appends to it after this. Chains average nine
        // points in a sixteen-point allocation, and reading is the run's peak,
        // so hand the slack back now.
        points.shrink_to_fit();
        pieces
            .entry(tile)
            .or_default()
            .push(Chain { feature, points });
    }
}

/// Split one segment at every tile boundary it crosses.
///
/// Returns `(tile_x, tile_y, from, to)` in travel order, in global coordinates,
/// with exact integer endpoints on the boundaries.
fn split_segment(
    p: Coord<i64>,
    q: Coord<i64>,
    extent: i64,
) -> Vec<(i64, i64, Coord<i64>, Coord<i64>)> {
    let mut cuts = vec![p];
    cuts.extend(crossings(p, q, extent));
    cuts.push(q);

    let mut out = Vec::new();
    for pair in cuts.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        if from == to {
            // A corner crossing gives two coincident cuts.
            continue;
        }
        // Classify by the midpoint, which is on a boundary only if the whole
        // sub-segment runs along it. Doubled to stay in integers.
        let (tile_x, tile_y) = (
            (from.x + to.x).div_euclid(2 * extent),
            (from.y + to.y).div_euclid(2 * extent),
        );
        out.push((tile_x, tile_y, from, to));
    }
    out
}

/// Every point where the segment crosses a tile boundary, in travel order.
fn crossings(p: Coord<i64>, q: Coord<i64>, extent: i64) -> Vec<Coord<i64>> {
    let (dx, dy) = (q.x - p.x, q.y - p.y);
    let mut found: Vec<(f64, Coord<i64>)> = Vec::new();

    // Vertical boundaries: x is exact, y is interpolated.
    for line in boundaries_between(p.x, q.x, extent) {
        let along = line - p.x;
        found.push((
            along as f64 / dx as f64,
            Coord {
                x: line,
                y: p.y + div_round(i128::from(along) * i128::from(dy), i128::from(dx)),
            },
        ));
    }

    // Horizontal boundaries: y is exact, x is interpolated.
    for line in boundaries_between(p.y, q.y, extent) {
        let along = line - p.y;
        found.push((
            along as f64 / dy as f64,
            Coord {
                x: p.x + div_round(i128::from(along) * i128::from(dx), i128::from(dy)),
                y: line,
            },
        ));
    }

    found.sort_by(|a, b| a.0.total_cmp(&b.0));
    // A crossing through a tile corner appears twice at the same exact point;
    // `split_segment` drops the zero-length piece between them.
    found.into_iter().map(|(_, point)| point).collect()
}

/// The multiples of `extent` strictly between `from` and `to`, in that order.
fn boundaries_between(from: i64, to: i64, extent: i64) -> Vec<i64> {
    if from == to {
        return Vec::new();
    }
    let (low, high) = (from.min(to), from.max(to));
    // First multiple strictly above `low`, last strictly below `high`.
    let first = low.div_euclid(extent) + 1;
    let last = -(-high).div_euclid(extent) - 1;

    let lines: Vec<i64> = (first..=last).map(|k| k * extent).collect();
    if from < to {
        lines
    } else {
        lines.into_iter().rev().collect()
    }
}

/// Division rounding to the nearest integer, halves away from zero.
fn div_round(numerator: i128, denominator: i128) -> i64 {
    let (numerator, denominator) = if denominator < 0 {
        (-numerator, -denominator)
    } else {
        (numerator, denominator)
    };
    let half = denominator / 2;
    let adjusted = if numerator >= 0 {
        numerator + half
    } else {
        numerator - half
    };
    (adjusted / denominator) as i64
}

#[cfg(test)]
mod membership {
    use super::*;

    fn tiles(points: &[(i64, i64)], z: u32, extent: u32) -> Vec<(u32, u32)> {
        let coords: Vec<Coord<i64>> = points.iter().map(|&(x, y)| Coord { x, y }).collect();
        let mut found = std::collections::BTreeSet::new();
        for_each_tile(&coords, z, extent, &mut |tile| {
            found.insert((tile.x, tile.y));
        });
        found.into_iter().collect()
    }

    #[test]
    fn a_point_inside_one_tile_touches_only_that_tile() {
        assert_eq!(tiles(&[(100, 100), (200, 200)], 4, 4096), vec![(0, 0)]);
    }

    #[test]
    fn a_line_across_a_boundary_touches_both_tiles() {
        assert_eq!(
            tiles(&[(4000, 100), (4200, 100)], 4, 4096),
            vec![(0, 0), (1, 0)]
        );
    }

    #[test]
    fn the_same_grid_is_addressed_by_a_deeper_zoom_and_a_smaller_extent() {
        // The property the finer ranking bucket rests on: `z + k` with
        // Cutting at `z + k` with `extent >> k` addresses the same grid.
        let line = [(0, 0), (12_345, 6_789)];
        let coarse = tiles(&line, 4, 4096);
        let fine = tiles(&line, 4 + 3, 4096 >> 3);

        assert!(fine.len() > coarse.len(), "fine {fine:?} coarse {coarse:?}");
        for (x, y) in &fine {
            assert!(
                coarse.contains(&(x >> 3, y >> 3)),
                "fine tile {x},{y} is not inside any coarse tile of {coarse:?}"
            );
        }
    }

    #[test]
    fn a_closed_ring_reports_the_tiles_along_its_closing_segment() {
        // Closed: the closing segment crosses tiles nothing else does.
        let open = tiles(&[(100, 100), (9000, 100), (9000, 200)], 4, 4096);
        let closed = tiles(&[(100, 100), (9000, 100), (9000, 200), (100, 100)], 4, 4096);
        assert!(
            closed.len() >= open.len(),
            "closing lost tiles: {closed:?} against {open:?}"
        );
        assert!(closed.contains(&(0, 0)) && closed.contains(&(2, 0)));
    }

    #[test]
    fn nothing_is_reported_for_a_single_point() {
        assert_eq!(tiles(&[(100, 100)], 4, 4096), Vec::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXTENT: u32 = 4096;
    const E: i64 = EXTENT as i64;

    fn coords(points: &[(i64, i64)]) -> Vec<Coord<i64>> {
        points.iter().map(|&(x, y)| Coord { x, y }).collect()
    }

    fn local(points: &[(Local, Local)]) -> Vec<Coord<Local>> {
        points.iter().map(|&(x, y)| Coord { x, y }).collect()
    }

    #[test]
    fn shrinking_an_exact_vector_does_nothing_at_all() {
        // Chains are shrunk more than once. That relies on `shrink_to_fit`
        // being free on an exact vector; if it weren't, it would cost millions
        // of reallocations in the hottest stage.
        let mut points = Vec::with_capacity(16);
        points.extend(local(&[(1, 2), (3, 4), (5, 6)]));
        assert_eq!(points.capacity(), 16);

        points.shrink_to_fit();
        assert_eq!(
            points.capacity(),
            3,
            "the first shrink gives the slack back"
        );
        let settled = points.as_ptr();

        points.shrink_to_fit();
        points.shrink_to_fit();
        assert_eq!(points.capacity(), 3);
        assert!(
            std::ptr::eq(points.as_ptr(), settled),
            "shrinking an exact vector must not reallocate"
        );
        assert_eq!(
            points,
            local(&[(1, 2), (3, 4), (5, 6)]),
            "and must not disturb the contents"
        );
    }

    fn cut(points: &[(i64, i64)]) -> Pieces {
        let mut pieces = Pieces::new();
        cut_linestring(7, &coords(points), 5, EXTENT, &mut pieces);
        pieces
    }

    fn tile(x: u32, y: u32) -> TileCoord {
        TileCoord { z: 5, x, y }
    }

    #[test]
    fn a_line_inside_one_tile_is_left_alone() {
        let pieces = cut(&[(100, 100), (300, 700), (900, 200)]);

        assert_eq!(pieces.len(), 1);
        assert_eq!(
            pieces[&tile(0, 0)],
            vec![Chain {
                feature: 7,
                points: local(&[(100, 100), (300, 700), (900, 200)]),
            }]
        );
    }

    #[test]
    fn a_line_crossing_one_boundary_splits_at_the_same_point_in_both_tiles() {
        // Horizontal, so the crossing lands at exactly y = 100 with no rounding.
        let pieces = cut(&[(E - 96, 100), (E + 104, 100)]);

        assert_eq!(pieces.len(), 2, "should touch exactly two tiles");
        assert_eq!(
            pieces[&tile(0, 0)][0].points,
            local(&[(4000, 100), (4096, 100)]),
            "left piece should run up to its right edge"
        );
        assert_eq!(
            pieces[&tile(1, 0)][0].points,
            local(&[(0, 100), (104, 100)]),
            "right piece should start at its left edge"
        );
    }

    #[test]
    fn the_shared_endpoint_is_identical_in_both_tiles() {
        // Diagonal, so the crossing is interpolated and rounded.
        let pieces = cut(&[(E - 1000, 40), (E + 1000, 2531)]);

        let left = pieces[&tile(0, 0)][0].points.last().unwrap();
        let right = pieces[&tile(1, 0)][0].points.first().unwrap();

        assert_eq!(left.x, EXTENT as Local, "left piece ends on its right edge");
        assert_eq!(right.x, 0, "right piece starts on its left edge");
        assert_eq!(
            left.y, right.y,
            "the same crossing must be computed once, not twice"
        );
    }

    #[test]
    fn an_interpolated_crossing_rounds_rather_than_truncating() {
        // Truncating would bias every crossing the same way and drift the
        // layer. The crossing lands exactly on a half unit.
        let pieces = cut(&[(E - 1, 0), (E + 1, 5)]);

        let crossing = *pieces[&tile(0, 0)][0].points.last().unwrap();
        assert_eq!(crossing.x, EXTENT as Local);
        // y = 2.5, which rounds to 3.
        assert_eq!(crossing.y, 3, "2.5 should round up, not truncate to 2");
    }

    #[test]
    fn a_line_crossing_many_tiles_is_covered_without_gaps() {
        // Diagonal across a 4x4 block of tiles.
        let pieces = cut(&[(10, 10), (4 * E - 10, 4 * E - 10)]);

        assert!(
            pieces.len() >= 4,
            "should touch at least the four diagonal tiles"
        );

        // Every chain must lie within its own tile's bounds.
        for (coord, chains) in &pieces {
            for chain in chains {
                for point in &chain.points {
                    assert!(
                        (0..=EXTENT as Local).contains(&point.x)
                            && (0..=EXTENT as Local).contains(&point.y),
                        "{point:?} is outside tile {coord:?}"
                    );
                }
                assert!(chain.points.len() >= 2, "a chain needs at least two points");
            }
        }
    }

    /// Put every chain back into global coordinates and walk them end to end.
    /// Panics unless they form one unbroken path; returns it.
    fn reassemble(pieces: &Pieces, start: Coord<i64>) -> Vec<Coord<i64>> {
        let mut remaining: Vec<Vec<Coord<i64>>> = pieces
            .iter()
            .flat_map(|(coord, chains)| {
                chains.iter().map(move |chain| {
                    chain
                        .points
                        .iter()
                        .map(|p| Coord {
                            x: i64::from(p.x) + i64::from(coord.x) * E,
                            y: i64::from(p.y) + i64::from(coord.y) * E,
                        })
                        .collect()
                })
            })
            .collect();

        let first = remaining
            .iter()
            .position(|chain: &Vec<Coord<i64>>| chain[0] == start)
            .expect("some chain should start where the line starts");
        let mut path = remaining.remove(first);

        while !remaining.is_empty() {
            let tail = *path.last().unwrap();
            let next = remaining
                .iter()
                .position(|chain| chain[0] == tail)
                .unwrap_or_else(|| panic!("no chain continues from {tail:?}"));
            let mut chain = remaining.remove(next);
            chain.remove(0); // drop the duplicated join point
            path.extend(chain);
        }
        path
    }

    /// Every chain must lie inside the tile that holds it.
    fn assert_chains_within_their_tiles(pieces: &Pieces) {
        for (coord, chains) in pieces {
            for chain in chains {
                assert!(chain.points.len() >= 2, "a chain needs at least two points");
                for point in &chain.points {
                    assert!(
                        (0..=EXTENT as Local).contains(&point.x)
                            && (0..=EXTENT as Local).contains(&point.y),
                        "{point:?} is outside tile {coord:?}"
                    );
                }
            }
        }
    }

    /// A small deterministic PRNG, so failures reproduce.
    fn prng(mut seed: u64) -> impl FnMut() -> u64 {
        move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        }
    }

    fn length(points: &[Coord<i64>]) -> f64 {
        points
            .windows(2)
            .map(|w| {
                let (dx, dy) = ((w[1].x - w[0].x) as f64, (w[1].y - w[0].y) as f64);
                dx.hypot(dy)
            })
            .sum()
    }

    /// Shortest distance from `point` to the polyline through `points`.
    fn distance_to_path(point: Coord<i64>, points: &[Coord<i64>]) -> f64 {
        points
            .windows(2)
            .map(|w| {
                let (px, py) = ((point.x - w[0].x) as f64, (point.y - w[0].y) as f64);
                let (dx, dy) = ((w[1].x - w[0].x) as f64, (w[1].y - w[0].y) as f64);
                let square = dx * dx + dy * dy;
                let t = ((px * dx + py * dy) / square).clamp(0.0, 1.0);
                (px - t * dx).hypot(py - t * dy)
            })
            .fold(f64::INFINITY, f64::min)
    }

    #[test]
    fn any_monotone_line_survives_being_cut_and_reassembled() {
        // Strictly increasing x can't self-intersect, so reassembly is
        // unambiguous and must be exact. Zoom 3 is an 8x8 grid.
        let mut next = prng(0xBADA55E1FBABE);

        for case in 0..2000 {
            let span = 8 * E;
            let vertices = 2 + (next() % 6) as usize;

            let mut xs: Vec<i64> = (0..vertices)
                .map(|_| (next() % span as u64) as i64)
                .collect();
            xs.sort_unstable();
            xs.dedup();
            if xs.len() < 2 {
                continue;
            }
            let original: Vec<Coord<i64>> = xs
                .into_iter()
                .map(|x| Coord {
                    x,
                    y: (next() % span as u64) as i64,
                })
                .collect();

            let mut pieces = Pieces::new();
            cut_linestring(1, &original, 3, EXTENT, &mut pieces);

            assert_chains_within_their_tiles(&pieces);

            let path = reassemble(&pieces, original[0]);
            assert_eq!(*path.first().unwrap(), original[0], "case {case}");
            assert_eq!(
                *path.last().unwrap(),
                *original.last().unwrap(),
                "case {case}"
            );
            for vertex in &original {
                assert!(
                    path.contains(vertex),
                    "case {case}: lost original vertex {vertex:?}"
                );
            }
        }
    }

    #[test]
    fn any_line_at_all_is_cut_without_losing_or_inventing_geometry() {
        // Unrestricted lines self-intersect, making reassembly ambiguous. So
        // check invariants instead: length preserved, every point on the line.
        let mut next = prng(0xBADA55E1FBABE);

        for case in 0..2000 {
            let span = 8 * E;
            let vertices = 2 + (next() % 6) as usize;
            let original: Vec<Coord<i64>> = (0..vertices)
                .map(|_| Coord {
                    x: (next() % span as u64) as i64,
                    y: (next() % span as u64) as i64,
                })
                .collect();
            if original.windows(2).any(|w| w[0] == w[1]) {
                continue;
            }

            let mut pieces = Pieces::new();
            cut_linestring(1, &original, 3, EXTENT, &mut pieces);
            assert_chains_within_their_tiles(&pieces);

            let mut cut_length = 0.0;
            let mut points = 0usize;
            for (coord, chains) in &pieces {
                for chain in chains {
                    let global: Vec<Coord<i64>> = chain
                        .points
                        .iter()
                        .map(|p| Coord {
                            x: i64::from(p.x) + i64::from(coord.x) * E,
                            y: i64::from(p.y) + i64::from(coord.y) * E,
                        })
                        .collect();
                    cut_length += length(&global);
                    points += global.len();

                    // Nothing invented: every point sits on the original line.
                    for point in &global {
                        let off = distance_to_path(*point, &original);
                        assert!(
                            off < 1.0,
                            "case {case}: {point:?} is {off:.3} units off the line"
                        );
                    }
                }
            }

            // Each crossing rounds by at most half a unit on one axis, changing
            // the length by at most one unit.
            let slack = points as f64;
            let original_length = length(&original);
            assert!(
                (cut_length - original_length).abs() <= slack,
                "case {case}: cut length {cut_length:.1} vs original \
                 {original_length:.1}, slack {slack:.1} — geometry lost or duplicated"
            );
        }
    }

    #[test]
    fn leaving_the_world_and_returning_to_the_same_tile_breaks_the_chain() {
        // A line crosses the antimeridian and comes back to the tile it left.
        // The two runs in that tile stay separate chains; the part between
        // wraps to the first column.
        let span = 1i64 << 2; // zoom 2, so the world is 4 tiles across
        let world = span * E;

        let mut pieces = Pieces::new();
        cut_linestring(
            1,
            &coords(&[
                (world - 500, E + 500),
                (world + 800, E + 900),
                (world + 800, E + 2000),
                (world - 500, E + 2500),
            ]),
            2,
            EXTENT,
            &mut pieces,
        );

        let chains = &pieces[&TileCoord { z: 2, x: 3, y: 1 }];
        assert_eq!(
            chains.len(),
            2,
            "the two in-world runs are separate: {:?}",
            chains.iter().map(|c| &c.points).collect::<Vec<_>>()
        );
        for chain in chains {
            for pair in chain.points.windows(2) {
                assert_ne!(pair[0], pair[1], "zero-length segment");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Rings
    // -----------------------------------------------------------------------

    fn cut_a_ring(points: &[(i64, i64)], z: u32) -> Pieces {
        let mut pieces = Pieces::new();
        cut_ring(7, &coords(points), z, EXTENT, &mut pieces);
        pieces
    }

    fn on_the_boundary(p: Coord<Local>) -> bool {
        p.x == 0 || p.x == EXTENT as Local || p.y == 0 || p.y == EXTENT as Local
    }

    #[test]
    fn a_ring_wholly_inside_one_tile_stays_one_closed_chain() {
        let pieces = cut_a_ring(&[(100, 100), (900, 100), (900, 900)], 5);

        assert_eq!(pieces.len(), 1);
        let chain = &pieces[&tile(0, 0)][0];
        assert_eq!(
            chain.points.first(),
            chain.points.last(),
            "a ring inside one tile comes back closed"
        );
        assert_eq!(
            chain.points,
            local(&[(100, 100), (900, 100), (900, 900), (100, 100)])
        );
    }

    #[test]
    fn a_ring_given_already_closed_is_not_closed_twice() {
        let open = cut_a_ring(&[(100, 100), (900, 100), (900, 900)], 5);
        let closed = cut_a_ring(&[(100, 100), (900, 100), (900, 900), (100, 100)], 5);
        assert_eq!(open, closed);
    }

    #[test]
    fn a_ring_starting_inside_a_tile_is_spliced_into_one_chain() {
        // The first vertex (100, 100) is inside tile (0, 0), so the sweep splits
        // that tile's run into its first and last chains.
        let pieces = cut_a_ring(&[(100, 100), (E + 500, 100), (E + 500, 900), (100, 900)], 5);

        let chains = &pieces[&tile(0, 0)];
        assert_eq!(
            chains.len(),
            1,
            "the two halves should be spliced, got {:?}",
            chains.iter().map(|c| &c.points).collect::<Vec<_>>()
        );

        let points = &chains[0].points;
        assert!(
            on_the_boundary(points[0]) && on_the_boundary(*points.last().unwrap()),
            "a spliced chain must start and end on the tile edge, got {:?} to {:?}",
            points[0],
            points.last().unwrap()
        );
        // The start vertex is now mid-chain.
        assert!(
            points.contains(&Coord { x: 100, y: 100 }),
            "the ring's own first vertex should survive inside the chain"
        );
    }

    #[test]
    fn a_ring_crossing_a_tile_edge_at_its_first_vertex_is_not_spliced() {
        // The mirror case: the first vertex is on the edge between (0,0) and
        // (1,0), leaving westward and returning from the east. The first and
        // last chains are in different tiles, so there must be no splice.
        let pieces = cut_a_ring(
            &[
                (E, 2000),
                (E - 800, 2000),
                (E - 800, 1000),
                (E + 800, 1000),
                (E + 800, 2000),
            ],
            5,
        );

        assert_eq!(pieces.len(), 2, "the ring spans exactly two tiles");
        assert_eq!(pieces[&tile(0, 0)].len(), 1);
        assert_eq!(pieces[&tile(1, 0)].len(), 1);

        let west = &pieces[&tile(0, 0)][0].points;
        assert_eq!(
            west,
            &local(&[
                (E as Local, 2000),
                (3296, 2000),
                (3296, 1000),
                (E as Local, 1000)
            ]),
            "the western chain should be the part west of the edge, no more"
        );
    }

    #[test]
    fn cutting_a_ring_neither_invents_nor_loses_boundary() {
        // Length catches a splice of two chains that don't meet: the jump stays
        // inside the tile and on the ring's vertices, so only length shows it.
        // Zero-length segments are checked too, since a duplicated point costs
        // no length.
        let mut next = prng(0xBADA55E1FBABE);

        for case in 0..2000 {
            let span = 4 * E;
            let vertices = 3 + (next() % 6) as usize;
            let ring: Vec<Coord<i64>> = (0..vertices)
                .map(|_| Coord {
                    x: (next() % span as u64) as i64,
                    y: (next() % span as u64) as i64,
                })
                .collect();
            // A repeated source vertex would make the duplicate check ambiguous.
            let mut closed = ring.clone();
            closed.push(ring[0]);
            if closed.windows(2).any(|w| w[0] == w[1]) {
                continue;
            }

            let mut pieces = Pieces::new();
            cut_ring(1, &ring, 2, EXTENT, &mut pieces);

            let mut cut_length = 0.0;
            let mut points = 0usize;
            for (coord, chains) in &pieces {
                for chain in chains {
                    for pair in chain.points.windows(2) {
                        assert_ne!(
                            pair[0], pair[1],
                            "case {case}: zero-length segment in {coord:?}"
                        );
                    }
                    let global: Vec<Coord<i64>> = chain
                        .points
                        .iter()
                        .map(|p| Coord {
                            x: i64::from(p.x) + i64::from(coord.x) * E,
                            y: i64::from(p.y) + i64::from(coord.y) * E,
                        })
                        .collect();
                    cut_length += length(&global);
                    points += global.len();
                }
            }

            // Each crossing rounds by at most half a unit on one axis.
            let slack = points as f64;
            let original = length(&closed);
            assert!(
                (cut_length - original).abs() <= slack,
                "case {case}: cut boundary {cut_length:.1} against original \
                 {original:.1}, slack {slack:.1} — an edge was invented or lost"
            );
        }
    }

    #[test]
    fn every_chain_of_any_ring_is_closed_or_runs_edge_to_edge() {
        // `lyv-close` can only close chains whose ends lie on the perimeter.
        let mut next = prng(0xBADA55E1FBABE);

        for case in 0..2000 {
            let span = 4 * E;
            let vertices = 3 + (next() % 6) as usize;
            let ring: Vec<Coord<i64>> = (0..vertices)
                .map(|_| Coord {
                    x: (next() % span as u64) as i64,
                    y: (next() % span as u64) as i64,
                })
                .collect();

            let mut pieces = Pieces::new();
            cut_ring(1, &ring, 2, EXTENT, &mut pieces);

            for (coord, chains) in &pieces {
                for chain in chains {
                    let (first, last) = (chain.points[0], *chain.points.last().unwrap());
                    assert!(
                        first == last || (on_the_boundary(first) && on_the_boundary(last)),
                        "case {case}: chain in {coord:?} runs {first:?} to {last:?}, \
                         which is neither closed nor edge to edge"
                    );
                    for point in &chain.points {
                        assert!(
                            (0..=EXTENT as Local).contains(&point.x)
                                && (0..=EXTENT as Local).contains(&point.y),
                            "case {case}: {point:?} is outside {coord:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn reassembling_every_chain_reproduces_the_original_line() {
        // Cutting loses nothing.
        let original = coords(&[
            (10, 10),
            (2 * E + 500, E - 300),
            (E - 20, 3 * E + 100),
            (5 * E, 5 * E),
        ]);
        let mut pieces = Pieces::new();
        cut_linestring(7, &original, 5, EXTENT, &mut pieces);

        let mut global: Vec<Vec<Coord<i64>>> = Vec::new();
        for (coord, chains) in &pieces {
            for chain in chains {
                global.push(
                    chain
                        .points
                        .iter()
                        .map(|p| Coord {
                            x: i64::from(p.x) + i64::from(coord.x) * E,
                            y: i64::from(p.y) + i64::from(coord.y) * E,
                        })
                        .collect(),
                );
            }
        }

        // Order chains by matching endpoints, from the original start.
        let mut remaining = global;
        let mut path = remaining
            .iter()
            .position(|c| c[0] == original[0])
            .map(|i| remaining.remove(i))
            .expect("some chain should start where the line starts");

        while !remaining.is_empty() {
            let tail = *path.last().unwrap();
            let next = remaining
                .iter()
                .position(|c| c[0] == tail)
                .unwrap_or_else(|| panic!("no chain continues from {tail:?}"));
            let mut chain = remaining.remove(next);
            chain.remove(0); // drop the duplicated join point
            path.extend(chain);
        }

        assert_eq!(*path.first().unwrap(), original[0]);
        assert_eq!(*path.last().unwrap(), *original.last().unwrap());

        // Every original vertex must survive somewhere in the path.
        for vertex in &original {
            assert!(path.contains(vertex), "lost original vertex {vertex:?}");
        }
    }
}
