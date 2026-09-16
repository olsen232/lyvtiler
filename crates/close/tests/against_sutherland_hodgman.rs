//! Cut polygons into tiles, close the pieces, and compare each tile's region
//! with Sutherland-Hodgman — correct and simple, but O(vertices × tiles), so
//! only an oracle. `docs/close.py` is the prototype of this comparison.

use geo_types::Coord;
use lyv_close::close_rings;
use lyv_core::{Chain, Local, Pieces};
use lyv_cut::cut_ring;

const EXTENT: u32 = 4096;
const E: i64 = EXTENT as i64;
const Z: u32 = 2;

/// Doubled shoelace area. Positive is clockwise on screen, consistent with MVT.
fn double_area(ring: &[Coord<i64>]) -> i64 {
    let mut total = 0i64;
    for pair in ring.windows(2) {
        total += pair[0].x * pair[1].y - pair[1].x * pair[0].y;
    }
    // Closed if the caller didn't.
    if ring.first() != ring.last() {
        let (last, first) = (ring[ring.len() - 1], ring[0]);
        total += last.x * first.y - first.x * last.y;
    }
    total
}

fn double_area_local(ring: &[Coord<Local>]) -> i64 {
    let promoted: Vec<Coord<i64>> = ring
        .iter()
        .map(|p| Coord {
            x: i64::from(p.x),
            y: i64::from(p.y),
        })
        .collect();
    double_area(&promoted)
}

/// The oracle: clip a ring to the axis-aligned box by four half-planes.
fn sutherland_hodgman(ring: &[Coord<f64>], low: Coord<f64>, high: Coord<f64>) -> Vec<Coord<f64>> {
    // (axis, sign, bound): keep points where sign * (coord - bound) >= 0.
    let planes = [
        (0usize, 1.0f64, low.x),
        (0, -1.0, high.x),
        (1, 1.0, low.y),
        (1, -1.0, high.y),
    ];

    let mut output = ring.to_vec();
    if output.first() == output.last() && output.len() > 1 {
        output.pop(); // the oracle works on an implicit ring
    }

    for (axis, sign, bound) in planes {
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

fn oracle_double_area(ring: &[Coord<i64>], tile_x: i64, tile_y: i64) -> f64 {
    let float: Vec<Coord<f64>> = ring
        .iter()
        .map(|p| Coord {
            x: p.x as f64,
            y: p.y as f64,
        })
        .collect();
    let clipped = sutherland_hodgman(
        &float,
        Coord {
            x: (tile_x * E) as f64,
            y: (tile_y * E) as f64,
        },
        Coord {
            x: ((tile_x + 1) * E) as f64,
            y: ((tile_y + 1) * E) as f64,
        },
    );
    if clipped.len() < 3 {
        return 0.0;
    }
    let mut total = 0.0;
    for i in 0..clipped.len() {
        let (a, b) = (clipped[i], clipped[(i + 1) % clipped.len()]);
        total += a.x * b.y - b.x * a.y;
    }
    total
}

fn prng(mut seed: u64) -> impl FnMut() -> u64 {
    move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    }
}

/// A random star-shaped (so valid) polygon, kept inside the world: the cut
/// wraps anything east or west of it and drops anything north or south, and
/// the oracle would count it where it is.
fn random_polygon(next: &mut impl FnMut() -> u64) -> Vec<Coord<i64>> {
    let span = (1i64 << Z) * E;
    let unit = |n: u64| (n % 1_000_000) as f64 / 1_000_000.0;

    let radius = (0.05 + unit(next()) * 0.4) * span as f64;
    // Centred so it stays inside the world.
    let cx = radius + unit(next()) * (span as f64 - 2.0 * radius);
    let cy = radius + unit(next()) * (span as f64 - 2.0 * radius);
    let count = 3 + (next() % 6) as usize;

    let mut angles: Vec<f64> = (0..count)
        .map(|_| unit(next()) * std::f64::consts::TAU)
        .collect();
    angles.sort_by(f64::total_cmp);

    let mut ring: Vec<Coord<i64>> = angles
        .into_iter()
        .map(|a| Coord {
            x: (cx + radius * a.cos()).round() as i64,
            y: (cy + radius * a.sin()).round() as i64,
        })
        .collect();
    ring.dedup();

    // Exteriors clockwise on screen: positive area.
    if double_area(&ring) < 0 {
        ring.reverse();
    }
    let span = (1i64 << Z) * E;
    assert!(
        ring.iter()
            .all(|p| (0..=span).contains(&p.x) && (0..=span).contains(&p.y)),
        "the generator must stay inside the world: {ring:?}"
    );
    ring
}

#[test]
fn every_tile_covers_the_region_the_oracle_says_it_should() {
    let mut next = prng(0xBADA55E1FBABE);

    let mut tested = 0usize;
    let mut multi_chain = 0usize;
    let mut skipped_covered = 0usize;

    for case in 0..1500 {
        let ring = random_polygon(&mut next);
        if ring.len() < 3 || double_area(&ring) == 0 {
            continue;
        }

        let mut pieces = Pieces::new();
        cut_ring(1, &ring, Z, EXTENT, &mut pieces);

        for (coord, chains) in &pieces {
            let closed = close_rings(chains, EXTENT);

            if chains.len() >= 2 {
                multi_chain += 1;
            }

            for c in &closed {
                assert!(
                    c.points.len() >= 4 && c.points.first() == c.points.last(),
                    "case {case}: tile {coord:?} produced an unclosed ring {:?}",
                    c.points
                );
                for p in &c.points {
                    assert!(
                        (0..=EXTENT as Local).contains(&p.x)
                            && (0..=EXTENT as Local).contains(&p.y),
                        "case {case}: {p:?} escaped tile {coord:?}"
                    );
                }
            }

            let ours: i64 = closed.iter().map(|c| double_area_local(&c.points)).sum();
            let theirs = oracle_double_area(&ring, i64::from(coord.x), i64::from(coord.y));

            // Each crossing rounds by up to half a unit on one axis, scaled by
            // the tile side in doubled area.
            let slack =
                4.0 * E as f64 * (1 + closed.iter().map(|c| c.points.len()).sum::<usize>()) as f64;
            assert!(
                (ours as f64 - theirs).abs() <= slack,
                "case {case}: tile {coord:?} covers {ours} but the oracle says \
                 {theirs:.0} (slack {slack:.0})\n  ring {ring:?}\n  chains {chains:?}\n  \
                 closed {closed:?}"
            );
            tested += 1;
        }

        // Swallowed tiles have no chains; that's coverage's job.
        let span = 1i64 << Z;
        skipped_covered += (span * span) as usize - pieces.len();
    }

    println!(
        "compared {tested} tiles, {multi_chain} of them multi-chain; \
         {skipped_covered} tile slots had no chains"
    );
    assert!(tested > 1000, "only {tested} tiles were compared");
    assert!(
        multi_chain > 50,
        "only {multi_chain} multi-chain tiles, so the chain-to-chain join is \
         barely exercised"
    );
}

#[test]
fn a_tile_the_polygon_swallows_whole_produces_nothing_to_close() {
    // A tile wholly covered gets no chains, so closing has nothing to do;
    // coverage fills it.
    // Around the middle four tiles of zoom 2, half a tile out from them.
    let (low, high) = (E / 2, 3 * E + E / 2);
    let ring = vec![
        Coord { x: low, y: low },
        Coord { x: high, y: low },
        Coord { x: high, y: high },
        Coord { x: low, y: high },
    ];

    let mut pieces = Pieces::new();
    cut_ring(1, &ring, Z, EXTENT, &mut pieces);

    for (x, y) in [(1, 1), (1, 2), (2, 1), (2, 2)] {
        assert!(
            !pieces.contains_key(&lyv_core::TileCoord { z: Z, x, y }),
            "swallowed tile {x}/{y} got chains"
        );
    }

    // Closing an empty tile is empty, not an error.
    let nothing: Vec<Chain> = Vec::new();
    assert!(close_rings(&nothing, EXTENT).is_empty());
}
