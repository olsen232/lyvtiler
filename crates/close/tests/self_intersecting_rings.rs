//! What closing does with a self-intersecting ring.
//!
//! Cutting and closing a well-formed ring conserves its area. A ring that
//! crosses itself can leave a piece whose ends are ordered as a hole's while
//! the ring is an exterior, and closing returns the tile's complement. Found on
//! floodplains feature 3256752, drawn as a rectangle over two tiles. `lyv`
//! detects and drops such features (`closing_would_lose_it`).

use geo_types::Coord;
use lyv_close::close_rings;
use lyv_core::{Local, Pieces};
use lyv_cut::cut_ring;

const EXTENT: u32 = 8192;
const E: i64 = EXTENT as i64;
const Z: u32 = 1;

/// Doubled shoelace area. Positive is clockwise on screen, consistent with MVT.
fn double_area(ring: &[Coord<i64>]) -> i128 {
    ring.windows(2)
        .map(|w| i128::from(w[0].x) * i128::from(w[1].y) - i128::from(w[1].x) * i128::from(w[0].y))
        .sum()
}

fn double_area_local(ring: &[Coord<Local>]) -> i128 {
    let promoted: Vec<Coord<i64>> = ring
        .iter()
        .map(|p| Coord {
            x: i64::from(p.x),
            y: i64::from(p.y),
        })
        .collect();
    double_area(&promoted)
}

/// Cut a ring and close every piece; return the pieces' total area and the
/// largest piece.
fn cut_and_close(ring: &[Coord<i64>]) -> (i128, i128) {
    let mut closed = ring.to_vec();
    if closed.first() != closed.last() {
        closed.push(closed[0]);
    }

    let mut pieces = Pieces::default();
    cut_ring(0, &closed, Z, EXTENT, &mut pieces);

    let mut total = 0i128;
    let mut largest = 0i128;
    for chains in pieces.values() {
        for chain in close_rings(chains, EXTENT) {
            let area = double_area_local(&chain.points);
            total += area;
            largest = largest.max(area.abs());
        }
    }
    (total, largest)
}

/// A tall thin clockwise ring just inside the tile boundary, with one
/// left-side vertex crossed past the right side and the boundary. Harmless
/// inside a tile; not across a boundary.
fn ring_whose_left_side_crosses_over() -> Vec<Coord<i64>> {
    let c = |x: i64, y: i64| Coord { x, y };
    vec![
        c(E - 92, 100),
        c(E - 2, 100),
        c(E - 2, 500),
        c(E - 92, 500),
        c(E - 92, 400),
        // Past the right side at E - 2 and the boundary at E.
        c(E + 8, 300),
        c(E - 92, 200),
    ]
}

#[test]
fn closing_a_clean_ring_across_a_boundary_conserves_its_area() {
    // The control: the same shape without the crossover.
    let c = |x: i64, y: i64| Coord { x, y };
    let ring = vec![c(E - 12, 100), c(E + 8, 100), c(E + 8, 400), c(E - 12, 400)];

    let mut closed = ring.clone();
    closed.push(closed[0]);
    let whole = double_area(&closed);
    let (total, _) = cut_and_close(&ring);

    assert_eq!(total, whole, "a clean ring must survive the round trip");
}

#[test]
fn a_self_intersecting_ring_closes_into_more_than_its_tile() {
    // Characterises current behaviour, the signature `lyv` detects: a piece
    // enclosing more than its tile. If closing is ever fixed to handle this,
    // replace this with the conservation assertion above.
    let ring = ring_whose_left_side_crosses_over();
    let mut closed = ring.clone();
    closed.push(closed[0]);
    let whole = double_area(&closed);

    let (total, largest) = cut_and_close(&ring);
    let tile = i128::from(E) * i128::from(E) * 2;

    assert_eq!(whole, 52_000, "the ring encloses very little");
    assert!(
        largest > tile,
        "a piece should have enclosed more than the tile's {tile}, got {largest}"
    );
    assert_ne!(
        total, whole,
        "and the pieces should not have come to the ring's own area"
    );
}
