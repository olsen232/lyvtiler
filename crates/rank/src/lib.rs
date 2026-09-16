//! The drop order: a total order over a layer's features.
//!
//! Any prefix of a total order is a valid keep-set, so "a feature never
//! vanishes as you zoom in" comes free: each zoom keeps a prefix, and a shorter
//! prefix is inside a longer one.
//!
//! Rankers sit behind the [`Ranker`] trait, chosen by flag, with contract tests
//! in `tests/contract.rs` written against the trait.
//!
//! The default, [`Evenstar`]: order each maxzoom tile's features by a lottery
//! weighted to the large ones, then visit the tiles in a fixed pseudo-random
//! order taking each tile's next unranked feature, round after round. Every
//! round takes one feature from every tile that has any left, so no region is
//! emptied — a global sort by area would give a city's small parcels up to one
//! lake. Large features still rise: one spanning `k` tiles gets `k` chances in
//! the first round, a rank of about `(T+1)/(k+1)`. See §8 of the design notes
//! and `docs/rank_sim.py`.

use std::collections::{BTreeMap, HashMap};

use lyv_core::TileCoord;

/// How big a feature is: area for polygons, length for lines, nothing for
/// points. The three kinds race separately, so an area is never compared with
/// a length. Zero-area polygons are dropped at read time, so zero area means a
/// line or a point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Size {
    pub area: f64,
    pub length: f64,
}

/// Polygons, lines, points.
const TIERS: usize = 3;

impl Size {
    /// Which race this is in: polygons, lines or points.
    fn tier(self) -> usize {
        if self.area > 0.0 {
            0
        } else if self.length > 0.0 {
            1
        } else {
            2
        }
    }

    /// Weight in its tier's draw: size squared, so a polygon twice the size of
    /// another wins four times in five, not two in three. Points weigh equally.
    fn weight(self) -> f64 {
        let size = if self.area > 0.0 {
            self.area
        } else if self.length > 0.0 {
            self.length
        } else {
            return 1.0;
        };
        size * size
    }

    pub fn polygon(area: f64) -> Self {
        Self { area, length: 0.0 }
    }

    pub fn line(length: f64) -> Self {
        Self { area: 0.0, length }
    }

    pub fn point() -> Self {
        Self {
            area: 0.0,
            length: 0.0,
        }
    }
}

/// Which features are in which tile, and how big each is. Built during the
/// cut; sizes are the whole feature's, not a clipped piece's, so a large
/// polygon doesn't rank low in a tile that caught only its corner.
#[derive(Default, Debug)]
pub struct Layer {
    tiles: BTreeMap<TileCoord, Vec<u64>>,
    sizes: HashMap<u64, Size>,
}

impl Layer {
    /// Record that `id` occupies `tile`. Repeats aren't filtered — a feature can
    /// leave several pieces in a tile, and filtering here would be quadratic —
    /// the sort in `draw` deduplicates.
    pub fn add(&mut self, tile: TileCoord, id: u64, size: Size) {
        self.sizes.insert(id, size);
        self.tiles.entry(tile).or_default().push(id);
    }

    /// Counts for the memory report (`lyv::footprint`): buckets, ids, id
    /// capacity and sizes. Capacity above ids means [`shrink`](Self::shrink)
    /// was missed.
    pub fn footprint(&self) -> (usize, usize, usize, usize) {
        let ids = self.tiles.values().map(Vec::len).sum();
        let capacity = self.tiles.values().map(Vec::capacity).sum();
        (self.tiles.len(), ids, capacity, self.sizes.capacity())
    }

    /// Hand back the capacity the buckets grew into (about 1.36× by doubling),
    /// once nothing more will be added. It's held until ranking, which can be
    /// the run's peak.
    pub fn shrink(&mut self) {
        for ids in self.tiles.values_mut() {
            ids.shrink_to_fit();
        }
        self.sizes.shrink_to_fit();
    }

    /// Absorb another layer's buckets, for building a layer across threads.
    /// Order doesn't matter: `draw` sorts and deduplicates each tile's list.
    pub fn merge(&mut self, other: Layer) {
        self.sizes.extend(other.sizes);
        for (tile, ids) in other.tiles {
            match self.tiles.entry(tile) {
                std::collections::btree_map::Entry::Occupied(mut held) => {
                    held.get_mut().extend(ids);
                }
                // Moved, not copied: most tiles are touched by one share.
                std::collections::btree_map::Entry::Vacant(empty) => {
                    empty.insert(ids);
                }
            }
        }
    }

    pub fn features(&self) -> usize {
        self.sizes.len()
    }

    pub fn tiles(&self) -> usize {
        self.tiles.len()
    }

    /// Each tile's features in drawn order, the tiles in visiting order.
    /// Shared by every ranker.
    fn sorted_tiles(&self) -> Vec<Vec<u64>> {
        let mut order: Vec<(&TileCoord, &Vec<u64>)> = self.tiles.iter().collect();
        // Ordered by a hash of the coordinate, not a shuffle, so a tile's
        // position doesn't depend on which other tiles exist: adding one
        // doesn't reorder the rest.
        order.sort_by_key(|(coord, _)| scramble(key_of(coord)));

        order
            .into_iter()
            .map(|(coord, ids)| self.draw(key_of(coord), ids))
            .collect()
    }

    /// Order one tile's features by a lottery weighted to the large ones.
    ///
    /// A sort would make the largest feature certain to survive and the
    /// smallest certain not to; a neighbourhood of small things sharing a tile
    /// with something big would be wiped. The draw gives each a chance in
    /// proportion to its weight.
    ///
    /// The exponential race: key `E / w`, `E` from the unit exponential, taken
    /// ascending, gives `P(first) = w / Σw` at every place (Efraimidis and
    /// Spirakis, 2006). Each tile draws independently, so a feature in ten tiles
    /// gets ten chances — which is how evenstar keeps preferring big features.
    fn draw(&self, tile: u64, ids: &[u64]) -> Vec<u64> {
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();

        // One queue per kind, interleaved below.
        let mut tiers: [Vec<(f64, u64)>; TIERS] = Default::default();
        for id in ids {
            let size = self.sizes.get(&id).copied().unwrap_or_else(Size::point);
            let key = exponential(tile, id) / size.weight();
            tiers[size.tier()].push((key, id));
        }
        for tier in tiers.iter_mut() {
            // Reversed so the smallest key, the race's winner, pops first.
            tier.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            tier.reverse();
        }

        let mut out = Vec::with_capacity(tiers.iter().map(Vec::len).sum());
        for step in 0.. {
            let live: Vec<usize> = (0..TIERS).filter(|&i| !tiers[i].is_empty()).collect();
            let Some(&pick) = live.first() else { break };
            // Uniformly among the kinds still holding anything. Mixed-kind
            // layers are rare.
            let pick = if live.len() == 1 {
                pick
            } else {
                live[(scramble(tile ^ scramble(step)) % live.len() as u64) as usize]
            };
            out.push(tiers[pick].pop().expect("picked a non-empty tier").1);
        }
        out
    }

    /// The size as one number. Works because the tiers never overlap.
    fn order_of(&self, id: u64) -> f64 {
        match self.sizes.get(&id) {
            Some(size) if size.area > 0.0 => size.area,
            // Lines below every polygon, points below every line.
            Some(size) => -1.0 / (1.0 + size.length),
            None => f64::NEG_INFINITY,
        }
    }
}

/// What a single tile is allowed to be. Zero means no limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub bytes: usize,
    pub features: usize,
}

/// The worst tile in a finished level, per limit. They needn't be the same
/// tile; either breaching is enough.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Worst {
    pub bytes: usize,
    pub features: usize,
}

impl Worst {
    /// The worse of two, limit by limit.
    pub fn max(self, other: Self) -> Self {
        Self {
            bytes: self.bytes.max(other.bytes),
            features: self.features.max(other.features),
        }
    }
}

impl Limits {
    /// How far past the limits the worst tile is, at least 1: the larger of the
    /// two breaches.
    pub fn overflow(&self, worst: Worst) -> f64 {
        let ratio = |amount: usize, limit: usize| {
            if limit == 0 {
                // Zero means no limit.
                1.0
            } else {
                amount as f64 / limit as f64
            }
        };
        ratio(worst.bytes, self.bytes)
            .max(ratio(worst.features, self.features))
            .max(1.0)
    }
}

/// Narrow a keep-count by an overflow factor: four times over keeps a quarter.
///
/// Never widens — a longer prefix would bring back features a deeper zoom
/// dropped. Never reaches zero — one feature can exceed a limit by itself, and
/// a blank tile is worse than a heavy one.
///
/// It under-corrects, because the ranking keeps large features and drops cheap
/// ones; the next level up measures the result and corrects from there.
pub fn narrow(cutoff: u32, overflow: f64) -> u32 {
    // Written so a NaN overflow means no narrowing.
    if !matches!(
        overflow.partial_cmp(&1.0),
        Some(std::cmp::Ordering::Greater)
    ) {
        return cutoff;
    }
    let kept = (f64::from(cutoff) / overflow).floor();
    (kept as u32).max(1)
}

/// Produces a total order over a layer's features.
pub trait Ranker {
    /// For flags, messages and test failures.
    fn name(&self) -> &'static str;

    /// Rank every feature, `0` most important: dense, each feature exactly once,
    /// ranks `0..n`.
    fn rank(&self, layer: &Layer) -> HashMap<u64, u32>;
}

/// One feature from every tile, then one more from every tile, and so on. The
/// default.
///
/// "Even" because no tile is skipped; "star" because within a tile it's a
/// weighted lottery, so large features usually win but not always.
#[derive(Debug)]
pub struct Evenstar;

impl Ranker for Evenstar {
    fn name(&self) -> &'static str {
        "evenstar"
    }

    fn rank(&self, layer: &Layer) -> HashMap<u64, u32> {
        let tiles = layer.sorted_tiles();
        // Where each tile has got to. Features already ranked from another tile
        // are skipped, so every round takes one new feature per tile.
        let mut cursor = vec![0usize; tiles.len()];
        let mut ranks = HashMap::with_capacity(layer.features());
        let mut next = 0u32;

        let mut emitted = true;
        while emitted {
            emitted = false;
            for (index, ids) in tiles.iter().enumerate() {
                while cursor[index] < ids.len() && ranks.contains_key(&ids[cursor[index]]) {
                    cursor[index] += 1;
                }
                if cursor[index] < ids.len() {
                    ranks.insert(ids[cursor[index]], next);
                    next += 1;
                    emitted = true;
                }
            }
        }
        ranks
    }
}

/// Every feature sorted by size, ignoring position: what evenstar argues
/// against, kept so the comparison can be made on real data.
#[derive(Debug)]
pub struct ByArea;

impl Ranker for ByArea {
    fn name(&self) -> &'static str {
        "by-area"
    }

    fn rank(&self, layer: &Layer) -> HashMap<u64, u32> {
        let mut ids: Vec<u64> = layer.sizes.keys().copied().collect();
        ids.sort_by(|a, b| {
            layer
                .order_of(*b)
                .total_cmp(&layer.order_of(*a))
                .then(scramble(*b).cmp(&scramble(*a)))
        });
        ids.into_iter()
            .enumerate()
            .map(|(rank, id)| (id, rank as u32))
            .collect()
    }
}

/// Choose a ranker by name, for the CLI.
pub fn by_name(name: &str) -> Option<Box<dyn Ranker>> {
    match name {
        "evenstar" => Some(Box::new(Evenstar)),
        "by-area" => Some(Box::new(ByArea)),
        _ => None,
    }
}

/// The names `by_name` accepts, in the order a help message should list them.
pub const NAMES: &[&str] = &["evenstar", "by-area"];

/// A tile coordinate as one number, so it can be hashed.
fn key_of(coord: &TileCoord) -> u64 {
    // Includes zoom, so the key is a property of the address.
    (u64::from(coord.z) << 58) | (u64::from(coord.x) << 29) | u64::from(coord.y)
}

/// A draw from the unit exponential, `-ln(U)`, seeded by tile and feature: each
/// tile draws independently, and runs agree.
fn exponential(tile: u64, id: u64) -> f64 {
    // The whole 53-bit mantissa; `+ 1` keeps it off zero.
    let bits = scramble(tile ^ scramble(id)) >> 11;
    let uniform = (bits + 1) as f64 / 9_007_199_254_740_993.0;
    -uniform.ln()
}

/// SplitMix64's finalizer. Not a hashmap hasher, because its output decides
/// what survives and must be the same across runs, machines and versions.
fn scramble(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scramble_is_a_fixed_function_not_a_random_one() {
        // It decides which features survive, so changing it changes every
        // tileset.
        assert_eq!(scramble(0), 16_294_208_416_658_607_535);
        assert_eq!(scramble(1), 10_451_216_379_200_822_465);
    }

    #[test]
    fn tile_keys_do_not_collide_across_the_grid() {
        let a = key_of(&TileCoord { z: 14, x: 1, y: 0 });
        let b = key_of(&TileCoord { z: 14, x: 0, y: 1 });
        let c = key_of(&TileCoord {
            z: 14,
            x: 0,
            y: 16_383,
        });
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn a_size_tier_never_overlaps_the_one_above_it() {
        let layer = {
            let mut layer = Layer::default();
            let tile = TileCoord { z: 0, x: 0, y: 0 };
            layer.add(tile, 1, Size::polygon(f64::MIN_POSITIVE));
            layer.add(tile, 2, Size::line(f64::MAX));
            layer.add(tile, 3, Size::point());
            layer
        };
        assert!(layer.order_of(1) > layer.order_of(2));
        assert!(layer.order_of(2) > layer.order_of(3));
    }
}
