//! Does evenstar keep regions alive that a sort by area empties? Runs both
//! rankers on a real source and reports how much of the map still has
//! anything at each budget.
//!
//! Usage: `cargo run --release -p lyv --example rank_stats -- FILE Z`

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use lyv_core::{Pieces, TileCoord};
use lyv_rank::{ByArea, Evenstar, Layer, Ranker, Size};

const EXTENT: u32 = 4096;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = std::path::PathBuf::from(args.next().expect("a source file"));
    let maxzoom: u32 = args.next().map_or(12, |z| z.parse().unwrap());

    let mut pieces = Pieces::new();
    let mut sizes: HashMap<u64, Size> = HashMap::new();
    lyv_source::read(
        &path,
        maxzoom,
        EXTENT,
        &lyv_source::Selection::default(),
        &mut |_total| {},
        &mut |feature| {
            let mut double_area = 0.0f64;
            let mut length = 0.0f64;
            for part in &feature.parts {
                let Some(origin) = part.first() else { continue };
                for pair in part.windows(2) {
                    let (ax, ay) = ((pair[0].x - origin.x) as f64, (pair[0].y - origin.y) as f64);
                    let (bx, by) = ((pair[1].x - origin.x) as f64, (pair[1].y - origin.y) as f64);
                    double_area += ax * by - bx * ay;
                    length += (bx - ax).hypot(by - ay);
                }
            }
            match feature.kind {
                lyv_source::Kind::Rings => {
                    for ring in &feature.parts {
                        lyv_cut::cut_ring(feature.id, ring, maxzoom, EXTENT, &mut pieces);
                    }
                    sizes.insert(feature.id, Size::polygon((double_area / 2.0).abs()));
                }
                lyv_source::Kind::Lines => {
                    for part in &feature.parts {
                        lyv_cut::cut_linestring(feature.id, part, maxzoom, EXTENT, &mut pieces);
                    }
                    sizes.insert(feature.id, Size::line(length));
                }
                lyv_source::Kind::Points => {
                    for part in &feature.parts {
                        if let Some(at) = part.first() {
                            lyv_cut::cut_point(feature.id, *at, maxzoom, EXTENT, &mut pieces);
                        }
                    }
                    sizes.insert(feature.id, Size::point());
                }
            }
            Ok(())
        },
    )?;

    // Which tile each feature is in, rebuilt here (`Layer` keeps it private).
    let mut in_tile: BTreeMap<TileCoord, HashSet<u64>> = BTreeMap::new();
    let mut layer = Layer::default();
    for (coord, chains) in pieces.iter() {
        for chain in chains {
            let size = sizes
                .get(&chain.feature)
                .copied()
                .unwrap_or_else(Size::point);
            layer.add(*coord, chain.feature, size);
            in_tile.entry(*coord).or_default().insert(chain.feature);
        }
    }

    println!(
        "{} at z{maxzoom}: {} features over {} tiles",
        path.display(),
        layer.features(),
        layer.tiles()
    );

    // Also a coarser grid, four levels up, closer to what one screen shows.
    let coarse = maxzoom.saturating_sub(4);

    for ranker in [
        Box::new(Evenstar) as Box<dyn Ranker>,
        Box::new(ByArea) as Box<dyn Ranker>,
    ] {
        let ranks = ranker.rank(&layer);
        println!("\n{}", ranker.name());
        println!(
            "{:>10}  {:>12}  {:>14}  {:>16}  {:>12}",
            "per tile", "features", "tiles with any", "cells with any", "emptiest cell"
        );

        // Swept in features per tile, which is what a zoom's budget fixes. The
        // rankers agree once tiles can afford hundreds, and diverge near one each.
        let total = layer.features();
        for per_tile_budget in [1usize, 4, 16, 64, 256, 1024] {
            let keep = (per_tile_budget * layer.tiles()).min(total).max(1) as u32;
            let mut per_tile: BTreeMap<TileCoord, usize> = BTreeMap::new();
            let mut per_cell: BTreeMap<(u32, u32), usize> = BTreeMap::new();
            for (coord, ids) in &in_tile {
                let kept = ids.iter().filter(|id| ranks[id] < keep).count();
                per_tile.insert(*coord, kept);
                let shift = maxzoom - coarse;
                *per_cell
                    .entry((coord.x >> shift, coord.y >> shift))
                    .or_default() += kept;
            }
            let tiles_alive = per_tile.values().filter(|n| **n > 0).count();
            let cells_alive = per_cell.values().filter(|n| **n > 0).count();
            println!(
                "{:>10}  {:>12}  {:>6} of {:>4}  {:>8} of {:>4}  {:>12}",
                per_tile_budget,
                keep,
                tiles_alive,
                per_tile.len(),
                cells_alive,
                per_cell.len(),
                per_cell.values().min().copied().unwrap_or(0),
            );
        }
    }
    Ok(())
}
