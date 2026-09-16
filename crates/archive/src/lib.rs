//! Writes finished tiles into an archive (PMTiles or MBTiles).
//!
//! The narrowest interface in the program: four integers and a byte slice.
//! Nothing upstream knows about TMS rows, gzip, Hilbert order or SQLite, and
//! nothing here knows how a tile was made. Tiles arrive as uncompressed MVT;
//! compression is each format's business.

use std::path::Path;

use anyhow::{Result, bail};

mod mbtiles;
mod pmtiles;

pub use mbtiles::MbtilesWriter;
pub use pmtiles::PmtilesWriter;

/// Archive metadata, known before the first tile is written.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub name: String,
    /// What built the tileset, e.g. `lyv 0.1.0`, so the file can say which
    /// version made it. Set by the caller: this crate doesn't know who called
    /// it.
    pub generator: Option<String>,
    pub min_zoom: u32,
    pub max_zoom: u32,
    /// The data's extent, `[min_lon, min_lat, max_lon, max_lat]`. Unset, both
    /// formats claim the whole world.
    pub bounds: Option<[f64; 4]>,
    /// The extent of data that crosses the antimeridian, with the eastern
    /// longitude past 180, when `bounds` has to claim every longitude instead.
    /// Recorded as `antimeridian_adjusted_bounds`, `"w,s,e,n"`.
    pub antimeridian_adjusted_bounds: Option<[f64; 4]>,
    /// Where a viewer should open, as `(lon, lat, zoom)`.
    pub center: Option<(f64, f64, u32)>,
    /// The tileset's layers, for TileJSON's `vector_layers` — the only way a
    /// consumer can learn them without decoding a tile.
    pub layers: Vec<LayerInfo>,
}

/// One vector layer, as TileJSON describes it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LayerInfo {
    pub id: String,
    pub min_zoom: u32,
    pub max_zoom: u32,
    /// Attribute names and their types, in a stable order.
    pub fields: Vec<(String, FieldType)>,
}

/// TileJSON's three field types. Every numeric width becomes `Number`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldType {
    Number,
    Boolean,
    String,
}

impl FieldType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Number => "Number",
            Self::Boolean => "Boolean",
            Self::String => "String",
        }
    }
}

/// `vector_layers` as TileJSON spells it. Shared by both writers: PMTiles puts
/// it in its metadata blob, MBTiles in a `json` metadata row.
pub fn vector_layers_json(layers: &[LayerInfo]) -> serde_json::Value {
    serde_json::Value::Array(
        layers
            .iter()
            .map(|layer| {
                let fields: serde_json::Map<String, serde_json::Value> = layer
                    .fields
                    .iter()
                    .map(|(name, kind)| (name.clone(), serde_json::Value::from(kind.as_str())))
                    .collect();
                serde_json::json!({
                    "id": layer.id,
                    "minzoom": layer.min_zoom,
                    "maxzoom": layer.max_zoom,
                    "fields": fields,
                })
            })
            .collect(),
    )
}

/// `[w, s, e, n]` as the `"w,s,e,n"` string both formats store bounds as.
pub fn bounds_string([w, s, e, n]: [f64; 4]) -> String {
    format!("{w},{s},{e},{n}")
}

/// One encoded tile and where it goes: `(z, x, y)` and its bytes.
pub type FinishedTile = ((u32, u32, u32), Vec<u8>);

/// A sink for finished tiles.
pub trait TileWriter {
    /// Store one tile. `y` is an XYZ row, from the north; `mvt` is
    /// uncompressed.
    fn write_tile(&mut self, z: u32, x: u32, y: u32, mvt: &[u8]) -> Result<()>;

    /// Store a run of tiles, in order. Lets a format compress a batch in
    /// parallel while still writing in order, as PMTiles' clustering needs.
    /// The default writes them one at a time.
    fn write_tiles(&mut self, batch: &[FinishedTile]) -> Result<()> {
        for ((z, x, y), mvt) in batch {
            self.write_tile(*z, *x, *y, mvt)?;
        }
        Ok(())
    }

    /// How many tiles to hand to `write_tiles` at once; zero means don't
    /// batch.
    fn batch_size(&self) -> usize {
        0
    }

    /// How many uncompressed bytes a batch may hold, whatever the count; zero
    /// means no limit.
    ///
    /// A count alone doesn't bound memory: tiles range from kilobytes to
    /// megabytes, and the count scales with cores. On 3.5M parcels, whose z9
    /// has tiles up to 4 MB, this saved 210 MB of peak resident set.
    fn batch_bytes(&self) -> usize {
        0
    }

    /// Finish the archive. `Box<Self>` keeps the trait object-safe; PMTiles
    /// can't write its header until it has every tile.
    fn finish(self: Box<Self>) -> Result<()>;
}

/// Create a writer for `path`, choosing the format from its extension.
pub fn create(path: &Path, metadata: &Metadata) -> Result<Box<dyn TileWriter>> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("mbtiles") => Ok(Box::new(MbtilesWriter::create(path, metadata)?)),
        Some("pmtiles") => Ok(Box::new(PmtilesWriter::create(path, metadata)?)),
        other => bail!(
            "unsupported output format {:?}; expected .mbtiles or .pmtiles",
            other.unwrap_or("(none)")
        ),
    }
}
