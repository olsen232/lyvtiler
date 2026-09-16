//! Names the structures behind a memory peak, for `--memory`.
//!
//! `lyv_alloc` counts live bytes but can't say whose. A few structures matter
//! here, so naming them and subtracting from the total gets most of what a heap
//! profiler would. [`Report::show`] always prints the residual: 95% explained
//! is an answer, 40% says look elsewhere.
//!
//! Counts, `Vec`s and `HashMap`s are exact. `BTreeMap` is an estimate (±30%),
//! the only one; see `btree_map_bytes`.

/// Heap bytes behind a `Vec<T>`: its capacity, not its length, which would
/// understate by up to a third and hide what `shrink_the_cut` saves.
pub fn vec_bytes<T>(capacity: usize) -> usize {
    capacity * size_of::<T>()
}

/// Heap bytes behind a `HashMap<K, V>`. hashbrown has a power-of-two bucket
/// count, one control byte per bucket, and fills to seven eighths, so
/// `capacity * 8 / 7` recovers the buckets exactly. (Rounding up after adding
/// anything doubles the answer.)
pub fn hash_map_bytes<K, V>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    // Under eight, hashbrown uses a fixed four or eight buckets.
    let buckets = if capacity < 8 {
        if capacity < 4 { 4 } else { 8 }
    } else {
        (capacity * 8 / 7).next_power_of_two()
    };
    buckets * (size_of::<(K, V)>() + 1)
}

/// How full a `BTreeMap` node is assumed to be. Nodes hold 5–11 entries
/// (B = 6), depending on insertion order, which nothing exposes; eight is the
/// middle. Hence ±30%.
const NODE_FILL: usize = 8;

/// Heap bytes behind a `BTreeMap<K, V>`, estimated (see `NODE_FILL`): a
/// 16-byte leaf header plus eleven keys and values, and internal nodes (twelve
/// child pointers) at about a ninth of the total.
pub fn btree_map_bytes<K, V>(len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    const CAPACITY: usize = 11;
    let leaf = 16 + CAPACITY * (size_of::<K>() + size_of::<V>());
    let nodes = len.div_ceil(NODE_FILL);
    // Internal nodes: each level a NODE_FILL-th of the one below.
    let internal = nodes / (NODE_FILL - 1);
    nodes * leaf + internal * (leaf + 12 * 8)
}

/// One named structure and what it costs.
pub struct Item {
    what: &'static str,
    bytes: usize,
    detail: String,
}

impl Item {
    pub fn new(what: &'static str, bytes: usize, detail: String) -> Self {
        Self {
            what,
            bytes,
            detail,
        }
    }
}

/// What the big structures cost at one moment, against what is actually live.
pub struct Report {
    when: &'static str,
    live: usize,
    items: Vec<Item>,
}

impl Report {
    /// Gather a report, or `None` if nothing counts allocations — without a
    /// total, there's no residual to show.
    pub fn at(when: &'static str, items: Vec<Item>) -> Option<Self> {
        Some(Self {
            when,
            live: lyv_progress::live()?,
            items,
        })
    }

    /// Print it, largest first.
    pub fn show(&self, progress: &lyv_progress::Progress) {
        let accounted: usize = self.items.iter().map(|item| item.bytes).sum();
        progress.say(&format!("  holding {} after {}", mb(self.live), self.when));
        let mut ranked: Vec<&Item> = self.items.iter().collect();
        ranked.sort_by_key(|item| std::cmp::Reverse(item.bytes));
        for item in ranked {
            progress.say(&format!(
                "    {:<10} {:>9}  {}",
                item.what,
                mb(item.bytes),
                item.detail
            ));
        }
        // Signed: estimates can overshoot, and over 100% is more honest than a
        // clamp.
        let residual = self.live as isize - accounted as isize;
        let share = if self.live > 0 {
            accounted as f64 / self.live as f64 * 100.0
        } else {
            0.0
        };
        progress.say(&format!(
            "    {:<10} {:>9}  {share:.0}% of live bytes named{}",
            "unnamed",
            mb(residual.unsigned_abs()),
            if residual < 0 {
                ", so the BTreeMap estimate is high"
            } else {
                ""
            }
        ));
    }
}

/// What the cut geometry costs, walked exactly, as capacity at every level.
pub fn of_pieces(pieces: &lyv_core::Pieces) -> Item {
    let mut chains = 0usize;
    let mut chain_capacity = 0usize;
    let mut points = 0usize;
    let mut point_capacity = 0usize;
    for held in pieces.values() {
        chains += held.len();
        chain_capacity += held.capacity();
        for chain in held {
            points += chain.points.len();
            point_capacity += chain.points.capacity();
        }
    }
    let bytes = btree_map_bytes::<lyv_core::TileCoord, Vec<lyv_core::Chain>>(pieces.len())
        + vec_bytes::<lyv_core::Chain>(chain_capacity)
        + vec_bytes::<geo_types::Coord<lyv_core::Local>>(point_capacity);
    Item::new(
        "pieces",
        bytes,
        format!(
            "{} tiles, {} chains, {} points{}",
            thousands(pieces.len()),
            thousands(chains),
            thousands(points),
            slack(points, point_capacity),
        ),
    )
}

/// What the ranking's buckets cost. The layer supplies counts; the arithmetic
/// stays here.
pub fn of_layer(layer: &lyv_rank::Layer) -> Item {
    let (buckets, ids, id_capacity, sizes) = layer.footprint();
    let bytes = btree_map_bytes::<lyv_core::TileCoord, Vec<u64>>(buckets)
        + vec_bytes::<u64>(id_capacity)
        + hash_map_bytes::<u64, lyv_rank::Size>(sizes);
    Item::new(
        "layer",
        bytes,
        format!(
            "{} buckets, {} ids{}, {} sizes",
            thousands(buckets),
            thousands(ids),
            slack(ids, id_capacity),
            thousands(sizes),
        ),
    )
}

/// What the per-feature facts cost.
pub fn of_facts(attr_bytes: usize) -> Item {
    Item::new(
        "facts",
        hash_map_bytes::<u64, u32>(attr_bytes),
        format!("{} attribute costs", thousands(attr_bytes)),
    )
}

/// `", 12% slack"`, or nothing when a vector fits. Non-empty means a missed
/// `shrink_to_fit`.
fn slack(len: usize, capacity: usize) -> String {
    if capacity <= len || len == 0 {
        return String::new();
    }
    format!(
        ", {:.0}% slack",
        (capacity - len) as f64 / len as f64 * 100.0
    )
}

fn thousands(n: usize) -> String {
    crate::timings::thousands(n as u64)
}

/// Megabytes, formatted like `lyv_alloc::mb` (which is optional, so can't be
/// borrowed).
fn mb(bytes: usize) -> String {
    format!("{:.0} MB", bytes as f64 / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vector_is_charged_for_capacity_not_length() {
        // Capacity, not length.
        assert_eq!(vec_bytes::<u64>(100), 800);
        let mut v: Vec<u64> = Vec::with_capacity(100);
        v.push(1);
        assert_eq!(vec_bytes::<u64>(v.capacity()), 800);
    }

    #[test]
    fn an_empty_container_costs_nothing() {
        // Empty collections don't allocate.
        assert_eq!(vec_bytes::<u64>(0), 0);
        assert_eq!(hash_map_bytes::<u64, u64>(0), 0);
        assert_eq!(btree_map_bytes::<u64, u64>(0), 0);
    }

    #[test]
    fn a_hash_map_is_charged_for_the_buckets_it_really_has() {
        // Against known bucket counts, not the formula: the bug this replaced
        // (doubling every map) had a test that agreed with it.
        let pair = size_of::<(u64, u32)>() + 1;
        assert_eq!(hash_map_bytes::<u64, u32>(57_344), 65_536 * pair);
        assert_eq!(hash_map_bytes::<u64, u32>(57_345), 131_072 * pair);
        assert_eq!(hash_map_bytes::<u64, u32>(14), 16 * pair);
        assert_eq!(hash_map_bytes::<u64, u32>(7), 8 * pair);
        assert_eq!(hash_map_bytes::<u64, u32>(3), 4 * pair);
    }

    #[test]
    fn a_real_map_is_never_charged_more_than_twice_its_contents() {
        // The worst real case is a map just past a doubling, about 2.3x its
        // contents; more means the estimate invents memory.
        const WORST: f64 = 2.5;
        for entries in [1u64, 9, 1_000, 100_000] {
            let map: std::collections::HashMap<u64, u32> =
                (0..entries).map(|n| (n, n as u32)).collect();
            let estimated = hash_map_bytes::<u64, u32>(map.capacity()) as f64;
            let contents = (entries as usize * (size_of::<(u64, u32)>() + 1)) as f64;
            assert!(
                estimated <= contents * WORST + 64.0,
                "{entries} entries charged {estimated} against contents of {contents}"
            );
        }
    }

    #[test]
    fn the_btree_estimate_sits_between_the_bounds_it_claims() {
        // A node holds 6 to 11 entries; the estimate must land in that range.
        let len = 1_000_000;
        let estimated = btree_map_bytes::<lyv_core::TileCoord, Vec<u64>>(len);
        let leaf = 16 + 11 * (size_of::<lyv_core::TileCoord>() + size_of::<Vec<u64>>());
        let fullest = len.div_ceil(11) * leaf;
        let emptiest = len.div_ceil(5) * leaf;
        assert!(
            estimated > fullest && estimated < emptiest,
            "{estimated} outside {fullest}..{emptiest}"
        );
    }

    #[test]
    fn slack_is_reported_only_when_there_is_some() {
        assert_eq!(slack(100, 100), "");
        assert_eq!(slack(0, 0), "");
        assert_eq!(slack(100, 136), ", 36% slack");
    }

    #[test]
    fn counts_read_as_counts() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(3_941_426), "3,941,426");
    }

    #[test]
    fn pieces_are_walked_to_the_last_point() {
        let mut pieces = lyv_core::Pieces::new();
        pieces.insert(
            lyv_core::TileCoord { z: 1, x: 0, y: 0 },
            vec![lyv_core::Chain {
                feature: 1,
                points: Vec::with_capacity(64),
            }],
        );
        let item = of_pieces(&pieces);
        // Reserved but unused points still count.
        assert!(
            item.detail.contains("1 tiles, 1 chains, 0 points"),
            "{}",
            item.detail
        );
        assert!(item.bytes >= 64 * size_of::<geo_types::Coord<lyv_core::Local>>());
    }
}
