//! Types every crate shares — tile addresses, per-tile pieces, attribute
//! values — and Web Mercator. Depends only on `geo-types`.

pub mod mercator;

use std::collections::BTreeMap;

use geo_types::Coord;

/// A tile address in the XYZ scheme, with `y` counted from the north.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct TileCoord {
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

impl TileCoord {
    /// The tile one level up that contains this one.
    pub fn parent(self) -> Self {
        Self {
            z: self.z.saturating_sub(1),
            x: self.x / 2,
            y: self.y / 2,
        }
    }

    /// Which quadrant of its parent this tile occupies, as `(x, y)` in `0..2`.
    pub fn quadrant(self) -> (u32, u32) {
        (self.x % 2, self.y % 2)
    }

    /// This tile's position along the Hilbert curve over the whole pyramid:
    /// zoom 0 is 0, zoom 1 is 1–4, zoom 2 is 5–20, and so on.
    ///
    /// PMTiles orders tiles this way. Nearby tiles get nearby ids, so a
    /// viewport is a near-contiguous run of bytes and a few range requests.
    pub fn hilbert_id(self) -> u64 {
        // Every shallower zoom comes first: 4^0 + ... + 4^(z-1).
        let mut id = ((1u64 << (2 * self.z)) - 1) / 3;

        // The standard xy-to-distance walk, folding the square and reflecting
        // the lower quadrants so the curve stays connected.
        let (mut x, mut y) = (u64::from(self.x), u64::from(self.y));
        let mut side = 1u64 << self.z;
        while side > 1 {
            side /= 2;
            let rx = u64::from(x & side > 0);
            let ry = u64::from(y & side > 0);
            id += side * side * ((3 * rx) ^ ry);
            if ry == 0 {
                if rx == 1 {
                    x = side.wrapping_sub(1).wrapping_sub(x);
                    y = side.wrapping_sub(1).wrapping_sub(y);
                }
                std::mem::swap(&mut x, &mut y);
            }
        }
        id
    }
}

/// A tile-local coordinate: `0..=extent` in a tile, up to `2 * extent` in the
/// merge's doubled frame.
///
/// Sixteen bits, as renderers store tile coordinates. A wider type buys nothing
/// downstream and doubled the memory of the cut level (520M points on
/// floodplains). The doubled frame is why extent is capped at 8192.
///
/// Arithmetic widens; only storage is narrow. Products overflow immediately, so
/// compute distances, areas and cross products in `i64` or `f64`.
pub type Local = i16;

/// The widest extent that keeps `2 * extent` inside [`Local`].
pub const MAX_EXTENT: u32 = 8192;

/// One feature's geometry within a single tile, in tile-local coordinates.
///
/// A chain's length says what it is:
///
/// - one point is a point;
/// - two or more is a run (part of a line);
/// - four or more with `first == last` is a ring. The length guard matters: a
///   one-point chain trivially has `first == last`.
///
/// No stage may change a chain's kind: a run leaves as a run or not at all,
/// and a point stays a point. A run collapsed onto one coordinate is dropped
/// (in `lyv_simplify`), since as a point it would move from a `line` style
/// layer to a `circle` one.
///
/// A tile wholly inside a polygon holds the tile square as an ordinary ring.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Chain {
    /// Which feature this came from, for rejoining chains when merging.
    pub feature: u64,
    pub points: Vec<Coord<Local>>,
}

/// Pieces of one or more features, grouped by the tile they belong to.
pub type Pieces = BTreeMap<TileCoord, Vec<Chain>>;

/// An attribute value: exactly the types MVT can carry. Our own type, since
/// `core` mustn't depend on a protobuf library.
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    Str(String),
    Float(f32),
    Double(f64),
    Int(i64),
    Uint(u64),
    Sint(i64),
    Bool(bool),
}

/// A layer's attribute key names, in first-seen order. Features store an
/// index into it rather than the name — a layer has a handful of names and
/// millions of features.
///
/// Only ever appended to (via [`Schema::of`]), so a feature encoded early still
/// decodes after the schema has grown: GeoJSON's keys arrive with the features.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    names: indexmap::IndexSet<String>,
}

impl Schema {
    /// This name's index, adding it if the schema has not seen it.
    pub fn of(&mut self, name: &str) -> u32 {
        if let Some(at) = self.names.get_index_of(name) {
            return at as u32;
        }
        let (at, _) = self.names.insert_full(name.to_string());
        at as u32
    }

    /// What an index means, or `None` if the schema predates it (a corrupt
    /// store).
    pub fn name(&self, at: u32) -> Option<&str> {
        self.names.get_index(at as usize).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    /// Rebuild from names in index order, as [`Schema::names`] yields them.
    pub fn from_names<I: IntoIterator<Item = String>>(names: I) -> Self {
        Self {
            names: names.into_iter().collect(),
        }
    }
}

/// Where an encoder finds a feature's attributes. A trait, not a map, so
/// implementations can hold keys as indices instead of a string per feature.
pub trait Attributes {
    /// Hand this feature's attributes to `visit`, in writing order. Each value
    /// is handed over, not lent: the encoder keeps it, and a value decoded on
    /// demand needn't be copied again.
    fn for_each(&self, feature: u64, visit: &mut dyn FnMut(&str, Value));
}

/// The simple implementation, for tests. The encode pass uses a denser one.
impl Attributes for std::collections::BTreeMap<u64, Vec<(String, Value)>> {
    fn for_each(&self, feature: u64, visit: &mut dyn FnMut(&str, Value)) {
        if let Some(properties) = self.get(&feature) {
            for (key, value) in properties {
                visit(key, value.clone());
            }
        }
    }
}

#[cfg(test)]
mod schema_tests {
    use super::Schema;

    #[test]
    fn a_name_keeps_its_index_however_many_arrive_after_it() {
        // An early feature's indices must keep their meaning as the schema
        // grows.
        let mut schema = Schema::default();
        let first = schema.of("name");
        let second = schema.of("height");
        for extra in 0..90 {
            schema.of(&format!("late_{extra}"));
        }
        assert_eq!(schema.of("name"), first, "an index is never reassigned");
        assert_eq!(schema.of("height"), second);
        assert_eq!(schema.name(first), Some("name"));
        assert_eq!(schema.name(second), Some("height"));
        assert_eq!(schema.len(), 92);
    }

    #[test]
    fn asking_twice_adds_once() {
        let mut schema = Schema::default();
        assert_eq!(schema.of("use"), 0);
        assert_eq!(schema.of("use"), 0);
        assert_eq!(schema.len(), 1);
    }

    #[test]
    fn a_schema_survives_being_written_down_and_read_back() {
        // The store's round trip must be exact, or every attribute is
        // mislabelled.
        let mut schema = Schema::default();
        for name in ["town_city", "use", "name", "capture_method"] {
            schema.of(name);
        }
        let written: Vec<String> = schema.names().map(str::to_string).collect();
        assert_eq!(Schema::from_names(written), schema);
    }

    #[test]
    fn an_index_the_schema_has_never_heard_of_is_not_a_name() {
        let schema = Schema::default();
        assert_eq!(schema.name(7), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tile_knows_its_parent_and_which_quadrant_it_fills() {
        let tile = TileCoord { z: 3, x: 5, y: 2 };
        assert_eq!(tile.parent(), TileCoord { z: 2, x: 2, y: 1 });
        assert_eq!(tile.quadrant(), (1, 0));
    }

    #[test]
    fn the_root_tile_is_its_own_parent() {
        // Saturates rather than underflowing.
        let root = TileCoord { z: 0, x: 0, y: 0 };
        assert_eq!(root.parent(), root);
    }

    #[test]
    fn all_four_children_of_a_tile_agree_on_their_parent() {
        let parent = TileCoord { z: 4, x: 6, y: 9 };
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let child = TileCoord {
                z: 5,
                x: parent.x * 2 + dx,
                y: parent.y * 2 + dy,
            };
            assert_eq!(child.parent(), parent);
            assert_eq!(child.quadrant(), (dx, dy));
        }
    }
}
