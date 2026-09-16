//! Writing tiles into an MBTiles (SQLite) archive.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Metadata, TileWriter};
use flate2::{Compression, write::GzEncoder};
use rusqlite::Connection;

/// Writes MVT tiles into an MBTiles archive.
pub struct MbtilesWriter {
    conn: Connection,
}

impl MbtilesWriter {
    /// Create a new archive at `path`, replacing any existing file.
    pub fn create(path: &Path, metadata: &Metadata) -> Result<Self> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }

        let conn = Connection::open(path)?;
        // One transaction for the whole archive, committed by `finish`: a
        // commit per tile is a durability barrier per tile.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE metadata (name TEXT, value TEXT);
             CREATE TABLE tiles (
                 zoom_level INTEGER,
                 tile_column INTEGER,
                 tile_row INTEGER,
                 tile_data BLOB
             );
             CREATE UNIQUE INDEX tile_index
                 ON tiles (zoom_level, tile_column, tile_row);",
        )?;

        let rows = [
            ("name", metadata.name.clone()),
            ("format", "pbf".to_string()),
            ("type", "overlay".to_string()),
            ("version", "2".to_string()),
            ("minzoom", metadata.min_zoom.to_string()),
            ("maxzoom", metadata.max_zoom.to_string()),
        ];

        // TileJSON's spelling. Without them a client assumes the whole world.
        let mut rows: Vec<(&str, String)> = rows.to_vec();
        if let Some(generator) = &metadata.generator {
            rows.push(("generator", generator.clone()));
        }
        if let Some(bounds) = metadata.bounds {
            rows.push(("bounds", super::bounds_string(bounds)));
        }
        if let Some(adjusted) = metadata.antimeridian_adjusted_bounds {
            rows.push((
                "antimeridian_adjusted_bounds",
                super::bounds_string(adjusted),
            ));
        }
        if let Some((lon, lat, zoom)) = metadata.center {
            rows.push(("center", format!("{lon},{lat},{zoom}")));
        }
        // MBTiles keeps TileJSON's extra fields in one `json` row.
        if !metadata.layers.is_empty() {
            rows.push((
                "json",
                serde_json::json!({
                    "vector_layers": super::vector_layers_json(&metadata.layers)
                })
                .to_string(),
            ));
        }
        for (key, value) in rows {
            conn.execute(
                "INSERT INTO metadata (name, value) VALUES (?1, ?2)",
                (key, value),
            )?;
        }

        Ok(Self { conn })
    }
}

impl TileWriter for MbtilesWriter {
    fn write_tile(&mut self, z: u32, x: u32, y: u32, mvt: &[u8]) -> Result<()> {
        let tiles_per_axis = 1u32
            .checked_shl(z)
            .with_context(|| format!("zoom {z} is out of range"))?;
        if x >= tiles_per_axis || y >= tiles_per_axis {
            bail!("tile {z}/{x}/{y} is outside zoom {z}, which is {tiles_per_axis} tiles square");
        }

        // TMS counts rows from the south, XYZ from the north.
        let tms_row = tiles_per_axis - 1 - y;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(mvt)?;
        let gzipped = encoder.finish()?;

        self.conn
            .prepare_cached(
                "INSERT INTO tiles (zoom_level, tile_column, tile_row, tile_data)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute((z, x, tms_row, gzipped))?;
        Ok(())
    }

    /// Commit. Without it the archive holds no tiles.
    fn finish(self: Box<Self>) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn meta(name: &str, min_zoom: u32, max_zoom: u32) -> Metadata {
        Metadata {
            name: name.to_string(),
            // Not this crate's version: the writer records what it's given.
            generator: Some("test-harness 9.9.9".to_string()),
            min_zoom,
            max_zoom,
            bounds: None,
            antimeridian_adjusted_bounds: None,
            center: None,
            layers: Vec::new(),
        }
    }

    /// Read back the single row of the `tiles` table.
    fn only_tile(path: &Path) -> (u32, u32, u32, Vec<u8>) {
        let conn = Connection::open(path).unwrap();
        conn.query_row(
            "SELECT zoom_level, tile_column, tile_row, tile_data FROM tiles",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    }

    fn metadata(path: &Path, key: &str) -> String {
        let conn = Connection::open(path).unwrap();
        conn.query_row("SELECT value FROM metadata WHERE name = ?1", [key], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[test]
    fn stores_a_tile_with_a_tms_flipped_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.mbtiles");

        let mut writer = MbtilesWriter::create(&path, &meta("test", 0, 1)).unwrap();
        // XYZ 1/0/0 is the north-west tile: TMS row 1.
        writer
            .write_tile(1, 0, 0, b"pretend this is a tile")
            .unwrap();
        Box::new(writer).finish().unwrap();

        let (z, column, row, _) = only_tile(&path);
        assert_eq!((z, column, row), (1, 0, 1));
    }

    #[test]
    fn stores_tile_data_gzipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.mbtiles");

        let mut writer = MbtilesWriter::create(&path, &meta("test", 0, 0)).unwrap();
        writer
            .write_tile(0, 0, 0, b"pretend this is a tile")
            .unwrap();
        Box::new(writer).finish().unwrap();

        let (_, _, _, data) = only_tile(&path);
        assert_eq!(
            &data[..2],
            &[0x1f, 0x8b],
            "should carry a gzip magic number"
        );

        let mut decoded = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&data[..]), &mut decoded)
            .unwrap();
        assert_eq!(decoded, b"pretend this is a tile");
    }

    #[test]
    fn records_the_metadata_a_reader_needs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.mbtiles");

        Box::new(MbtilesWriter::create(&path, &meta("my tiles", 3, 9)).unwrap())
            .finish()
            .unwrap();

        assert_eq!(metadata(&path, "name"), "my tiles");
        assert_eq!(metadata(&path, "format"), "pbf");
        assert_eq!(metadata(&path, "minzoom"), "3");
        assert_eq!(metadata(&path, "maxzoom"), "9");
        assert_eq!(metadata(&path, "generator"), "test-harness 9.9.9");
    }

    #[test]
    fn antimeridian_adjusted_bounds_get_their_own_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("across.mbtiles");
        let mut across = meta("across", 0, 2);
        across.bounds = Some([-180.0, -40.0, 180.0, -38.0]);
        across.antimeridian_adjusted_bounds = Some([175.0, -40.0, 185.0, -38.0]);

        Box::new(MbtilesWriter::create(&path, &across).unwrap())
            .finish()
            .unwrap();

        assert_eq!(metadata(&path, "bounds"), "-180,-40,180,-38");
        assert_eq!(
            metadata(&path, "antimeridian_adjusted_bounds"),
            "175,-40,185,-38"
        );
    }

    #[test]
    fn a_tileset_with_no_generator_has_no_generator_row() {
        // Absent, not an empty string.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anonymous.mbtiles");
        let mut anonymous = meta("my tiles", 0, 2);
        anonymous.generator = None;

        Box::new(MbtilesWriter::create(&path, &anonymous).unwrap())
            .finish()
            .unwrap();

        let conn = Connection::open(&path).unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM metadata WHERE name = 'generator'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
    }
}
