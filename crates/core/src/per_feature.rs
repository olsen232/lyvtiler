//! One value per feature, by feature id.
//!
//! Parquet and FlatGeobuf number features by row, so ids are dense and an
//! array indexed by id costs only the values. A `HashMap<u64, T>` costs
//! several times that: on 30.6M features, 58.7M slots of key, value and
//! control byte. GeoJSON can bring its own sparse ids, so past a point the
//! array gives way to a map; behaviour is the same either way.

use std::collections::HashMap;

/// How far past what's held an id may land before the ids are taken to be
/// sparse: the array may hold this many empty slots beyond four per value.
const SPARSE_SLACK: u64 = 1_000_000;

#[derive(Clone)]
pub struct PerFeature<T> {
    repr: Repr<T>,
    len: usize,
}

#[derive(Clone)]
enum Repr<T> {
    /// `values[i]` is id `base + i`, present when bit `i` of `present` is set.
    /// `base` is the first id inserted, so a share of the read whose ids start
    /// in the millions doesn't hold an array from zero.
    Dense {
        base: u64,
        values: Vec<T>,
        present: Vec<u64>,
    },
    Sparse(HashMap<u64, T>),
}

impl<T> Default for PerFeature<T> {
    fn default() -> Self {
        Self {
            repr: Repr::Dense {
                base: 0,
                values: Vec::new(),
                present: Vec::new(),
            },
            len: 0,
        }
    }
}

fn is_set(present: &[u64], index: usize) -> bool {
    present[index / 64] & (1 << (index % 64)) != 0
}

impl<T: Copy + Default> PerFeature<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, id: u64, value: T) {
        if !self.fits(id) {
            self.go_sparse();
        }
        match &mut self.repr {
            Repr::Dense {
                base,
                values,
                present,
            } => {
                let index = (id - *base) as usize;
                if index >= values.len() {
                    values.resize(index + 1, T::default());
                    present.resize(index / 64 + 1, 0);
                }
                values[index] = value;
                let bit = 1 << (index % 64);
                if present[index / 64] & bit == 0 {
                    present[index / 64] |= bit;
                    self.len += 1;
                }
            }
            Repr::Sparse(map) => {
                if map.insert(id, value).is_none() {
                    self.len += 1;
                }
            }
        }
    }

    /// Whether `id` belongs in the array: at or past its base, and not so far
    /// past what's held that the gap would cost more than a map. An empty
    /// array takes `id` as its base.
    fn fits(&mut self, id: u64) -> bool {
        match &mut self.repr {
            Repr::Dense { base, values, .. } => {
                if values.is_empty() {
                    *base = id;
                    return true;
                }
                id >= *base && id - *base <= 4 * self.len as u64 + SPARSE_SLACK
            }
            Repr::Sparse(_) => true,
        }
    }

    fn go_sparse(&mut self) {
        let map: HashMap<u64, T> = self.iter().collect();
        self.repr = Repr::Sparse(map);
    }

    pub fn get(&self, id: u64) -> Option<T> {
        match &self.repr {
            Repr::Dense {
                base,
                values,
                present,
            } => {
                let index = usize::try_from(id.checked_sub(*base)?).ok()?;
                (index < values.len() && is_set(present, index)).then(|| values[index])
            }
            Repr::Sparse(map) => map.get(&id).copied(),
        }
    }

    pub fn contains(&self, id: u64) -> bool {
        self.get(id).is_some()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether it's still the array: for tests and the memory report.
    pub fn is_dense(&self) -> bool {
        matches!(self.repr, Repr::Dense { .. })
    }

    /// Every id and value, in id order.
    pub fn iter(&self) -> Box<dyn Iterator<Item = (u64, T)> + '_> {
        match &self.repr {
            Repr::Dense {
                base,
                values,
                present,
            } => Box::new(
                values
                    .iter()
                    .enumerate()
                    .filter(move |(index, _)| is_set(present, *index))
                    .map(move |(index, value)| (*base + index as u64, *value)),
            ),
            Repr::Sparse(map) => {
                let mut listed: Vec<(u64, T)> = map.iter().map(|(id, v)| (*id, *v)).collect();
                listed.sort_unstable_by_key(|(id, _)| *id);
                Box::new(listed.into_iter())
            }
        }
    }

    pub fn ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.iter().map(|(id, _)| id)
    }

    pub fn values(&self) -> impl Iterator<Item = T> + '_ {
        self.iter().map(|(_, value)| value)
    }

    /// Absorb another, for a layer built in shares across threads. A value
    /// in both is `other`'s, as with `HashMap::extend`.
    pub fn extend(&mut self, other: PerFeature<T>) {
        for (id, value) in other.iter() {
            self.insert(id, value);
        }
    }

    /// Hand back the capacity grown into, once nothing more is added.
    pub fn shrink(&mut self) {
        match &mut self.repr {
            Repr::Dense {
                values, present, ..
            } => {
                values.shrink_to_fit();
                present.shrink_to_fit();
            }
            Repr::Sparse(map) => map.shrink_to_fit(),
        }
    }

    /// Bytes held, capacity included, for `lyv::footprint`. A map's is an
    /// estimate: a slot per entry plus a control byte.
    pub fn bytes(&self) -> usize {
        match &self.repr {
            Repr::Dense {
                values, present, ..
            } => values.capacity() * std::mem::size_of::<T>() + present.capacity() * 8,
            Repr::Sparse(map) => map.capacity() * (std::mem::size_of::<(u64, T)>() + 1),
        }
    }
}

impl<T: Copy + Default> FromIterator<(u64, T)> for PerFeature<T> {
    fn from_iter<I: IntoIterator<Item = (u64, T)>>(entries: I) -> Self {
        let mut held = Self::new();
        for (id, value) in entries {
            held.insert(id, value);
        }
        held
    }
}

/// Equal when they hold the same ids and values, whichever way each is held.
impl<T: Copy + Default + PartialEq> PartialEq for PerFeature<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<T: Copy + Default + std::fmt::Debug> std::fmt::Debug for PerFeature<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::PerFeature;

    #[test]
    fn dense_ids_are_held_in_order() {
        let mut held = PerFeature::new();
        for id in [5u64, 7, 6, 5] {
            held.insert(id, id as u32 * 10);
        }
        assert!(held.is_dense());
        assert_eq!(held.len(), 3);
        assert_eq!(held.get(6), Some(60));
        assert_eq!(held.get(4), None);
        assert_eq!(held.get(8), None);
        assert!(held.contains(7));
        assert_eq!(
            held.iter().collect::<Vec<_>>(),
            vec![(5, 50), (6, 60), (7, 70)]
        );
    }

    #[test]
    fn sparse_ids_switch_to_a_map_and_read_the_same() {
        let ids = [3u64, 9_000_000_000, 4, 1];
        let mut held = PerFeature::new();
        for id in ids {
            held.insert(id, (id % 1000) as u32);
        }
        assert!(!held.is_dense());
        assert_eq!(held.len(), 4);
        assert_eq!(held.get(9_000_000_000), Some(0));
        assert_eq!(
            held.iter().collect::<Vec<_>>(),
            vec![(1, 1), (3, 3), (4, 4), (9_000_000_000, 0)]
        );
        let dense: PerFeature<u32> = [(1, 1), (3, 3), (4, 4), (9_000_000_000, 0)]
            .into_iter()
            .collect();
        assert_eq!(held, dense);
    }

    #[test]
    fn extending_merges_shares_in_any_order() {
        let mut whole = PerFeature::new();
        let mut first = PerFeature::new();
        let mut second = PerFeature::new();
        for id in 100u64..110 {
            first.insert(id, id);
        }
        for id in 110u64..120 {
            second.insert(id, id);
        }
        whole.extend(second);
        whole.extend(first);
        assert_eq!(whole.len(), 20);
        assert_eq!(
            whole.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            (100..120).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_unit_value_is_a_set() {
        let mut set = PerFeature::new();
        set.insert(2, ());
        set.insert(2, ());
        assert_eq!(set.len(), 1);
        assert!(set.contains(2));
        assert!(!set.contains(3));
    }
}
