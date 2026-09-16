//! MVT encoding and decoding. The only crate that knows protobuf exists.

use std::collections::{BTreeMap, HashMap};

use geo_types::{
    Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};
use geozero::mvt::{Message, TagsBuilder, Tile, TileValue, tile};
use geozero::{ToGeo, ToMvt};

pub use lyv_core::{Chain, Local, Value};

/// The MVT specification version we emit.
const MVT_VERSION: u32 = 2;

/// Our attribute value as geozero's, which can write it.
fn to_tile_value(value: &Value) -> TileValue {
    match value {
        Value::Str(s) => TileValue::Str(s.clone()),
        Value::Float(f) => TileValue::Float(*f),
        Value::Double(d) => TileValue::Double(*d),
        Value::Int(i) => TileValue::Int(*i),
        Value::Uint(u) => TileValue::Uint(*u),
        Value::Sint(i) => TileValue::Sint(*i),
        Value::Bool(b) => TileValue::Bool(*b),
    }
}

/// Roughly how many bytes a tile's geometry will take, without encoding it.
///
/// For the maxzoom probe (§8), which estimates the worst tile at several
/// candidate zooms. A vertex is two zigzag varints of delta, almost always two
/// bytes each within a tile. Looser than [`attribute_cost`] because geometry is
/// often the smaller half. Checked against `encode_chains` in
/// `tests/geometry_cost.rs`.
pub fn geometry_cost(chains: &[Chain]) -> usize {
    if chains.is_empty() {
        return 0;
    }
    // Per chain: geometry framing, MoveTo and LineTo command words, ClosePath
    // for a ring. Charged per chain, since a feature in several pieces pays for
    // each.
    const PER_CHAIN: usize = 12;
    const PER_VERTEX: usize = 4;
    let vertices: usize = chains.iter().map(|chain| chain.points.len()).sum();
    chains.len() * PER_CHAIN + vertices * PER_VERTEX
}

/// Roughly how many bytes a feature's attributes add to an encoded tile.
///
/// Thinning has to predict a tile's size before encoding it; geometry alone
/// under-reads by 20x on attribute-heavy layers (§8). Lives beside the encoder
/// so the two can't drift.
///
/// Charged per attribute: a tag pair (two varint indices) and a value-pool
/// entry. Keys aren't charged — a layer's keys pool almost perfectly, and
/// charging them over-read by 45%. Values are charged per feature, though a
/// shared value is written once, so it over-reads: the safe direction, and
/// nearly exact on the layers that blow limits, where every value is distinct.
/// Checked in `tests/attribute_cost.rs`.
pub fn attribute_cost(properties: &[(String, Value)]) -> usize {
    properties
        .iter()
        .map(|(_key, value)| TAG_PAIR_BYTES + value_entry_bytes(value))
        .sum()
}

/// Two varint pool indices: two bytes while the pools hold under 128 entries,
/// more after. Averaged, since it's a byte against a pool entry of tens.
const TAG_PAIR_BYTES: usize = 3;

/// One entry in the `values` pool. A nested message, so framed twice.
fn value_entry_bytes(value: &Value) -> usize {
    let inner = match value {
        // Length-delimited: field tag, its own length, then the bytes.
        Value::Str(s) => 1 + length_prefix(s.len()) + s.len(),
        // Fixed-width on the wire.
        Value::Float(_) => 1 + 4,
        Value::Double(_) => 1 + 8,
        // Varints at their actual width: small integers are common.
        Value::Int(i) => 1 + varint_bytes(i.unsigned_abs()),
        Value::Uint(u) => 1 + varint_bytes(*u),
        Value::Sint(i) => 1 + varint_bytes(i.unsigned_abs().saturating_mul(2)),
        Value::Bool(_) => 1 + 1,
    };
    1 + length_prefix(inner) + inner
}

fn length_prefix(payload: usize) -> usize {
    varint_bytes(payload as u64)
}

fn varint_bytes(value: u64) -> usize {
    match value {
        0 => 1,
        _ => (u64::BITS - value.leading_zeros()).div_ceil(7) as usize,
    }
}

/// Encode a tile's chains, grouped by feature: a feature with two runs in the
/// tile is one MVT feature with a multi-part geometry. No attributes; the
/// intermediate pyramid doesn't carry them.
pub fn encode_chains(layer_name: &str, extent: u32, chains: &[Chain]) -> Vec<u8> {
    encode_chains_with(layer_name, extent, chains, &BTreeMap::new())
}

/// As [`encode_chains`], attaching each feature's attributes once per feature.
/// Ids with no entry get no tags.
pub fn encode_chains_with(
    layer_name: &str,
    extent: u32,
    chains: &[Chain],
    attributes: &dyn lyv_core::Attributes,
) -> Vec<u8> {
    // Group by feature id in first-seen order, so the output is stable.
    let mut order: Vec<u64> = Vec::new();
    let mut parts: HashMap<u64, Vec<LineString<f64>>> = HashMap::new();
    for chain in chains {
        let entry = parts.entry(chain.feature).or_insert_with(|| {
            order.push(chain.feature);
            Vec::new()
        });
        entry.push(LineString::from(
            chain
                .points
                .iter()
                .map(|p| (f64::from(p.x), f64::from(p.y)))
                .collect::<Vec<_>>(),
        ));
    }

    let mut tags = TagsBuilder::new();

    let features = order
        .into_iter()
        .map(|id| {
            let lines = parts.remove(&id).expect("grouped above");
            // All single coordinates: points (MVT has no one-point
            // LineString). All closed: a polygon. Otherwise a MultiLineString,
            // even for one part — MVT encodes the two identically.
            let geometry = match group_points(&lines) {
                Some(points) => Geometry::MultiPoint(MultiPoint::new(points)),
                None => match group_rings(&lines) {
                    Some(polygons) => Geometry::MultiPolygon(MultiPolygon::new(polygons)),
                    None => Geometry::MultiLineString(MultiLineString::new(lines)),
                },
            };
            let mut encoded = geometry
                .to_mvt_unscaled()
                .expect("tile-local geometry should always encode");
            encoded.id = Some(id);
            let mut pairs = Vec::new();
            attributes.for_each(id, &mut |key, value| {
                let (key_index, value_index) = tags.insert_ref(key, to_tile_value(value));
                pairs.push(key_index);
                pairs.push(value_index);
            });
            encoded.tags = pairs;
            encoded
        })
        .collect();

    let (keys, values) = tags.into_tags();

    Tile {
        layers: vec![tile::Layer {
            version: MVT_VERSION,
            name: layer_name.to_string(),
            features,
            keys,
            values: values.into_iter().map(tile::Value::from).collect(),
            extent: Some(extent),
        }],
    }
    .encode_to_vec()
}

/// A tile's extent, from its first layer. A layer that doesn't say has the
/// spec's default, 4096.
pub fn decode_extent(bytes: &[u8]) -> Result<u32, DecodeError> {
    let decoded = Tile::decode(bytes).map_err(|error| DecodeError::Malformed(error.to_string()))?;
    Ok(decoded
        .layers
        .first()
        .and_then(|layer| layer.extent)
        .unwrap_or(4096))
}

/// Read a tile back into chains, one per geometry part, each with its feature
/// id. The inverse of [`encode_chains`].
pub fn decode_chains(bytes: &[u8]) -> Result<Vec<Chain>, DecodeError> {
    let decoded = Tile::decode(bytes).map_err(|error| DecodeError::Malformed(error.to_string()))?;

    let mut chains = Vec::new();
    for layer in &decoded.layers {
        for feature in &layer.features {
            let id = feature.id.ok_or(DecodeError::MissingFeatureId)?;
            let geometry = feature
                .to_geo()
                .map_err(|error| DecodeError::Malformed(error.to_string()))?;

            // Rings come back closed explicitly: MVT's ClosePath doesn't repeat
            // the first point, and the pipeline recognises a ring by
            // `first == last`.
            let lines = match geometry {
                // Each point is a one-coordinate chain.
                Geometry::Point(point) => vec![LineString::from(vec![point.0])],
                Geometry::MultiPoint(points) => points
                    .0
                    .into_iter()
                    .map(|point| LineString::from(vec![point.0]))
                    .collect(),
                Geometry::LineString(line) => vec![line],
                Geometry::MultiLineString(lines) => lines.0,
                Geometry::Polygon(polygon) => rings_of(polygon),
                Geometry::MultiPolygon(polygons) => {
                    polygons.0.into_iter().flat_map(rings_of).collect()
                }
                other => {
                    return Err(DecodeError::Malformed(format!(
                        "expected a point, linestring or polygon, found {other:?}"
                    )));
                }
            };

            for line in lines {
                chains.push(Chain {
                    feature: id,
                    // Exact: MVT coordinates are integers, and geozero
                    // accumulates them as `i32` before widening. They fit
                    // `Local` because only our own tiles are decoded here; the
                    // assert catches a silently saturating cast.
                    points: line
                        .0
                        .iter()
                        .map(|c| {
                            debug_assert!(
                                c.x >= f64::from(Local::MIN)
                                    && c.x <= f64::from(Local::MAX)
                                    && c.y >= f64::from(Local::MIN)
                                    && c.y <= f64::from(Local::MAX),
                                "tile coordinate ({}, {}) does not fit a Local",
                                c.x,
                                c.y
                            );
                            Coord {
                                x: c.x as Local,
                                y: c.y as Local,
                            }
                        })
                        .collect(),
                });
            }
        }
    }
    Ok(chains)
}

/// The feature's parts as points, if every one is a single coordinate.
///
/// All or nothing: a mixture would need two MVT features, giving one feature
/// two ids in a tile. Nothing produces one; if it happens, it goes out as lines,
/// wrong but visible.
fn group_points(parts: &[LineString<f64>]) -> Option<Vec<Point<f64>>> {
    if parts.is_empty() || !parts.iter().all(|part| part.0.len() == 1) {
        return None;
    }
    Some(parts.iter().map(|part| Point::from(part.0[0])).collect())
}

/// Group closed rings into polygons, each an exterior followed by its holes.
/// `None` unless every part is a ring.
///
/// MVT has no nesting: a reader takes a positive ring as a new polygon and the
/// negative rings after it as its holes. So each hole is matched to the
/// exterior containing it — the smallest, if several do. A hole in no exterior
/// is dropped rather than punched into an unrelated ring.
fn group_rings(parts: &[LineString<f64>]) -> Option<Vec<Polygon<f64>>> {
    if parts.is_empty() || !parts.iter().all(is_closed_ring) {
        return None;
    }

    let mut exteriors: Vec<(LineString<f64>, f64)> = Vec::new();
    let mut holes: Vec<LineString<f64>> = Vec::new();
    for part in parts {
        let area = signed_area(part);
        if area > 0.0 {
            exteriors.push((part.clone(), area));
        } else if area < 0.0 {
            holes.push(part.clone());
        }
        // Zero area: nothing to draw.
    }
    if exteriors.is_empty() {
        return None;
    }

    let mut interiors: Vec<Vec<LineString<f64>>> = vec![Vec::new(); exteriors.len()];
    for hole in holes {
        let probe = hole.0[0];
        let smallest = exteriors
            .iter()
            .enumerate()
            .filter(|(_, (ring, _))| contains(ring, probe))
            .min_by(|(_, (_, a)), (_, (_, b))| a.total_cmp(b))
            .map(|(index, _)| index);
        if let Some(index) = smallest {
            interiors[index].push(hole);
        }
    }

    Some(
        exteriors
            .into_iter()
            .zip(interiors)
            .map(|((exterior, _), holes)| Polygon::new(exterior, holes))
            .collect(),
    )
}

fn is_closed_ring(part: &LineString<f64>) -> bool {
    part.0.len() >= 4 && part.0.first() == part.0.last()
}

/// Shoelace area. Positive is clockwise on screen, consistent with MVT.
fn signed_area(ring: &LineString<f64>) -> f64 {
    ring.0
        .windows(2)
        .map(|w| w[0].x * w[1].y - w[1].x * w[0].y)
        .sum::<f64>()
        / 2.0
}

/// Is this point inside the ring? Crossing count, half-open in y.
fn contains(ring: &LineString<f64>, point: Coord<f64>) -> bool {
    let mut inside = false;
    for pair in ring.0.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if (a.y <= point.y) != (b.y <= point.y) {
            let crossing = a.x + (point.y - a.y) / (b.y - a.y) * (b.x - a.x);
            if crossing > point.x {
                inside = !inside;
            }
        }
    }
    inside
}

/// What kind of geometry a feature holds. `decode_chains` turns rings back into
/// chains, so this is how to check a polygon went out as a polygon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GeometryKind {
    Point { points: usize },
    Line { parts: usize },
    Areal { polygons: usize, interiors: usize },
}

impl GeometryKind {
    /// Is this a polygon rather than a linestring?
    pub fn is_areal(self) -> bool {
        matches!(self, Self::Areal { .. })
    }

    /// How many holes it has.
    pub fn interior_rings(self) -> usize {
        match self {
            Self::Areal { interiors, .. } => interiors,
            Self::Point { .. } | Self::Line { .. } => 0,
        }
    }
}

/// The geometry kind of every feature in a tile, with its id.
pub fn decode_geometry_kinds(bytes: &[u8]) -> Result<Vec<(u64, GeometryKind)>, DecodeError> {
    let decoded = Tile::decode(bytes).map_err(|e| DecodeError::Malformed(e.to_string()))?;

    let mut out = Vec::new();
    for layer in &decoded.layers {
        for feature in &layer.features {
            let id = feature.id.ok_or(DecodeError::MissingFeatureId)?;
            let geometry = feature
                .to_geo()
                .map_err(|e| DecodeError::Malformed(e.to_string()))?;
            let kind = match geometry {
                Geometry::Point(_) => GeometryKind::Point { points: 1 },
                Geometry::MultiPoint(points) => GeometryKind::Point {
                    points: points.0.len(),
                },
                Geometry::LineString(_) => GeometryKind::Line { parts: 1 },
                Geometry::MultiLineString(lines) => GeometryKind::Line {
                    parts: lines.0.len(),
                },
                Geometry::Polygon(polygon) => GeometryKind::Areal {
                    polygons: 1,
                    interiors: polygon.interiors().len(),
                },
                Geometry::MultiPolygon(polygons) => GeometryKind::Areal {
                    polygons: polygons.0.len(),
                    interiors: polygons.0.iter().map(|p| p.interiors().len()).sum(),
                },
                other => {
                    return Err(DecodeError::Malformed(format!("unexpected {other:?}")));
                }
            };
            out.push((id, kind));
        }
    }
    Ok(out)
}

/// One feature's id and the attributes it carries.
pub type FeatureAttributes = (u64, Vec<(String, Value)>);

/// Read the attributes a tile holds, per feature. Features with no tags come
/// back with an empty list, so every feature present is listed.
pub fn decode_attributes(bytes: &[u8]) -> Result<Vec<FeatureAttributes>, DecodeError> {
    let decoded = Tile::decode(bytes).map_err(|error| DecodeError::Malformed(error.to_string()))?;

    let mut out = Vec::new();
    for layer in &decoded.layers {
        for feature in &layer.features {
            let id = feature.id.ok_or(DecodeError::MissingFeatureId)?;
            let mut properties = Vec::new();
            for pair in feature.tags.chunks_exact(2) {
                let (key_index, value_index) = (pair[0] as usize, pair[1] as usize);
                let key = layer.keys.get(key_index).ok_or_else(|| {
                    DecodeError::Malformed(format!("tag key {key_index} is out of range"))
                })?;
                let value = layer.values.get(value_index).ok_or_else(|| {
                    DecodeError::Malformed(format!("tag value {value_index} is out of range"))
                })?;
                properties.push((key.clone(), from_tile_value(value)?));
            }
            out.push((id, properties));
        }
    }
    Ok(out)
}

/// The protobuf value message back into ours. Exactly one field is set.
fn from_tile_value(value: &tile::Value) -> Result<Value, DecodeError> {
    if let Some(s) = &value.string_value {
        return Ok(Value::Str(s.clone()));
    }
    if let Some(f) = value.float_value {
        return Ok(Value::Float(f));
    }
    if let Some(d) = value.double_value {
        return Ok(Value::Double(d));
    }
    if let Some(i) = value.int_value {
        return Ok(Value::Int(i));
    }
    if let Some(u) = value.uint_value {
        return Ok(Value::Uint(u));
    }
    if let Some(i) = value.sint_value {
        return Ok(Value::Sint(i));
    }
    if let Some(b) = value.bool_value {
        return Ok(Value::Bool(b));
    }
    Err(DecodeError::Malformed("a tag value holds nothing".into()))
}

/// A polygon's rings, exterior first, each closed explicitly. `Polygon::new`
/// already closes them; the assert keeps that true.
fn rings_of(polygon: Polygon<f64>) -> Vec<LineString<f64>> {
    let (exterior, interiors) = polygon.into_inner();
    std::iter::once(exterior)
        .chain(interiors)
        .inspect(|ring| {
            debug_assert_eq!(
                ring.0.first(),
                ring.0.last(),
                "geo handed back an unclosed polygon ring"
            );
        })
        .collect()
}

/// Why a tile could not be read.
#[derive(Debug)]
pub enum DecodeError {
    /// The bytes are not a well-formed MVT tile.
    Malformed(String),
    /// A feature has no id, so its pieces could never be rejoined.
    MissingFeatureId,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "not a well-formed vector tile: {why}"),
            Self::MissingFeatureId => write!(
                f,
                "a feature has no id; pieces of it could never be rejoined when merging"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(u64, Vec<(&str, Value)>)]) -> BTreeMap<u64, Vec<(String, Value)>> {
        pairs
            .iter()
            .map(|(id, props)| {
                (
                    *id,
                    props
                        .iter()
                        .map(|(k, v)| ((*k).to_string(), v.clone()))
                        .collect(),
                )
            })
            .collect()
    }

    fn chain_of(feature: u64, points: &[(Local, Local)]) -> Chain {
        Chain {
            feature,
            points: points.iter().map(|&(x, y)| Coord { x, y }).collect(),
        }
    }

    fn ring(points: &[(Local, Local)]) -> Vec<Coord<Local>> {
        let mut out: Vec<Coord<Local>> = points.iter().map(|&(x, y)| Coord { x, y }).collect();
        out.push(out[0]);
        out
    }

    fn geometry_of(bytes: &[u8]) -> Geometry<f64> {
        let tile = Tile::decode(bytes).unwrap();
        tile.layers[0].features[0].to_geo().unwrap()
    }

    #[test]
    fn a_feature_whose_parts_are_all_rings_goes_out_as_a_polygon() {
        // Other consumers read the geometry type; MapLibre fills a linestring
        // anyway, which hid this.
        let chains = vec![Chain {
            feature: 1,
            points: ring(&[(0, 0), (100, 0), (100, 100), (0, 100)]),
        }];
        let bytes = encode_chains("water", 4096, &chains);
        assert!(
            matches!(
                geometry_of(&bytes),
                Geometry::Polygon(_) | Geometry::MultiPolygon(_)
            ),
            "got {:?}",
            geometry_of(&bytes)
        );
    }

    #[test]
    fn an_open_chain_still_goes_out_as_a_linestring() {
        let chains = vec![Chain {
            feature: 1,
            points: vec![Coord { x: 0, y: 0 }, Coord { x: 100, y: 100 }],
        }];
        assert!(matches!(
            geometry_of(&encode_chains("water", 4096, &chains)),
            Geometry::LineString(_) | Geometry::MultiLineString(_)
        ));
    }

    #[test]
    fn a_hole_is_attached_to_the_exterior_that_contains_it() {
        // Holes must follow their own exterior. Two squares with a hole each,
        // given interleaved.
        let left = ring(&[(0, 0), (100, 0), (100, 100), (0, 100)]);
        let left_hole = ring(&[(20, 20), (20, 80), (80, 80), (80, 20)]);
        let right = ring(&[(200, 0), (300, 0), (300, 100), (200, 100)]);
        let right_hole = ring(&[(220, 20), (220, 80), (280, 80), (280, 20)]);

        let chains: Vec<Chain> = [left_hole, right, left, right_hole]
            .into_iter()
            .map(|points| Chain { feature: 1, points })
            .collect();

        let Geometry::MultiPolygon(polygons) = geometry_of(&encode_chains("w", 4096, &chains))
        else {
            panic!("expected a multipolygon");
        };
        assert_eq!(polygons.0.len(), 2, "two exteriors, two polygons");
        for polygon in &polygons.0 {
            assert_eq!(polygon.interiors().len(), 1, "each keeps one hole");
            let hole = &polygon.interiors()[0].0[0];
            let xs: Vec<f64> = polygon.exterior().0.iter().map(|c| c.x).collect();
            let (low, high) = (
                xs.iter().cloned().fold(f64::MAX, f64::min),
                xs.iter().cloned().fold(f64::MIN, f64::max),
            );
            assert!(
                hole.x > low && hole.x < high,
                "a hole at x={} was attached to the exterior spanning {low}..{high}",
                hole.x
            );
        }
    }

    #[test]
    fn a_hole_with_no_exterior_to_cut_is_dropped() {
        // A hole with no exterior can't be expressed and mustn't be attached
        // to an unrelated ring. Coverage should prevent it arising.
        let lone_hole = Chain {
            feature: 1,
            points: ring(&[(20, 20), (20, 80), (80, 80), (80, 20)]),
        };
        let bytes = encode_chains("w", 4096, std::slice::from_ref(&lone_hole));
        let tile = Tile::decode(&bytes[..]).unwrap();
        // So it falls back to a linestring rather than an invalid polygon.
        assert!(matches!(
            tile.layers[0].features[0].to_geo().unwrap(),
            Geometry::LineString(_) | Geometry::MultiLineString(_)
        ));
    }

    #[test]
    fn rings_round_trip_through_polygon_encoding() {
        // The store's round trip: a ring written as a polygon comes back as a
        // closed chain.
        let chains = vec![
            Chain {
                feature: 1,
                points: ring(&[(0, 0), (100, 0), (100, 100), (0, 100)]),
            },
            Chain {
                feature: 1,
                points: ring(&[(20, 20), (20, 80), (80, 80), (80, 20)]),
            },
        ];
        let back = decode_chains(&encode_chains("w", 4096, &chains)).unwrap();
        assert_eq!(back.len(), 2);
        for chain in &back {
            assert_eq!(chain.points.first(), chain.points.last(), "closure lost");
        }
        assert_eq!(back, chains, "the rings changed on the way through");
    }

    #[test]
    fn attributes_round_trip_through_a_tile() {
        let chains = vec![
            chain_of(1, &[(0, 0), (10, 10)]),
            chain_of(2, &[(1, 1), (2, 2)]),
        ];
        let attributes = attrs(&[
            (
                1,
                vec![
                    ("name", Value::Str("Wairarapa".into())),
                    ("area", Value::Int(78)),
                    ("depth", Value::Double(2.5)),
                    ("tidal", Value::Bool(false)),
                ],
            ),
            (2, vec![("name", Value::Str("Ruamahanga".into()))]),
        ]);

        let bytes = encode_chains_with("water", 4096, &chains, &attributes);
        let back = decode_attributes(&bytes).unwrap();

        assert_eq!(back.len(), 2);
        for (id, properties) in back {
            assert_eq!(
                properties,
                *attributes.get(&id).unwrap(),
                "feature {id} came back different"
            );
        }
    }

    #[test]
    fn a_features_attributes_are_written_once_however_many_chains_it_has() {
        // Three pieces in a tile are one feature with one set of tags.
        let chains = vec![
            chain_of(1, &[(0, 0), (10, 10)]),
            chain_of(1, &[(20, 20), (30, 30)]),
            chain_of(1, &[(40, 40), (50, 50)]),
        ];
        let attributes = attrs(&[(1, vec![("name", Value::Str("Ruamahanga".into()))])]);

        let bytes = encode_chains_with("water", 4096, &chains, &attributes);
        let layer = Tile::decode(&bytes[..]).unwrap().layers.remove(0);

        assert_eq!(layer.features.len(), 1, "three chains, one feature");
        assert_eq!(layer.features[0].tags.len(), 2, "one key/value pair");
        assert_eq!(layer.keys, vec!["name"]);
    }

    #[test]
    fn attributes_are_pooled_across_features() {
        // A shared value is stored once.
        let chains = vec![
            chain_of(1, &[(0, 0), (10, 10)]),
            chain_of(2, &[(20, 20), (30, 30)]),
        ];
        let attributes = attrs(&[
            (
                1,
                vec![("kind", Value::Str("river".into())), ("id", Value::Int(1))],
            ),
            (
                2,
                vec![("kind", Value::Str("river".into())), ("id", Value::Int(2))],
            ),
        ]);

        let bytes = encode_chains_with("water", 4096, &chains, &attributes);
        let layer = Tile::decode(&bytes[..]).unwrap().layers.remove(0);

        assert_eq!(layer.keys.len(), 2, "keys pooled: kind, id");
        assert_eq!(
            layer.values.len(),
            3,
            "values pooled: the shared \"river\" once, plus 1 and 2"
        );
    }

    #[test]
    fn a_feature_with_no_entry_gets_no_tags() {
        let chains = vec![chain_of(1, &[(0, 0), (10, 10)])];
        let bytes = encode_chains_with("water", 4096, &chains, &BTreeMap::new());
        let layer = Tile::decode(&bytes[..]).unwrap().layers.remove(0);

        assert!(layer.features[0].tags.is_empty());
        assert!(layer.keys.is_empty() && layer.values.is_empty());
    }

    #[test]
    fn encoding_without_attributes_is_byte_identical_to_before() {
        // The intermediate pyramid stays attribute-free.
        let chains = vec![chain_of(1, &[(0, 0), (10, 10)])];
        assert_eq!(
            encode_chains("water", 4096, &chains),
            encode_chains_with("water", 4096, &chains, &BTreeMap::new()),
        );
    }

    #[test]
    fn every_attribute_type_reaches_the_wire() {
        let chains = vec![chain_of(1, &[(0, 0), (10, 10)])];
        let attributes = attrs(&[(
            1,
            vec![
                ("s", Value::Str("x".into())),
                ("i", Value::Int(-7)),
                ("u", Value::Uint(7)),
                ("d", Value::Double(1.5)),
                ("b", Value::Bool(true)),
            ],
        )]);
        let bytes = encode_chains_with("water", 4096, &chains, &attributes);
        let layer = Tile::decode(&bytes[..]).unwrap().layers.remove(0);

        assert_eq!(layer.features[0].tags.len(), 10, "five pairs");
        assert_eq!(layer.keys.len(), 5);
        assert_eq!(layer.values.len(), 5);
    }

    #[test]
    fn a_closed_ring_round_trips_with_its_closing_point() {
        // A ring's repeated closing point must survive the round trip, or
        // every stored polygon would change.
        let ring = Chain {
            feature: 3,
            points: vec![
                Coord { x: 100, y: 100 },
                Coord { x: 900, y: 150 },
                Coord { x: 500, y: 800 },
                Coord { x: 100, y: 100 },
            ],
        };

        let bytes = encode_chains("polys", 4096, std::slice::from_ref(&ring));
        let back = decode_chains(&bytes).expect("a ring should decode");
        assert_eq!(back, vec![ring]);
    }

    #[test]
    fn a_ring_with_a_repeated_interior_point_is_not_silently_tidied() {
        // No deduplication: the merge compares coordinates exactly.
        let ring = Chain {
            feature: 1,
            points: vec![
                Coord { x: 0, y: 0 },
                Coord { x: 4096, y: 0 },
                Coord { x: 4096, y: 4096 },
                Coord { x: 0, y: 4096 },
                Coord { x: 0, y: 0 },
            ],
        };
        let bytes = encode_chains("polys", 4096, std::slice::from_ref(&ring));
        assert_eq!(decode_chains(&bytes).unwrap(), vec![ring]);
    }
    use geozero::mvt::{Message, Tile};
}

#[cfg(test)]
mod chain_tests {
    use super::*;
    use geo_types::Coord;

    fn chain(feature: u64, points: &[(Local, Local)]) -> Chain {
        Chain {
            feature,
            points: points.iter().map(|&(x, y)| Coord { x, y }).collect(),
        }
    }

    fn round_trip(chains: &[Chain]) -> Vec<Chain> {
        decode_chains(&encode_chains("l", 4096, chains)).expect("should decode")
    }

    #[test]
    fn one_chain_survives_a_round_trip_exactly() {
        let chains = vec![chain(7, &[(10, 20), (30, 40), (50, 20)])];
        assert_eq!(round_trip(&chains), chains);
    }

    #[test]
    fn point_order_is_not_reversed() {
        // Direction must survive: the merge joins one chain's tail to
        // another's head.
        let chains = vec![chain(1, &[(0, 0), (100, 5), (200, 500)])];
        let back = round_trip(&chains);
        assert_eq!(back[0].points.first(), chains[0].points.first());
        assert_eq!(back[0].points.last(), chains[0].points.last());
    }

    #[test]
    fn several_features_stay_separate() {
        let chains = vec![
            chain(1, &[(0, 0), (10, 10)]),
            chain(2, &[(20, 20), (30, 30)]),
            chain(3, &[(40, 40), (50, 50)]),
        ];
        assert_eq!(round_trip(&chains), chains);
    }

    #[test]
    fn two_chains_of_one_feature_become_one_multipart_feature() {
        // Two runs of one feature in a tile are one multi-part feature.
        let chains = vec![
            chain(9, &[(0, 0), (10, 0)]),
            chain(9, &[(100, 0), (110, 0)]),
        ];

        let bytes = encode_chains("l", 4096, &chains);
        let tile = Tile::decode(&bytes[..]).unwrap();
        assert_eq!(
            tile.layers[0].features.len(),
            1,
            "should be one feature with two parts, not two features"
        );
        assert_eq!(tile.layers[0].features[0].id, Some(9));

        assert_eq!(round_trip(&chains), chains, "and both parts must come back");
    }

    #[test]
    fn chains_of_the_same_feature_are_grouped_even_when_not_adjacent() {
        let chains = vec![
            chain(1, &[(0, 0), (10, 0)]),
            chain(2, &[(0, 50), (10, 50)]),
            chain(1, &[(100, 0), (110, 0)]),
        ];
        let bytes = encode_chains("l", 4096, &chains);
        let tile = Tile::decode(&bytes[..]).unwrap();
        assert_eq!(tile.layers[0].features.len(), 2);

        let mut back = round_trip(&chains);
        back.sort_by_key(|c| (c.feature, c.points[0].x));
        let mut expected = chains.clone();
        expected.sort_by_key(|c| (c.feature, c.points[0].x));
        assert_eq!(back, expected);
    }

    #[test]
    fn points_have_a_geometry_kind() {
        let one = encode_chains("l", 4096, &[chain(1, &[(5, 5)])]);
        let two = encode_chains("l", 4096, &[chain(2, &[(5, 5)]), chain(2, &[(9, 9)])]);
        assert_eq!(
            decode_geometry_kinds(&one).unwrap(),
            vec![(1, GeometryKind::Point { points: 1 })]
        );
        assert_eq!(
            decode_geometry_kinds(&two).unwrap(),
            vec![(2, GeometryKind::Point { points: 2 })]
        );
    }

    #[test]
    fn a_tile_says_its_extent() {
        let tile = encode_chains("l", 8192, &[chain(1, &[(0, 0), (10, 0)])]);
        assert_eq!(decode_extent(&tile).unwrap(), 8192);
    }

    #[test]
    fn an_empty_tile_round_trips_as_no_chains() {
        // An empty tile is a real tile: a merge that collapsed to nothing is
        // still stored.
        let bytes = encode_chains("l", 4096, &[]);
        assert_eq!(decode_chains(&bytes).unwrap(), Vec::new());
    }

    #[test]
    fn coordinates_outside_the_extent_survive() {
        // Buffers go outside 0..extent, which the spec allows.
        let chains = vec![chain(1, &[(-80, -80), (4176, 4176)])];
        assert_eq!(round_trip(&chains), chains);
    }

    #[test]
    fn rubbish_is_rejected_rather_than_misread() {
        let error = decode_chains(b"this is not a vector tile at all")
            .expect_err("should refuse to decode");
        assert!(matches!(error, DecodeError::Malformed(_)), "got {error:?}");
    }

    #[test]
    fn a_feature_without_an_id_is_rejected() {
        // The merge joins pieces by id, so a feature without one is an error.
        let mut tile =
            Tile::decode(&encode_chains("l", 4096, &[chain(1, &[(0, 0), (1, 1)])])[..]).unwrap();
        tile.layers[0].features[0].id = None;

        let error = decode_chains(&tile.encode_to_vec()).expect_err("should refuse");
        assert!(
            matches!(error, DecodeError::MissingFeatureId),
            "got {error:?}"
        );
    }
}

#[cfg(test)]
mod chain_property_tests {
    use super::*;
    use geo_types::Coord;

    /// At least two points each: a single point isn't a run.
    fn generated_chains(next: &mut impl FnMut() -> u64) -> Vec<Chain> {
        let count = 1 + (next() % 6) as usize;
        (0..count)
            .map(|_| Chain {
                feature: next() % 4,
                points: (0..2 + (next() % 8) as usize)
                    .map(|_| Coord {
                        x: (next() % 8192) as Local - 2048,
                        y: (next() % 8192) as Local - 2048,
                    })
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn any_set_of_chains_survives_a_round_trip() {
        let mut seed = 0xBADA55E1FBABEu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for case in 0..2000 {
            let chains = generated_chains(&mut next);
            let back = decode_chains(&encode_chains("l", 4096, &chains))
                .unwrap_or_else(|e| panic!("case {case}: {e}"));

            // Grouping reorders, so compare as a multiset of tuples.
            let key = |c: &Chain| {
                (
                    c.feature,
                    c.points.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>(),
                )
            };
            let mut expected: Vec<_> = chains.iter().map(key).collect();
            let mut actual: Vec<_> = back.iter().map(key).collect();
            expected.sort();
            actual.sort();
            assert_eq!(actual, expected, "case {case}");
        }
    }

    #[test]
    fn every_feature_id_is_preserved() {
        let mut seed = 0xBADA55E1FBABEu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for case in 0..500 {
            let chains = generated_chains(&mut next);
            let back = decode_chains(&encode_chains("l", 4096, &chains)).unwrap();

            let mut expected: Vec<u64> = chains.iter().map(|c| c.feature).collect();
            let mut actual: Vec<u64> = back.iter().map(|c| c.feature).collect();
            expected.sort_unstable();
            actual.sort_unstable();
            assert_eq!(actual, expected, "case {case}");
        }
    }
}
