//! Merges four child tiles into their parent.
//!
//! Children are placed in a doubled frame (`2 * extent`), joined there, and
//! only then halved. Joining must see exact coordinates: two coincident edges
//! can round apart and fail to cancel.
//!
//! **Lines** rejoin only where an endpoint lies on an internal seam
//! (`x = mid` or `y = mid`). A feature can leave the tile and come back at the
//! same point elsewhere, which looks like a join but isn't.
//!
//! **Rings** are unioned by cancelling their shared seam edges (`lyv-seam`).

use std::collections::HashMap;

use geo_types::Coord;

use lyv_core::{Chain, Local, Pieces, TileCoord};

/// Merge every tile of one zoom level into the level above it, on one thread.
///
/// `pieces` must all be at the same zoom. Returns the tiles at `zoom - 1`. The
/// pipeline uses [`group`] and [`Grouped::merge`] to merge parents in parallel.
pub fn merge_level(pieces: &Pieces, extent: u32) -> Pieces {
    merge_level_to(pieces, extent, extent)
}

/// As [`merge_level`], into a parent of a different extent.
pub fn merge_level_to(pieces: &Pieces, child_extent: u32, parent_extent: u32) -> Pieces {
    let mut grouped = group(pieces.clone(), child_extent, parent_extent, true);
    std::mem::take(&mut grouped.parents)
        .into_iter()
        .map(|(parent, chains)| (parent, grouped.merge(chains)))
        .collect()
}

/// Children gathered under their parents, not yet merged. Each parent reads
/// only its own children, so merging parallelises by parent.
pub struct Grouped {
    /// A vector, not a map: workers take these by index.
    pub parents: Vec<(TileCoord, Vec<Chain>)>,
    extent: Local,
    divisor: Local,
    /// Whether a feature that shrinks away leaves a mark behind.
    dust: bool,
}

impl Grouped {
    /// Join and halve one parent's worth of geometry.
    pub fn merge(&self, chains: Vec<Chain>) -> Vec<Chain> {
        merge_one(chains, self.extent, self.divisor, self.dust)
    }
}

/// Gather children under their parents without merging any of them.
pub fn group(pieces: Pieces, child_extent: u32, parent_extent: u32, dust: bool) -> Grouped {
    let frame = 2 * child_extent;
    assert!(
        frame.is_multiple_of(parent_extent),
        "a parent of {parent_extent} cannot hold four children of {child_extent}"
    );
    let divisor = Local::try_from(frame / parent_extent).expect("a sane scale");
    let extent = child_extent as Local;
    let mut grouped: HashMap<TileCoord, Vec<Chain>> = HashMap::new();

    // Taken by value and offset in place: copying held the level twice, and
    // peaked at 13.1 GB on floodplains.
    for (child, mut chains) in pieces {
        let parent = TileCoord {
            z: child.z.saturating_sub(1),
            x: child.x / 2,
            y: child.y / 2,
        };
        // The child's quadrant. No halving yet.
        let offset_x = Local::try_from(child.x % 2).unwrap() * extent;
        let offset_y = Local::try_from(child.y % 2).unwrap() * extent;

        for chain in chains.iter_mut() {
            for point in chain.points.iter_mut() {
                point.x += offset_x;
                point.y += offset_y;
            }
        }
        grouped.entry(parent).or_default().append(&mut chains);
    }

    Grouped {
        parents: grouped.into_iter().collect(),
        extent,
        divisor,
        dust,
    }
}

/// Join one parent's geometry, then scale it into the parent's frame. The
/// seams are at `mid = extent` in the doubled frame.
fn merge_one(chains: Vec<Chain>, extent: Local, divisor: Local, dust: bool) -> Vec<Chain> {
    let mid = extent;

    // Points have nothing to join or cancel; the line path would drop them.
    let (dots, rest): (Vec<Chain>, Vec<Chain>) =
        chains.into_iter().partition(|c| c.points.len() == 1);

    let (rings, lines): (Vec<Chain>, Vec<Chain>) =
        rest.into_iter().partition(|c| lyv_seam::is_ring(&c.points));

    let mut out = Vec::new();

    // Halved, then deduplicated within a feature: a MultiPoint thins itself as
    // its points round together. Two features at one coordinate both stay;
    // thinning those is the ranking's job. Insertion order is kept.
    //
    // A point that rounds onto the far edge is pulled back a unit: MapLibre
    // draws no point at `extent`, and the tile it would belong to is another
    // parent's.
    let parent_extent = 2 * extent / divisor;
    let mut seen: std::collections::HashSet<(u64, Local, Local)> = std::collections::HashSet::new();
    for dot in dots {
        let mut points = scale(&dot.points, divisor);
        points[0].x = points[0].x.min(parent_extent - 1);
        points[0].y = points[0].y.min(parent_extent - 1);
        let at = points[0];
        if seen.insert((dot.feature, at.x, at.y)) {
            out.push(Chain {
                feature: dot.feature,
                points,
            });
        }
    }

    // A line that halves onto one coordinate is set aside with its unhalved
    // endpoints (its direction), to become dust below if the feature has
    // nothing else left.
    let mut collapsed: Vec<(u64, Coord<Local>, Coord<Local>)> = Vec::new();
    for chain in join_at_seams(lines, mid) {
        let mut points = scale(&chain.points, divisor);
        points.dedup();
        if points.len() >= 2 {
            out.push(Chain {
                feature: chain.feature,
                points,
            });
        } else if dust && !points.is_empty() {
            let (first, last) = (chain.points[0], chain.points[chain.points.len() - 1]);
            collapsed.push((chain.feature, points[0], direction(first, last)));
        }
    }

    // Cancel, then halve: halving is where rounding happens.
    for welded in lyv_seam::weld(&rings, &[mid], &[mid]) {
        // A ring that rounds itself inside out is dropped, not kept as a
        // phantom hole.
        let before = lyv_seam::double_area(&welded.points);
        if let Some(points) =
            lyv_seam::tidy_keeping_winding(scale(&welded.points, divisor), Some(before))
        {
            out.push(Chain {
                feature: welded.feature,
                points,
            });
        }
    }

    // Line dust: one stub per feature, and only where nothing else of it is
    // left — the same rule as ring dust.
    if !collapsed.is_empty() {
        let present: std::collections::HashSet<u64> = out.iter().map(|c| c.feature).collect();
        let mut given: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for (feature, at, step) in collapsed {
            if present.contains(&feature) || !given.insert(feature) {
                continue;
            }
            out.push(Chain {
                feature,
                points: unit_segment(at, step, extent * 2 / divisor),
            });
        }
    }
    out
}

/// Which way a run pointed, as a sign per axis. A run whose ends coincide gets
/// east; any choice is arbitrary, but it's the same every run.
fn direction(first: Coord<Local>, last: Coord<Local>) -> Coord<Local> {
    let step = Coord {
        x: (last.x - first.x).signum(),
        y: (last.y - first.y).signum(),
    };
    if step.x == 0 && step.y == 0 {
        Coord { x: 1, y: 0 }
    } else {
        step
    }
}

/// The shortest segment the grid can hold at `at`, pointing along `step`.
///
/// If the far end would leave the tile, the segment slides back a unit rather
/// than reversing: direction is the one thing a reader can check against the
/// neighbours.
fn unit_segment(at: Coord<Local>, step: Coord<Local>, extent: Local) -> Vec<Coord<Local>> {
    let head = Coord {
        x: at.x + step.x,
        y: at.y + step.y,
    };
    let inside = |p: Coord<Local>| (0..=extent).contains(&p.x) && (0..=extent).contains(&p.y);
    if inside(head) {
        vec![at, head]
    } else {
        let tail = Coord {
            x: at.x - step.x,
            y: at.y - step.y,
        };
        vec![tail, at]
    }
}

fn scale(points: &[Coord<Local>], divisor: Local) -> Vec<Coord<Local>> {
    points
        .iter()
        .map(|p| Coord {
            x: scale_one(p.x, divisor),
            y: scale_one(p.y, divisor),
        })
        .collect()
}

/// Scale down, rounding to nearest (`docs/quant.py`: truncation drifts the
/// layer). Halves go away from zero, like the cut's `div_round`.
fn scale_one(value: Local, divisor: Local) -> Local {
    let half = divisor / 2;
    if value >= 0 {
        (value + half) / divisor
    } else {
        (value - half) / divisor
    }
}

/// Is this point on one of the parent's two internal seams?
fn on_seam(point: Coord<Local>, mid: Local) -> bool {
    point.x == mid || point.y == mid
}

/// Rejoin chains that were split at a child boundary.
fn join_at_seams(chains: Vec<Chain>, mid: Local) -> Vec<Chain> {
    // Index chain starts on a seam: the only starts a predecessor can meet.
    let mut starts: HashMap<(u64, Local, Local), Vec<usize>> = HashMap::new();
    for (index, chain) in chains.iter().enumerate() {
        let first = chain.points[0];
        if on_seam(first, mid) {
            starts
                .entry((chain.feature, first.x, first.y))
                .or_default()
                .push(index);
        }
    }

    // And ends, so finding a predecessor is a lookup, not a quadratic scan.
    let mut ends: HashMap<(u64, Local, Local), Vec<usize>> = HashMap::new();
    for (index, chain) in chains.iter().enumerate() {
        let last = *chain.points.last().expect("a chain has points");
        if on_seam(last, mid) {
            ends.entry((chain.feature, last.x, last.y))
                .or_default()
                .push(index);
        }
    }

    let mut consumed = vec![false; chains.len()];
    let mut out = Vec::new();

    for index in 0..chains.len() {
        if consumed[index] {
            continue;
        }
        // Start runs only at a chain nothing feeds into.
        let head = chains[index].points[0];
        if on_seam(head, mid)
            && has_predecessor(&ends, &consumed, index, chains[index].feature, head)
        {
            continue;
        }

        consumed[index] = true;
        let mut points = chains[index].points.clone();
        let feature = chains[index].feature;

        // Follow the chain forwards through seam joins.
        loop {
            let tail = *points.last().unwrap();
            if !on_seam(tail, mid) {
                break;
            }
            let Some(next) = starts
                .get(&(feature, tail.x, tail.y))
                .and_then(|candidates| candidates.iter().find(|&&i| !consumed[i]).copied())
            else {
                break;
            };
            consumed[next] = true;
            points.extend_from_slice(&chains[next].points[1..]);
        }

        out.push(Chain { feature, points });
    }

    // Whatever is left forms closed loops on seams; emitted as is.
    for (index, chain) in chains.iter().enumerate() {
        if !consumed[index] {
            out.push(chain.clone());
        }
    }
    out
}

/// Does some other unconsumed chain of the same feature end where `index`
/// begins?
fn has_predecessor(
    ends: &HashMap<(u64, Local, Local), Vec<usize>>,
    consumed: &[bool],
    index: usize,
    feature: u64,
    head: Coord<Local>,
) -> bool {
    ends.get(&(feature, head.x, head.y))
        .is_some_and(|candidates| {
            candidates
                .iter()
                .any(|&other| other != index && !consumed[other])
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyv_cut::cut_linestring;

    const EXTENT: u32 = 4096;

    fn at(x: Local, y: Local) -> Coord<Local> {
        Coord { x, y }
    }

    #[test]
    fn a_collapsed_line_keeps_the_way_it_ran() {
        // (4,4)->(5,5) halves to (2,2); the stub keeps its direction: (3,3).
        assert_eq!(
            unit_segment(at(2, 2), direction(at(4, 4), at(5, 5)), EXTENT as Local),
            vec![at(2, 2), at(3, 3)]
        );
        // Reversed, it keeps the reversed direction.
        assert_eq!(
            unit_segment(at(2, 2), direction(at(5, 5), at(4, 4)), EXTENT as Local),
            vec![at(2, 2), at(1, 1)]
        );
    }

    #[test]
    fn the_anchor_gives_way_at_the_tile_edge_and_the_direction_does_not() {
        // Slides back a unit rather than reversing.
        let east = direction(at(0, 0), at(10, 0));
        let stub = unit_segment(at(4096, 100), east, 4096);
        assert_eq!(stub, vec![at(4095, 100), at(4096, 100)]);
        assert!(stub[1].x > stub[0].x, "reversed: {stub:?}");
        for p in &stub {
            assert!((0..=4096).contains(&p.x) && (0..=4096).contains(&p.y));
        }
        // The near edge behaves the same way in the other direction.
        let west = direction(at(10, 0), at(0, 0));
        assert_eq!(
            unit_segment(at(0, 100), west, 4096),
            vec![at(1, 100), at(0, 100)]
        );
    }

    #[test]
    fn a_run_whose_ends_coincide_still_gets_a_direction() {
        // No direction left: east, consistently.
        assert_eq!(direction(at(7, 7), at(7, 7)), at(1, 0));
    }

    #[test]
    fn extra_detail_at_the_child_level_scales_by_more_than_a_half() {
        // A frame of 16384 down to a parent of 4096: a quarter.
        let mut pieces = Pieces::new();
        let full = |e: Local| {
            let mut p = vec![
                Coord { x: 0, y: 0 },
                Coord { x: e, y: 0 },
                Coord { x: e, y: e },
                Coord { x: 0, y: e },
            ];
            p.push(p[0]);
            Chain {
                feature: 1,
                points: p,
            }
        };
        for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            pieces.insert(TileCoord { z: 1, x, y }, vec![full(8192)]);
        }

        let parent = merge_level_to(&pieces, 8192, 4096);
        let chains = &parent[&TileCoord { z: 0, x: 0, y: 0 }];
        assert_eq!(chains.len(), 1, "four full children make one full parent");
        assert_eq!(
            chains[0].points,
            full(4096).points,
            "the parent should fill its own 4096 grid exactly"
        );
    }

    #[test]
    fn scaling_by_a_quarter_still_rounds_to_nearest() {
        assert_eq!(scale_one(0, 4), 0);
        assert_eq!(scale_one(2, 4), 1, "0.5 rounds away from zero");
        assert_eq!(scale_one(5, 4), 1);
        assert_eq!(scale_one(6, 4), 2);
        assert_eq!(scale_one(-6, 4), -2, "and symmetrically for negatives");
    }

    #[test]
    fn halving_rounds_to_nearest_rather_than_truncating() {
        // Truncation would drift the layer. Halves go away from zero, like the
        // cut.
        assert_eq!(scale_one(0, 2), 0);
        assert_eq!(scale_one(2, 2), 1);
        assert_eq!(
            scale_one(3, 2),
            2,
            "1.5 rounds away from zero, not down to 1"
        );
        assert_eq!(scale_one(4, 2), 2);
        assert_eq!(scale_one(-3, 2), -2, "and symmetrically for negatives");
        assert_eq!(scale_one(-4, 2), -2);

        // Over consecutive integers, rounding averages out; truncation doesn't.
        let rounded: Local = (0..1000).map(|v| scale_one(v, 2) * 2 - v).sum();
        assert_eq!(rounded, 500, "round-half-away leaves no systematic drift");
        let truncated: Local = (0..1000).map(|v| (v / 2) * 2 - v).sum();
        assert_eq!(truncated, -500, "truncation drifts every odd coordinate");
    }

    const E: i64 = EXTENT as i64;

    fn coords(points: &[(i64, i64)]) -> Vec<Coord<i64>> {
        points.iter().map(|&(x, y)| Coord { x, y }).collect()
    }

    fn cut_at(points: &[(i64, i64)], z: u32) -> Pieces {
        let mut pieces = Pieces::new();
        cut_linestring(1, &coords(points), z, EXTENT, &mut pieces);
        pieces
    }

    /// Every chain, in global coordinates for its own zoom.
    fn global(pieces: &Pieces) -> Vec<Vec<Coord<i64>>> {
        pieces
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
            .collect()
    }

    fn total_length(pieces: &Pieces) -> f64 {
        global(pieces)
            .iter()
            .flat_map(|chain| {
                chain
                    .windows(2)
                    .map(|w| {
                        let (dx, dy) = ((w[1].x - w[0].x) as f64, (w[1].y - w[0].y) as f64);
                        dx.hypot(dy)
                    })
                    .collect::<Vec<_>>()
            })
            .sum()
    }

    #[test]
    fn a_line_split_across_two_children_comes_back_as_one_chain() {
        // Crosses from child 3/0/0 to 3/1/0, an internal seam of 2/0/0.
        let pieces = cut_at(&[(E - 500, 300), (E + 500, 300)], 3);
        assert_eq!(pieces.len(), 2, "should start life as two chains");

        let merged = merge_level(&pieces, EXTENT);

        assert_eq!(merged.len(), 1, "both children share one parent");
        let chains = &merged[&TileCoord { z: 2, x: 0, y: 0 }];
        assert_eq!(chains.len(), 1, "the two pieces should be rejoined");
        assert_eq!(
            (chains[0].points.first(), chains[0].points.last()),
            (
                Some(&Coord { x: 1798, y: 150 }),
                Some(&Coord { x: 2298, y: 150 })
            )
        );
    }

    #[test]
    fn a_line_reaching_the_parents_own_boundary_stays_open() {
        // Ends on the parent's own edge, so it stays open for the grandparent.
        let pieces = cut_at(&[(300, 300), (2 * E + 300, 300)], 3);
        let merged = merge_level(&pieces, EXTENT);

        let chains = &merged[&TileCoord { z: 2, x: 0, y: 0 }];
        assert_eq!(chains.len(), 1);
        assert_eq!(
            chains[0].points.last(),
            Some(&Coord {
                x: EXTENT as Local,
                y: 150
            }),
            "should end exactly on the parent's edge"
        );
    }

    #[test]
    fn chains_meeting_on_the_parents_own_boundary_are_not_joined() {
        // Leaves the tile and comes back at the same point: looks like a join,
        // but both ends are on the parent's right edge, not an internal seam.
        let outbound = Chain {
            feature: 1,
            points: vec![Coord { x: 3000, y: 1000 }, Coord { x: 4096, y: 2000 }],
        };
        let inbound = Chain {
            feature: 1,
            points: vec![Coord { x: 4096, y: 2000 }, Coord { x: 3000, y: 3000 }],
        };

        let mut pieces = Pieces::new();
        pieces.insert(
            TileCoord { z: 3, x: 1, y: 0 },
            vec![outbound.clone(), inbound.clone()],
        );

        let merged = merge_level(&pieces, EXTENT);
        let chains = &merged[&TileCoord { z: 2, x: 0, y: 0 }];

        let meeting = Coord {
            x: EXTENT as Local,
            y: 1000,
        };
        assert_eq!(
            chains.len(),
            2,
            "should stay as two chains meeting at {meeting:?} on the parent's \
             own edge, not be spliced into one; got {chains:?}"
        );

        // The same pair on an internal seam does join.
        let on_seam_pair = |x: Local| {
            let mut pieces = Pieces::new();
            pieces.insert(
                TileCoord { z: 3, x: 0, y: 0 },
                vec![
                    Chain {
                        feature: 1,
                        points: vec![Coord { x: 3000, y: 1000 }, Coord { x, y: 2000 }],
                    },
                    Chain {
                        feature: 1,
                        points: vec![Coord { x, y: 2000 }, Coord { x: 3000, y: 3000 }],
                    },
                ],
            );
            merge_level(&pieces, EXTENT)[&TileCoord { z: 2, x: 0, y: 0 }].len()
        };
        // Child 3/0/0's right edge is the parent's internal seam at x = 2048.
        assert_eq!(on_seam_pair(4096), 1, "a seam meeting should join");
    }

    #[test]
    fn merging_a_level_preserves_total_length() {
        let line = [(300, 400), (5 * E, 3 * E), (E, 6 * E), (7 * E, 7 * E)];
        let children = cut_at(&line, 3);
        let parents = merge_level(&children, EXTENT);

        // Halving the coordinates halves every distance.
        let expected = total_length(&children) / 2.0;
        let actual = total_length(&parents);
        let slack = 4.0 * global(&parents).iter().map(|c| c.len()).sum::<usize>() as f64;
        assert!(
            (actual - expected).abs() < slack,
            "merged length {actual:.1} vs expected {expected:.1}, slack {slack:.1}"
        );
    }

    #[test]
    fn a_point_that_rounds_onto_the_far_edge_is_pulled_back_inside() {
        // 8191 in the eastern child is 16383 in the frame, a quarter of 4095.75,
        // which rounds to 4096: the edge, where MapLibre draws nothing.
        let mut pieces = Pieces::new();
        let dot = |x, y| Chain {
            feature: 1,
            points: vec![Coord { x, y }],
        };
        pieces.insert(TileCoord { z: 1, x: 1, y: 1 }, vec![dot(8191, 8191)]);
        let parent = merge_level_to(&pieces, 8192, 4096);
        let chains = &parent[&TileCoord { z: 0, x: 0, y: 0 }];
        assert_eq!(chains[0].points, vec![Coord { x: 4095, y: 4095 }]);
    }

    #[test]
    fn merging_up_agrees_with_cutting_at_the_coarser_zoom() {
        // Going up a level must land where cutting the source directly would.
        let mut next = {
            let mut seed = 0xBADA55E1FBABEu64;
            move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            }
        };

        for case in 0..500 {
            let span = 8 * E;
            let vertices = 2 + (next() % 5) as usize;
            // Even coordinates so halving is exact; otherwise truncating `/ 2`
            // here and rounding in the merge would differ by a unit.
            let mut xs: Vec<i64> = (0..vertices)
                .map(|_| ((next() % span as u64) as i64 / 2) * 2)
                .collect();
            xs.sort_unstable();
            xs.dedup();
            if xs.len() < 2 {
                continue;
            }
            let line: Vec<(i64, i64)> = xs
                .into_iter()
                .map(|x| (x, ((next() % span as u64) as i64 / 2) * 2))
                .collect();

            let merged = merge_level(&cut_at(&line, 3), EXTENT);
            let direct = cut_at(
                &line
                    .iter()
                    .map(|&(x, y)| (x / 2, y / 2))
                    .collect::<Vec<_>>(),
                2,
            );

            // Every tile with substantial geometry must appear both ways. Tiles
            // a line merely grazes can differ, since halving is accurate only to
            // a unit.
            const SUBSTANTIAL: f64 = 4.0;
            let substantial = |pieces: &Pieces| -> Vec<TileCoord> {
                pieces
                    .iter()
                    .filter(|(coord, chains)| {
                        let mut one = Pieces::new();
                        one.insert(**coord, (*chains).clone());
                        total_length(&one) > SUBSTANTIAL
                    })
                    .map(|(coord, _)| *coord)
                    .collect()
            };
            assert_eq!(
                substantial(&merged),
                substantial(&direct),
                "case {case}: different tiles hold real geometry"
            );

            let (a, b) = (total_length(&merged), total_length(&direct));
            let slack = 4.0 * global(&merged).iter().map(|c| c.len()).sum::<usize>() as f64;
            assert!(
                (a - b).abs() < slack,
                "case {case}: merged length {a:.1} vs direct {b:.1}, slack {slack:.1}"
            );
        }
    }
}
