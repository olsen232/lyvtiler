//! What a tile's geometry costs, without encoding it, checked against
//! `encode_chains`.

use geo_types::Coord;
use lyv_core::{Chain, Local};
use lyv_mvt::{encode_chains, geometry_cost};

const EXTENT: u32 = 4096;

/// A chain of `points` vertices wandering across the tile in steps of a few
/// hundred units, like real geometry at tile resolution.
fn wander(feature: u64, points: usize, seed: Local) -> Chain {
    let mut x = 100 + seed * 7;
    let mut y = 200 + seed * 11;
    Chain {
        feature,
        points: (0..points)
            .map(|i| {
                x = (x + 137 + (i as Local % 13) * 29) % 4000;
                y = (y + 211 + (i as Local % 7) * 43) % 4000;
                Coord { x, y }
            })
            .collect(),
    }
}

fn ratio(chains: &[Chain]) -> f64 {
    geometry_cost(chains) as f64 / encode_chains("l", EXTENT, chains).len() as f64
}

#[test]
fn an_empty_tile_costs_nothing() {
    assert_eq!(geometry_cost(&[]), 0);
}

#[test]
fn the_estimate_is_within_a_quarter_of_the_encoded_size() {
    // Loose on purpose: geometry is the small half, so only the order of
    // magnitude matters.
    for (name, chains) in [
        ("one long chain", vec![wander(1, 5_000, 0)]),
        (
            "many short chains",
            (0..500)
                .map(|i| wander(i, 12, i as Local))
                .collect::<Vec<_>>(),
        ),
        (
            "one feature in many pieces",
            (0..200).map(|i| wander(7, 20, i)).collect::<Vec<_>>(),
        ),
        (
            "a mix",
            (0..300)
                .map(|i| wander(i, 4 + (i as usize % 50), i as Local))
                .collect::<Vec<_>>(),
        ),
    ] {
        let ratio = ratio(&chains);
        assert!(
            (0.75..=1.25).contains(&ratio),
            "{name}: estimated {} against encoded {}, a factor of {ratio:.2}",
            geometry_cost(&chains),
            encode_chains("l", EXTENT, &chains).len()
        );
    }
}

#[test]
fn more_vertices_cost_more() {
    assert!(geometry_cost(&[wander(1, 400, 0)]) > geometry_cost(&[wander(1, 40, 0)]));
}

#[test]
fn splitting_one_feature_into_pieces_costs_more_than_leaving_it_whole() {
    // More pieces must cost more, or the probe would think cutting finer is
    // free.
    let whole = vec![wander(1, 200, 0)];
    let pieces: Vec<Chain> = (0..10).map(|i| wander(1, 20, i)).collect();
    assert!(geometry_cost(&pieces) > geometry_cost(&whole));
}
