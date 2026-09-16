//! The contract every ranker must honour, written against the trait so new
//! rankers inherit it: a total order over the features, so any prefix is a
//! usable keep-set.

use lyv_core::TileCoord;
use lyv_rank::{ByArea, Evenstar, Layer, Ranker, Size};

/// Every ranker this crate offers.
fn rankers() -> Vec<Box<dyn lyv_rank::Ranker>> {
    vec![Box::new(Evenstar), Box::new(ByArea)]
}

fn tile(x: u32, y: u32) -> TileCoord {
    TileCoord { z: 3, x, y }
}

/// `tiles` tiles, each holding `per_tile` features of its own, areas
/// descending.
fn spread(tiles: u32, per_tile: u64) -> Layer {
    let mut layer = Layer::default();
    for t in 0..tiles {
        for i in 0..per_tile {
            let id = u64::from(t) * 1000 + i;
            layer.add(tile(t, 0), id, Size::polygon((per_tile - i) as f64));
        }
    }
    layer
}

#[test]
fn every_feature_is_ranked_exactly_once() {
    for ranker in rankers() {
        let layer = spread(5, 4);
        let ranks = ranker.rank(&layer);
        assert_eq!(
            ranks.len(),
            20,
            "{} lost or duplicated features",
            ranker.name()
        );

        let mut seen: Vec<u32> = ranks.values().copied().collect();
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..20).collect::<Vec<u32>>(),
            "{} did not produce a dense total order",
            ranker.name()
        );
    }
}

#[test]
fn a_feature_in_several_tiles_is_ranked_once_not_once_per_tile() {
    // A rank belongs to the feature, not the tile, or a polygon's clipped
    // pieces would be dropped independently.
    for ranker in rankers() {
        let mut layer = Layer::default();
        for t in 0..4 {
            layer.add(tile(t, 0), 7, Size::polygon(100.0));
            layer.add(tile(t, 0), u64::from(t) + 10, Size::polygon(1.0));
        }
        let ranks = ranker.rank(&layer);
        assert_eq!(
            ranks.len(),
            5,
            "{} counted a feature per tile",
            ranker.name()
        );
    }
}

#[test]
fn ranking_is_deterministic() {
    // Two runs must drop the same features.
    for ranker in rankers() {
        let layer = spread(9, 3);
        assert_eq!(
            ranker.rank(&layer),
            ranker.rank(&layer),
            "{} is not deterministic",
            ranker.name()
        );
    }
}

#[test]
fn an_empty_layer_ranks_nothing() {
    for ranker in rankers() {
        assert!(ranker.rank(&Layer::default()).is_empty());
    }
}

#[test]
fn evenstar_takes_one_feature_from_every_tile_before_any_tile_gets_two() {
    // No tile is skipped however small its features. Tile 0's features dwarf
    // everyone else's; a global sort would take all four first.
    let mut layer = Layer::default();
    for i in 0..4 {
        layer.add(tile(0, 0), i, Size::polygon(1_000_000.0 - i as f64));
    }
    for t in 1..6 {
        layer.add(tile(t, 0), u64::from(t) + 100, Size::polygon(1.0));
    }

    let ranks = Evenstar.rank(&layer);
    let first_round: Vec<u64> = (0..6)
        .map(|r| *ranks.iter().find(|&(_, &v)| v == r).unwrap().0)
        .collect();

    // Six tiles: the first six ranks are one from each.
    let mut tiles_represented: Vec<u64> = first_round
        .iter()
        .map(|id| if *id < 100 { 0 } else { id - 100 })
        .collect();
    tiles_represented.sort_unstable();
    assert_eq!(
        tiles_represented,
        vec![0, 1, 2, 3, 4, 5],
        "the first round should hold one feature from each of the six tiles, got {first_round:?}"
    );
}

#[test]
fn the_more_tiles_a_feature_spans_the_better_it_ranks() {
    // Span alone favours a feature: in k tiles it gets k chances per round.
    // Every feature here is the same size, so span is all that's at work.
    // Measured over many features, since one can be unlucky.
    const TILES: u32 = 200;
    let mut layer = Layer::default();
    for t in 0..TILES {
        for i in 0..10 {
            layer.add(tile(t, 0), u64::from(t) * 1000 + i, Size::polygon(1.0));
        }
    }

    let spans = [1u32, 2, 5, 20];
    let mut wide: Vec<(u32, Vec<u64>)> = Vec::new();
    let mut next_id = 900_000u64;
    for span in spans {
        let mut ids = Vec::new();
        for n in 0..25 {
            let id = next_id;
            next_id += 1;
            // Spread the occupied tiles out rather than clumping them.
            for s in 0..span {
                let t = (n * 7 + s * 61) % TILES;
                layer.add(tile(t, 0), id, Size::polygon(1.0));
            }
            ids.push(id);
        }
        wide.push((span, ids));
    }

    let ranks = Evenstar.rank(&layer);
    let means: Vec<(u32, f64)> = wide
        .iter()
        .map(|(span, ids)| {
            let total: u64 = ids.iter().map(|id| u64::from(ranks[id])).sum();
            (*span, total as f64 / ids.len() as f64)
        })
        .collect();

    for pair in means.windows(2) {
        let (fewer, worse) = pair[0];
        let (more, better) = pair[1];
        assert!(
            better < worse,
            "spanning {more} tiles should rank better than spanning {fewer}, \
             got mean {better:.0} against {worse:.0} (all of them: {means:?})"
        );
    }
}

/// How often each tile's first pick is `wanted`, over `tiles` tiles holding the
/// same contest.
fn head_share(tiles: u32, wanted: u64, contest: &[(u64, Size)]) -> f64 {
    let mut layer = Layer::default();
    for t in 0..tiles {
        for (id, size) in contest {
            // Different ids per tile, or it's one draw, not many.
            layer.add(tile(t, 0), id + u64::from(t) * 10, *size);
        }
    }
    let ranks = Evenstar.rank(&layer);

    // The first `tiles` ranks are round one.
    let wins = (0..tiles)
        .filter(|t| {
            let mine: Vec<u64> = contest
                .iter()
                .map(|(id, _)| id + u64::from(*t) * 10)
                .collect();
            let best = mine.iter().min_by_key(|id| ranks[id]).unwrap();
            *best == wanted + u64::from(*t) * 10
        })
        .count();
    wins as f64 / f64::from(tiles)
}

#[test]
fn a_polygon_twice_the_size_wins_four_times_in_five() {
    // Weight is size squared: 2:1 in size is four wins in five. The smaller
    // wins sometimes, where a sort would mean never.
    let share = head_share(4000, 1, &[(1, Size::polygon(2.0)), (2, Size::polygon(1.0))]);
    assert!(
        (0.76..0.84).contains(&share),
        "the larger polygon headed its tile {share:.3} of the time, expected about 0.8"
    );
}

#[test]
fn equal_polygons_split_evenly() {
    let share = head_share(4000, 1, &[(1, Size::polygon(5.0)), (2, Size::polygon(5.0))]);
    assert!(
        (0.46..0.54).contains(&share),
        "one of two equal polygons headed its tile {share:.3} of the time"
    );
}

#[test]
fn a_much_larger_polygon_almost_always_wins() {
    // Ten times the area is a hundred times the weight: usually, not always.
    let share = head_share(
        4000,
        1,
        &[(1, Size::polygon(10.0)), (2, Size::polygon(1.0))],
    );
    assert!(
        (0.96..0.995).contains(&share),
        "ten times the area headed its tile {share:.3} of the time, expected about 0.99"
    );
}

#[test]
fn each_kind_gets_an_equal_share_of_the_draw() {
    // Kinds take turns, a third each, however lopsided their sizes — separate
    // races, not one.
    let contest = [
        (1, Size::polygon(0.001)),
        (2, Size::line(1_000_000.0)),
        (3, Size::point()),
    ];
    for wanted in [1, 2, 3] {
        let share = head_share(4000, wanted, &contest);
        assert!(
            (0.30..0.37).contains(&share),
            "kind {wanted} headed its tile {share:.3} of the time, expected about a third"
        );
    }
}

#[test]
fn the_tile_order_does_not_depend_on_which_other_tiles_exist() {
    // Visiting order is a hash of the tile coordinate, so adding a tile leaves
    // the others' relative order alone.
    let order = |tiles: &[u32]| -> Vec<u32> {
        let mut layer = Layer::default();
        for &t in tiles {
            layer.add(tile(t, 0), u64::from(t), Size::polygon(1.0));
        }
        let ranks = Evenstar.rank(&layer);
        let mut by_rank: Vec<(u32, u32)> = ranks.iter().map(|(id, r)| (*r, *id as u32)).collect();
        by_rank.sort_unstable();
        by_rank.into_iter().map(|(_, id)| id).collect()
    };

    let before = order(&[1, 2, 3, 4, 5]);
    let after: Vec<u32> = order(&[1, 2, 3, 4, 5, 99])
        .into_iter()
        .filter(|t| *t != 99)
        .collect();
    assert_eq!(
        before, after,
        "adding a tile reordered the tiles that were already there"
    );
}

/// Building a layer in shares and merging them ranks exactly as building it
/// whole — what the parallel ranking relies on.
mod merging {
    use lyv_core::TileCoord;
    use lyv_rank::{Layer, Size, by_name};

    fn tile(x: u32, y: u32) -> TileCoord {
        TileCoord { z: 14, x, y }
    }

    /// Features span several tiles and tiles hold several features, so a merge
    /// has overlap to get wrong.
    fn populate(layer: &mut Layer, ids: std::ops::Range<u64>) {
        for id in ids {
            let size = Size {
                area: (id % 7 + 1) as f64 * 100.0,
                length: 0.0,
            };
            for step in 0..(id % 3 + 1) {
                layer.add(
                    tile((id % 5) as u32 + step as u32, (id % 4) as u32),
                    id,
                    size,
                );
            }
        }
    }

    #[test]
    fn one_piece_and_three_shares_rank_identically() {
        let mut whole = Layer::default();
        populate(&mut whole, 0..60);

        let mut merged = Layer::default();
        for share in [0..20, 20..40, 40..60] {
            let mut part = Layer::default();
            populate(&mut part, share);
            merged.merge(part);
        }

        assert_eq!(whole.tiles(), merged.tiles(), "same tiles");
        assert_eq!(whole.features(), merged.features(), "same features");
        for name in lyv_rank::NAMES {
            let ranker = by_name(name).unwrap();
            assert_eq!(
                ranker.rank(&whole),
                ranker.rank(&merged),
                "{name} disagreed between one piece and three shares"
            );
        }
    }

    #[test]
    fn the_order_shares_are_merged_in_does_not_matter() {
        // Threads finish in any order.
        let build = |shares: [std::ops::Range<u64>; 3]| {
            let mut layer = Layer::default();
            for share in shares {
                let mut part = Layer::default();
                populate(&mut part, share);
                layer.merge(part);
            }
            by_name("evenstar").unwrap().rank(&layer)
        };
        assert_eq!(
            build([0..20, 20..40, 40..60]),
            build([40..60, 0..20, 20..40])
        );
    }

    #[test]
    fn merging_an_empty_share_changes_nothing() {
        // Shares can be empty when there are fewer features than threads.
        let mut layer = Layer::default();
        populate(&mut layer, 0..10);
        let before = by_name("evenstar").unwrap().rank(&layer);
        layer.merge(Layer::default());
        assert_eq!(by_name("evenstar").unwrap().rank(&layer), before);
    }
}
