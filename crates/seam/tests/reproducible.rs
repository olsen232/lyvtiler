//! Welding must not depend on hash-map iteration order.
//!
//! The order seam edges are bucketed in decides where `reassemble` starts each
//! ring. Nothing needs any particular start, but it must be the same every run:
//! the start vertex is encoded into the tile, and it's the vertex simplification
//! keeps. A `HashMap` made 6 tiles in 5,148 differ between runs. It only shows
//! with more than one seam line per axis, as in the buffer.

use geo_types::Coord;
use lyv_core::{Chain, Local};
use lyv_seam::weld;

fn ring(points: &[(Local, Local)]) -> Vec<Coord<Local>> {
    points.iter().map(|&(x, y)| Coord { x, y }).collect()
}

/// Two unit squares side by side, one feature, every edge on a seam line.
fn two_squares() -> Vec<Chain> {
    vec![
        Chain {
            feature: 1,
            points: ring(&[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)]),
        },
        Chain {
            feature: 1,
            points: ring(&[(100, 0), (200, 0), (200, 100), (100, 100), (100, 0)]),
        },
    ]
}

#[test]
fn the_shared_edge_cancels_and_leaves_one_ring() {
    // The seam at x=100 cancels, leaving the pair's outline. Three vertical
    // lines hold edges, which makes bucket order observable.
    let welded = weld(&two_squares(), &[0, 100, 200], &[0, 100]);
    assert_eq!(welded.len(), 1, "the shared edge should have cancelled");

    let points = &welded[0].points;
    assert_eq!(points.first(), points.last(), "a ring must be closed");
    assert_eq!(
        lyv_seam::double_area(points),
        2 * 200 * 100,
        "the union of two 100x100 squares"
    );
    // Nothing sits on the cancelled seam except where the outline crosses it.
    let on_seam = points.iter().filter(|p| p.x == 100).count();
    assert!(
        on_seam <= 2,
        "the cancelled seam should not survive as geometry: {points:?}"
    );
}

#[test]
fn the_ring_is_identical_whichever_order_the_squares_arrive_in() {
    // Input order mustn't matter either.
    let mut reversed = two_squares();
    reversed.reverse();
    let forward = weld(&two_squares(), &[0, 100, 200], &[0, 100]);
    let backward = weld(&reversed, &[0, 100, 200], &[0, 100]);
    assert_eq!(forward.len(), backward.len());
    assert_eq!(
        lyv_seam::double_area(&forward[0].points),
        lyv_seam::double_area(&backward[0].points),
    );
}

/// The same input welds to the same ring, starting at the same vertex, every
/// time. Every new `HashMap` draws its own iteration order, so repeating the
/// weld tries a fresh order each time: a hash-ordered bucket fails this
/// reliably rather than now and then.
#[test]
fn welding_the_same_input_gives_the_same_ring_every_time() {
    let first = weld(&two_squares(), &[0, 100, 200], &[0, 100]);
    for attempt in 1..200 {
        let again = weld(&two_squares(), &[0, 100, 200], &[0, 100]);
        assert_eq!(again, first, "attempt {attempt} welded differently");
    }
}
