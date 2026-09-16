//! Writing tiles into a PMTiles archive.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use anyhow::{Context, Result};
use pmtiles::{Compression, PmTilesStreamWriter, PmTilesWriter, TileCoord, TileType};

use super::{Metadata, TileWriter};

/// Writes MVT tiles into a PMTiles archive. The header and directories can
/// only be written once every tile is known, so the real work is in
/// [`TileWriter::finish`].
pub struct PmtilesWriter {
    inner: PmTilesStreamWriter<BufWriter<File>>,
    /// How many tiles to compress at once. See [`TileWriter::write_tiles`].
    workers: usize,
}

impl PmtilesWriter {
    /// Create a new archive at `path`, replacing any existing file.
    pub fn create(path: &Path, metadata: &Metadata) -> Result<Self> {
        let mut blob = serde_json::json!({ "name": metadata.name });
        if let Some(generator) = &metadata.generator {
            blob["generator"] = generator.as_str().into();
        }
        if let Some(adjusted) = metadata.antimeridian_adjusted_bounds {
            blob["antimeridian_adjusted_bounds"] = super::bounds_string(adjusted).into();
        }
        if !metadata.layers.is_empty() {
            blob["vector_layers"] = super::vector_layers_json(&metadata.layers);
        }
        let json = blob.to_string();

        // The header declares gzip, and we compress ourselves, in parallel;
        // `add_raw_tile` takes bytes already compressed.
        let mut builder = PmTilesWriter::new(TileType::Mvt)
            .tile_compression(Compression::Gzip)
            .min_zoom(metadata.min_zoom as u8)
            .max_zoom(metadata.max_zoom as u8)
            .metadata(&json);

        // Fixed header fields. Unset, they claim the whole world.
        if let Some([min_lon, min_lat, max_lon, max_lat]) = metadata.bounds {
            builder = builder.bounds(min_lon, min_lat, max_lon, max_lat);
        }
        if let Some((lon, lat, zoom)) = metadata.center {
            builder = builder.center(lon, lat).center_zoom(zoom as u8);
        }

        let inner = builder
            .create(BufWriter::new(File::create(path)?))
            .with_context(|| format!("creating {}", path.display()))?;

        Ok(Self {
            inner,
            workers: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        })
    }
}

impl TileWriter for PmtilesWriter {
    fn write_tile(&mut self, z: u32, x: u32, y: u32, mvt: &[u8]) -> Result<()> {
        let coord = TileCoord::new(z as u8, x, y)
            .with_context(|| format!("invalid tile coordinate {z}/{x}/{y}"))?;

        // Identical tiles are deduplicated and run-length encoded here.
        self.inner
            .add_raw_tile(coord, &gzip(mvt)?)
            .with_context(|| format!("writing tile {z}/{x}/{y}"))
    }

    fn batch_size(&self) -> usize {
        // Keeps every worker busy on ordinary tiles of tens of KB.
        self.workers * 8
    }

    fn batch_bytes(&self) -> usize {
        // A weight ceiling for unusually large tiles, independent of worker
        // count: eight of the largest tiles, under 64 MB across both copies.
        // So far the count always binds first.
        32 * 1024 * 1024
    }

    fn write_tiles(&mut self, batch: &[super::FinishedTile]) -> Result<()> {
        // Compressed in parallel, written in order: PMTiles marks an archive
        // unclustered on the first tile out of order.
        let mut compressed: Vec<Result<Vec<u8>>> = Vec::new();
        let share = batch.len().div_ceil(self.workers.max(1));
        std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .chunks(share.max(1))
                .map(|part| scope.spawn(move || part.iter().map(|(_, mvt)| gzip(mvt)).collect()))
                .collect();
            for handle in handles {
                let part: Vec<Result<Vec<u8>>> = handle.join().expect("a compressing thread");
                compressed.extend(part);
            }
        });

        for (((z, x, y), _), bytes) in batch.iter().zip(compressed) {
            let coord = TileCoord::new(*z as u8, *x, *y)
                .with_context(|| format!("invalid tile coordinate {z}/{x}/{y}"))?;
            self.inner
                .add_raw_tile(coord, &bytes?)
                .with_context(|| format!("writing tile {z}/{x}/{y}"))?;
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        self.inner
            .finalize()
            .context("finalising the PMTiles archive")
    }
}

/// Gzip one tile, at the level PMTiles' own writer uses.
fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Metadata {
        Metadata {
            name: "test".to_string(),
            // Not this crate's version: the writer records what it's given.
            generator: Some("test-harness 9.9.9".to_string()),
            min_zoom: 0,
            max_zoom: 2,
            bounds: None,
            antimeridian_adjusted_bounds: None,
            center: None,
            layers: Vec::new(),
        }
    }

    /// PMTiles v3 files start with `PMTiles` and a version byte.
    fn assert_is_pmtiles_v3(path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[..7], b"PMTiles", "should carry the PMTiles magic");
        assert_eq!(bytes[7], 3, "should declare spec version 3");
    }

    #[test]
    fn writes_a_pmtiles_v3_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.pmtiles");

        let mut writer = PmtilesWriter::create(&path, &meta()).unwrap();
        writer
            .write_tile(0, 0, 0, b"pretend this is a tile")
            .unwrap();
        Box::new(writer).finish().unwrap();

        assert_is_pmtiles_v3(&path);
    }

    #[test]
    fn the_json_metadata_names_what_generated_the_tileset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.pmtiles");

        let writer = PmtilesWriter::create(&path, &meta()).unwrap();
        Box::new(writer).finish().unwrap();

        // Read from the metadata section the header points to.
        let bytes = std::fs::read(&path).unwrap();
        let at = |offset: usize| {
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize
        };
        let (offset, length) = (at(24), at(32));
        let blob = &bytes[offset..offset + length];
        let json: serde_json::Value = if blob.starts_with(&[0x1f, 0x8b]) {
            let mut out = Vec::new();
            std::io::copy(
                &mut flate2::read::GzDecoder::new(blob),
                &mut std::io::Cursor::new(&mut out),
            )
            .unwrap();
            serde_json::from_slice(&out).unwrap()
        } else {
            serde_json::from_slice(blob).unwrap()
        };

        assert_eq!(json["generator"], "test-harness 9.9.9");
    }

    #[test]
    fn a_tileset_with_no_generator_omits_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anonymous.pmtiles");
        let mut anonymous = meta();
        anonymous.generator = None;

        let writer = PmtilesWriter::create(&path, &anonymous).unwrap();
        Box::new(writer).finish().unwrap();

        assert_is_pmtiles_v3(&path);
    }

    #[test]
    fn an_archive_with_no_tiles_is_still_valid() {
        // A zoom level can legitimately be empty, so an empty archive is fine.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.pmtiles");

        let writer = PmtilesWriter::create(&path, &meta()).unwrap();
        Box::new(writer).finish().unwrap();

        assert_is_pmtiles_v3(&path);
    }

    #[test]
    fn replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.pmtiles");
        std::fs::write(&path, b"stale rubbish that is not a pmtiles archive").unwrap();

        let mut writer = PmtilesWriter::create(&path, &meta()).unwrap();
        writer
            .write_tile(1, 0, 0, b"pretend this is a tile")
            .unwrap();
        Box::new(writer).finish().unwrap();

        assert_is_pmtiles_v3(&path);
    }
}
