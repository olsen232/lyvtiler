//! The tile store: an SQLite file holding the pyramid between stages, plus
//! each feature's attributes and rank.
//!
//! **An empty tile is a real tile.** A tile whose inputs existed but whose
//! output collapsed is stored as a zero-length blob and reads back as
//! `Some(&[])`, not `None`. The pipeline only visits tiles that exist, so
//! absence means "no input here".

use anyhow::{Context, Result};
use lyv_core::TileCoord;

pub use lyv_core::{Schema, Value};
use rusqlite::{Connection, OptionalExtension};

/// A place to keep tiles between pipeline stages.
pub trait TileStore {
    /// Store one tile, replacing any at that address. A zero-length `data` must
    /// be stored (see the module docs).
    fn put(&mut self, coord: TileCoord, data: &[u8]) -> Result<()>;

    /// Read one tile back. `None` means no tile was ever stored there.
    fn get(&self, coord: TileCoord) -> Result<Option<Vec<u8>>>;

    /// Visit every tile at one zoom in Hilbert order, the order PMTiles numbers
    /// tiles in (§11). A callback, since a level can hold millions of tiles.
    fn for_each_at(
        &self,
        z: u32,
        visit: &mut dyn FnMut(TileCoord, Vec<u8>) -> Result<()>,
    ) -> Result<()>;

    /// Commit anything buffered, so another connection can see it. Reads
    /// through this handle flush on their own.
    fn flush(&self) -> Result<()>;

    /// Which zooms hold any tiles, ascending. Small by construction.
    fn zooms(&self) -> Result<Vec<u32>>;

    /// How many tiles the store holds, over every zoom, for progress totals.
    fn count(&self) -> Result<u64>;

    /// Record a feature's attributes, replacing any held for that id.
    ///
    /// Once per feature, not per piece: a feature can have pieces in thousands
    /// of tiles. New key names join the [`schema`](Self::schema), so GeoJSON
    /// needs nothing declared up front.
    fn put_feature(&mut self, id: u64, properties: &[(String, Value)]) -> Result<()>;

    /// The key names every row indexes into. Only ever appended to, so old rows
    /// stay readable.
    fn schema(&self) -> &Schema;

    /// A feature's attributes, or `None` if it has none recorded.
    fn feature(&self, id: u64) -> Result<Option<Vec<(String, Value)>>>;

    /// Record the drop ranking, replacing any held: half of one ranking mixed
    /// with half of another isn't a ranking.
    ///
    /// Each entry is `(id, rank, attribute bytes)`. The attribute cost rides
    /// along because thinning needs it to estimate encoded tile size.
    fn put_ranks(&mut self, ranks: &[(u64, u32, u32)]) -> Result<()>;

    /// A feature's rank and attribute cost, or `None` if not ranked.
    fn rank(&self, id: u64) -> Result<Option<(u32, u32)>>;

    /// Every feature's attributes, as stored, in one pass. Undecoded, so the
    /// caller can decode across every core; see [`for_each_property`] and
    /// [`schema`](Self::schema).
    fn for_each_feature(&self, visit: &mut dyn FnMut(u64, &[u8]) -> Result<()>) -> Result<()>;

    /// Every feature's rank and attribute cost, for thinning.
    fn for_each_rank(&self, visit: &mut dyn FnMut(u64, u32, u32) -> Result<()>) -> Result<()>;
}

/// Writes per commit. Each commit is a durability barrier: 19,769 rows/s
/// committing each against 218,606 batched. Batched rather than one transaction
/// for the run, which would grow the WAL without limit and lose everything on
/// a crash.
const BATCH: usize = 10_000;

/// A [`TileStore`] backed by one SQLite file.
pub struct SqliteStore {
    conn: Connection,
    /// Writes since the last commit. A `Cell` because reads flush too, and
    /// reads only have `&self`: a written row must always read back.
    pending: std::cell::Cell<usize>,
    /// The key names every attribute row indexes into.
    schema: Schema,
    /// Whether `schema` has grown since it was saved. It must reach the file
    /// before any row indexing into it is read, so it's saved with the flush
    /// every read performs.
    schema_dirty: std::cell::Cell<bool>,
}

/// Where the attribute schema is kept in the `job` table.
const SCHEMA_PARAM: &str = "schema";

/// How much of the store to memory-map by default: nothing.
///
/// Mapping is about 2% faster on floodplains and no faster on parcels. It's
/// off because its description takes three sentences that undo each other:
///
/// - the extra memory makes things go faster;
/// - but it is not really memory, because the kernel can reclaim it;
/// - but if it keeps being reclaimed, things run slower again.
///
/// Everything that reads resident set believes only the first. `--mmap` turns
/// it on.
pub const DEFAULT_MMAP_BYTES: u64 = 0;

impl SqliteStore {
    /// Open or create a store at `path`, mapping [`DEFAULT_MMAP_BYTES`] of it.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        Self::open_mapping(path, DEFAULT_MMAP_BYTES)
    }

    /// Open or create a store at `path`, mapping at most `mmap_bytes` of it.
    /// Zero maps nothing (see [`DEFAULT_MMAP_BYTES`]).
    pub fn open_mapping(path: &std::path::Path, mmap_bytes: u64) -> Result<Self> {
        Self::from_connection(
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?,
            mmap_bytes,
        )
    }

    /// A store that exists only for the life of the process. For tests.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, 0)
    }

    /// Settings for a scratch file of large blobs, deleted on success. SQLite's
    /// defaults suit a long-lived database of small rows; with them the store
    /// was 39.5% of floodplains' thread-time.
    ///
    /// Must run before anything touches the file: `page_size` can only be set
    /// while it's empty.
    fn tune(conn: &Connection, mmap_bytes: u64) -> Result<()> {
        conn.execute_batch(
            // Tiles average 16.5 KB; at 4 KB pages each read chases overflow
            // pages. 64 KB is SQLite's maximum. Ignored on an existing file,
            // which keeps its page size and still works.
            "PRAGMA page_size = 65536;
             PRAGMA journal_mode = WAL;
             -- In WAL mode this stops fsyncing on every commit, leaving it to
             -- the checkpoint. It gives up durability across a *power* loss,
             -- which this file does not need — it is scratch, deleted on
             -- success, and a lost one only costs a re-run.
             PRAGMA synchronous = NORMAL;
             -- Per connection, and encoding opens one per worker, so this is
             -- five times what it says during that stage. Kept modest for
             -- that reason: 64 MB is enough to hold the working set of a
             -- Hilbert sweep without moving the run's memory peak, which sits
             -- in `reading` where only one connection exists.
             PRAGMA cache_size = -65536;
             PRAGMA temp_store = MEMORY;",
        )?;
        // Reads from the page cache without a copy. Shows in resident set, not
        // in `--memory`. Interpolated because `PRAGMA` takes no parameters;
        // it's a `u64`, so nothing can be injected.
        conn.execute_batch(&format!("PRAGMA mmap_size = {mmap_bytes};"))?;
        Ok(())
    }

    fn from_connection(conn: Connection, mmap_bytes: u64) -> Result<Self> {
        Self::tune(&conn, mmap_bytes)?;

        // WITHOUT ROWID: a lookup is one B-tree probe. Keyed by Hilbert id, so
        // a level reads back in PMTiles order.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS job (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS features (
                 id    INTEGER PRIMARY KEY,
                 attrs BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ranks (
                 id         INTEGER PRIMARY KEY,
                 rank       INTEGER NOT NULL,
                 attr_bytes INTEGER NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS tiles (
                 z       INTEGER NOT NULL,
                 tile_id INTEGER NOT NULL,
                 y       INTEGER NOT NULL,
                 x       INTEGER NOT NULL,
                 data    BLOB NOT NULL,
                 PRIMARY KEY (z, tile_id)
             ) WITHOUT ROWID;
             -- Keys without the blobs, purely so that counting is cheap.
             --
             -- `WITHOUT ROWID` puts each row *inside* the primary-key B-tree,
             -- which is what makes a lookup one probe instead of two — and
             -- what makes `SELECT count(*)` read every tile's bytes on the way
             -- past, since the leaves it has to walk are the data. On 3.9M
             -- floodplain features that is a multi-gigabyte scan to learn a
             -- number, and it ran twice: once to tell the encode pass how many
             -- tiles to expect, once for the archive pass. Fifty-two seconds
             -- of an eight-minute run, inside a stage that looked slow for
             -- reasons nobody could see.
             --
             -- Measured on a 744 MB store: 0.52s to count cold, 0.00s through
             -- this index, which costs 127 KB.
             CREATE INDEX IF NOT EXISTS tiles_z ON tiles (z);",
        )
        .context("creating the tile store schema")?;

        // Read directly rather than through `flush`, which writes it back.
        let recorded: Option<String> = conn
            .query_row(
                "SELECT value FROM job WHERE key = ?1",
                (SCHEMA_PARAM,),
                |row| row.get(0),
            )
            .optional()?;
        let schema = match recorded.as_deref() {
            Some(text) => Schema::from_names(
                serde_json::from_str::<Vec<String>>(text)
                    .context("reading the attribute schema back")?,
            ),
            None => Schema::default(),
        };

        Ok(Self {
            conn,
            pending: std::cell::Cell::new(0),
            schema,
            schema_dirty: std::cell::Cell::new(false),
        })
    }

    /// Write the schema to `job`, as JSON: a key name could contain a newline.
    fn save_schema(&self) -> Result<()> {
        let names: Vec<&str> = self.schema.names().collect();
        self.conn.execute(
            "INSERT OR REPLACE INTO job (key, value) VALUES (?1, ?2)",
            (SCHEMA_PARAM, serde_json::to_string(&names)?),
        )?;
        Ok(())
    }

    /// Count one write, opening a transaction if needed and committing when the
    /// batch is full.
    fn wrote(&self) -> Result<()> {
        if self.pending.get() == 0 {
            self.conn.execute_batch("BEGIN")?;
        }
        self.pending.set(self.pending.get() + 1);
        if self.pending.get() >= BATCH {
            // Via `flush`, so a grown schema is committed with its rows.
            TileStore::flush(self)?;
        }
        Ok(())
    }
}

impl Drop for SqliteStore {
    /// Commit whatever is buffered, so a failed run's store keeps its last
    /// rows. A successful run flushes first, where a failure can be reported.
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl TileStore for SqliteStore {
    fn put(&mut self, coord: TileCoord, data: &[u8]) -> Result<()> {
        self.wrote()?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO tiles (z, tile_id, y, x, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (coord.z, coord.hilbert_id() as i64, coord.y, coord.x, data),
            )
            .with_context(|| format!("storing tile {}/{}/{}", coord.z, coord.x, coord.y))?;
        Ok(())
    }

    fn get(&self, coord: TileCoord) -> Result<Option<Vec<u8>>> {
        self.flush()?;
        self.conn
            .query_row(
                "SELECT data FROM tiles WHERE z = ?1 AND tile_id = ?2",
                (coord.z, coord.hilbert_id() as i64),
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
            .with_context(|| format!("reading tile {}/{}/{}", coord.z, coord.x, coord.y))
    }

    fn for_each_at(
        &self,
        z: u32,
        visit: &mut dyn FnMut(TileCoord, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        self.flush()?;
        let mut statement = self
            .conn
            .prepare("SELECT y, x, data FROM tiles WHERE z = ?1 ORDER BY tile_id")?;
        let mut rows = statement.query((z,))?;
        while let Some(row) = rows.next()? {
            let coord = TileCoord {
                z,
                y: row.get(0)?,
                x: row.get(1)?,
            };
            // The visitor's error propagates, so a failed level doesn't look
            // finished.
            visit(coord, row.get(2)?)?;
        }
        Ok(())
    }

    fn count(&self) -> Result<u64> {
        self.flush()?;
        let total: i64 = self
            .conn
            .query_row("SELECT count(*) FROM tiles", (), |row| row.get(0))?;
        Ok(total as u64)
    }

    fn zooms(&self) -> Result<Vec<u32>> {
        self.flush()?;
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT z FROM tiles ORDER BY z")?;
        let zooms = statement
            .query_map((), |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<u32>>>()?;
        Ok(zooms)
    }

    fn flush(&self) -> Result<()> {
        // Before the commit, so that the schema and the rows that index into
        // it land in the same transaction. A row visible without its names
        // would decode to nothing.
        if self.schema_dirty.get() {
            self.save_schema()?;
            self.schema_dirty.set(false);
        }
        if self.pending.get() > 0 {
            self.conn.execute_batch("COMMIT")?;
            self.pending.set(0);
        }
        Ok(())
    }

    fn put_feature(&mut self, id: u64, properties: &[(String, Value)]) -> Result<()> {
        let before = self.schema.len();
        let attrs = encode_properties(&mut self.schema, properties);
        if self.schema.len() != before {
            self.schema_dirty.set(true);
        }
        self.wrote()?;
        self.conn.execute(
            "INSERT OR REPLACE INTO features (id, attrs) VALUES (?1, ?2)",
            (id as i64, attrs),
        )?;
        Ok(())
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn feature(&self, id: u64) -> Result<Option<Vec<(String, Value)>>> {
        // A write must always be readable straight back.
        self.flush()?;
        let stored: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT attrs FROM features WHERE id = ?1",
                [id as i64],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored.map(|attrs| decode_properties(&self.schema, &attrs)))
    }

    fn put_ranks(&mut self, ranks: &[(u64, u32, u32)]) -> Result<()> {
        // One transaction: a million single-statement inserts would be a
        // million fsyncs, and a half-written ranking is worse than none.
        //
        // Flushed first, because `transaction()` cannot nest inside the batch
        // the ordinary writes keep open.
        self.flush()?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM ranks", ())?;
        {
            let mut insert =
                tx.prepare("INSERT INTO ranks (id, rank, attr_bytes) VALUES (?1, ?2, ?3)")?;
            for (id, rank, attr_bytes) in ranks {
                insert.execute((*id as i64, *rank, *attr_bytes))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn rank(&self, id: u64) -> Result<Option<(u32, u32)>> {
        // A write must always be readable straight back.
        self.flush()?;
        Ok(self
            .conn
            .query_row(
                "SELECT rank, attr_bytes FROM ranks WHERE id = ?1",
                [id as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    fn for_each_feature(&self, visit: &mut dyn FnMut(u64, &[u8]) -> Result<()>) -> Result<()> {
        self.flush()?;
        let mut statement = self.conn.prepare("SELECT id, attrs FROM features")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            // Borrowed from SQLite's page, not copied: this runs millions of
            // times and the caller copies into its arena anyway.
            visit(id as u64, row.get_ref(1)?.as_blob()?)?;
        }
        Ok(())
    }

    fn for_each_rank(&self, visit: &mut dyn FnMut(u64, u32, u32) -> Result<()>) -> Result<()> {
        self.flush()?;
        let mut query = self
            .conn
            .prepare("SELECT id, rank, attr_bytes FROM ranks")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            visit(row.get::<_, i64>(0)? as u64, row.get(1)?, row.get(2)?)?;
        }
        Ok(())
    }
}

/// The attribute row format: per attribute, a LEB128 index into the
/// [`Schema`], a one-byte type tag, then the payload.
///
/// It was JSON, and about 63% of every row was key names, type tags and
/// punctuation repeated on all 3.9M rows. The names now live once in the
/// `Schema`, persisted in `job`, which only ever grows, so an early row stays
/// readable after a later feature adds a name (GeoJSON has no header).
///
/// Booleans are two tags with no payload. Strings carry a LEB128 length;
/// integers are LEB128, zigzagged where signed. The types are MVT's, kept
/// distinct because MVT cares about width and signedness.
mod tag {
    pub const STR: u8 = 1;
    pub const FLOAT: u8 = 2;
    pub const DOUBLE: u8 = 3;
    pub const INT: u8 = 4;
    pub const UINT: u8 = 5;
    pub const SINT: u8 = 6;
    pub const FALSE: u8 = 7;
    pub const TRUE: u8 = 8;
}

/// Append a LEB128 unsigned integer.
fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Read a LEB128 unsigned integer, advancing `at`. `None` if the blob ends
/// mid-number, so a truncated row stops rather than loops.
fn take_varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        // Ten groups of seven cover a u64; more means a corrupt blob.
        if shift >= 70 {
            return None;
        }
    }
}

/// Zigzag, so a small negative is a small varint.
fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Encode one feature's attributes, adding any new names to the schema.
fn encode_properties(schema: &mut Schema, properties: &[(String, Value)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in properties {
        put_varint(&mut out, u64::from(schema.of(key)));
        match value {
            Value::Str(s) => {
                out.push(tag::STR);
                put_varint(&mut out, s.len() as u64);
                out.extend_from_slice(s.as_bytes());
            }
            Value::Float(f) => {
                out.push(tag::FLOAT);
                out.extend_from_slice(&f.to_le_bytes());
            }
            Value::Double(d) => {
                out.push(tag::DOUBLE);
                out.extend_from_slice(&d.to_le_bytes());
            }
            Value::Int(i) => {
                out.push(tag::INT);
                put_varint(&mut out, zigzag(*i));
            }
            Value::Uint(u) => {
                out.push(tag::UINT);
                put_varint(&mut out, *u);
            }
            Value::Sint(i) => {
                out.push(tag::SINT);
                put_varint(&mut out, zigzag(*i));
            }
            Value::Bool(b) => out.push(if *b { tag::TRUE } else { tag::FALSE }),
        }
    }
    out
}

/// Visit one stored row as `(schema index, value)` pairs, without resolving
/// names — the encode pass interns by index, so a `String` per attribute would
/// be wasted. Public because that pass decodes the table itself, across cores.
///
/// Panics on a row that doesn't decode: the same run wrote it, so that's a
/// bug, and carrying on would drop the row's attributes without a word.
pub fn for_each_property(bytes: &[u8], visit: &mut dyn FnMut(u32, Value)) {
    let mut at = 0;
    while at < bytes.len() {
        let Some(index) = take_varint(bytes, &mut at).and_then(|i| u32::try_from(i).ok()) else {
            malformed(bytes, at);
        };
        let Some(&tag) = bytes.get(at) else {
            malformed(bytes, at)
        };
        at += 1;
        let value = match tag {
            tag::STR => {
                let Some(len) = take_varint(bytes, &mut at).map(|l| l as usize) else {
                    malformed(bytes, at);
                };
                let Some(raw) = bytes.get(at..at + len) else {
                    malformed(bytes, at);
                };
                at += len;
                let Ok(text) = std::str::from_utf8(raw) else {
                    malformed(bytes, at);
                };
                Value::Str(text.to_string())
            }
            tag::FLOAT => {
                let Some(raw) = bytes.get(at..at + 4) else {
                    malformed(bytes, at);
                };
                at += 4;
                Value::Float(f32::from_le_bytes(raw.try_into().expect("four bytes")))
            }
            tag::DOUBLE => {
                let Some(raw) = bytes.get(at..at + 8) else {
                    malformed(bytes, at);
                };
                at += 8;
                Value::Double(f64::from_le_bytes(raw.try_into().expect("eight bytes")))
            }
            tag::INT => match take_varint(bytes, &mut at) {
                Some(raw) => Value::Int(unzigzag(raw)),
                None => malformed(bytes, at),
            },
            tag::UINT => match take_varint(bytes, &mut at) {
                Some(raw) => Value::Uint(raw),
                None => malformed(bytes, at),
            },
            tag::SINT => match take_varint(bytes, &mut at) {
                Some(raw) => Value::Sint(unzigzag(raw)),
                None => malformed(bytes, at),
            },
            tag::FALSE => Value::Bool(false),
            tag::TRUE => Value::Bool(true),
            _ => malformed(bytes, at),
        };
        visit(index, value);
    }
}

/// A stored attribute row that doesn't decode.
fn malformed(bytes: &[u8], at: usize) -> ! {
    panic!(
        "a stored attribute row is malformed at byte {at} of {}",
        bytes.len()
    )
}

/// As [`for_each_property`], with names resolved.
pub fn decode_properties(schema: &Schema, bytes: &[u8]) -> Vec<(String, Value)> {
    let mut properties = Vec::new();
    for_each_property(bytes, &mut |index, value| {
        let name = schema.name(index).expect("a stored index is in the schema");
        properties.push((name.to_string(), value));
    });
    properties
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn tile(z: u32, x: u32, y: u32) -> TileCoord {
        TileCoord { z, x, y }
    }

    fn store() -> SqliteStore {
        SqliteStore::in_memory().unwrap()
    }

    #[test]
    fn a_stored_tile_reads_back_byte_for_byte() {
        let mut s = store();
        s.put(tile(3, 1, 2), b"some geometry").unwrap();
        assert_eq!(
            s.get(tile(3, 1, 2)).unwrap().as_deref(),
            Some(&b"some geometry"[..])
        );
    }

    #[test]
    fn an_address_never_written_reads_as_none() {
        let s = store();
        assert!(s.get(tile(3, 1, 2)).unwrap().is_none());
    }

    #[test]
    fn an_empty_tile_is_not_the_same_as_an_absent_one() {
        // A computed-but-empty tile must be distinguishable from a region never
        // visited.
        let mut s = store();
        s.put(tile(5, 9, 9), b"").unwrap();

        assert_eq!(
            s.get(tile(5, 9, 9)).unwrap(),
            Some(Vec::new()),
            "an empty tile must read back as Some(empty), not None"
        );
        assert_eq!(
            s.get(tile(5, 9, 8)).unwrap(),
            None,
            "its neighbour was never written and must read back as None"
        );
    }

    #[test]
    fn writing_a_tile_twice_replaces_it() {
        // Rewriting a tile must be safe.
        let mut s = store();
        s.put(tile(1, 0, 1), b"first").unwrap();
        s.put(tile(1, 0, 1), b"second").unwrap();
        assert_eq!(
            s.get(tile(1, 0, 1)).unwrap().as_deref(),
            Some(&b"second"[..])
        );
    }

    #[test]
    fn a_level_is_walked_in_hilbert_order() {
        // Hilbert order, so a level copies straight into a clustered PMTiles.
        let mut s = store();
        for (x, y) in [(1, 1), (0, 1), (2, 0), (0, 0)] {
            s.put(tile(4, x, y), b"x").unwrap();
        }
        s.put(tile(5, 0, 0), b"other zoom").unwrap();

        let mut seen = Vec::new();
        s.for_each_at(4, &mut |coord, _| {
            seen.push((coord.x, coord.y));
            Ok(())
        })
        .unwrap();

        let mut expected = vec![(1, 1), (0, 1), (2, 0), (0, 0)];
        expected.sort_by_key(|&(x, y)| tile(4, x, y).hilbert_id());
        assert_eq!(seen, expected);
        // Stated outright, so a broken `hilbert_id` can't agree with itself.
        assert_eq!(seen, vec![(0, 0), (1, 1), (0, 1), (2, 0)]);
    }

    #[test]
    fn a_level_walks_in_ascending_id_order_across_a_whole_grid() {
        // Whatever the insertion order, ids come back ascending, or PMTiles
        // marks the archive unclustered.
        let mut s = store();
        // Scattered on purpose.
        for i in 0..64u32 {
            let (x, y) = ((i * 37) % 8, (i * 53) % 8);
            s.put(tile(3, x, y), b"x").unwrap();
        }

        let mut ids = Vec::new();
        s.for_each_at(3, &mut |coord, _| {
            ids.push(coord.hilbert_id());
            Ok(())
        })
        .unwrap();

        assert!(ids.len() > 1);
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "ids came back out of order: {ids:?}"
        );
    }

    #[test]
    fn walking_a_level_yields_the_tile_data() {
        let mut s = store();
        s.put(tile(2, 3, 3), b"payload").unwrap();

        let mut seen = Vec::new();
        s.for_each_at(2, &mut |_, data| {
            seen.push(data);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec![b"payload".to_vec()]);
    }

    #[test]
    fn a_failing_visitor_stops_the_walk_and_reports() {
        let mut s = store();
        for x in 0..5 {
            s.put(tile(3, x, 0), b"x").unwrap();
        }
        let mut count = 0;
        let result = s.for_each_at(3, &mut |_, _| {
            count += 1;
            if count == 3 {
                anyhow::bail!("visitor gave up");
            }
            Ok(())
        });
        assert!(result.is_err(), "the error must not be swallowed");
        assert_eq!(count, 3, "and the walk must stop there");
    }

    #[test]
    fn zooms_lists_only_levels_that_hold_tiles() {
        let mut s = store();
        s.put(tile(7, 0, 0), b"x").unwrap();
        s.put(tile(2, 0, 0), b"x").unwrap();

        assert_eq!(s.zooms().unwrap(), vec![2, 7]);
    }

    #[test]
    fn a_store_survives_being_closed_and_reopened() {
        // A kept store reopens with its tiles.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiles.sqlite");

        {
            let mut s = SqliteStore::open(&path).unwrap();
            s.put(tile(6, 4, 5), b"survives").unwrap();
            s.put(tile(6, 4, 6), b"").unwrap();
        }

        let s = SqliteStore::open(&path).unwrap();
        assert_eq!(
            s.get(tile(6, 4, 5)).unwrap().as_deref(),
            Some(&b"survives"[..])
        );
        assert_eq!(
            s.get(tile(6, 4, 6)).unwrap(),
            Some(Vec::new()),
            "an empty tile must survive as an empty tile, not vanish"
        );
    }

    #[test]
    fn every_attribute_type_survives_the_side_table() {
        // MVT distinguishes integer widths and signedness; so must the store.
        let mut store = SqliteStore::in_memory().unwrap();
        let properties = vec![
            ("name".to_string(), Value::Str("Wairarapa".into())),
            ("count".to_string(), Value::Int(-78)),
            ("unsigned".to_string(), Value::Uint(u64::MAX)),
            ("signed".to_string(), Value::Sint(-5)),
            ("depth".to_string(), Value::Double(2.5)),
            ("ratio".to_string(), Value::Float(0.25)),
            ("tidal".to_string(), Value::Bool(true)),
        ];

        store.put_feature(42, &properties).unwrap();
        assert_eq!(store.feature(42).unwrap(), Some(properties));
    }

    #[test]
    fn a_feature_with_no_attributes_recorded_reads_as_none() {
        let store = SqliteStore::in_memory().unwrap();
        assert_eq!(store.feature(7).unwrap(), None);
    }

    #[test]
    fn recording_a_feature_twice_keeps_the_later_attributes() {
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .put_feature(1, &[("v".to_string(), Value::Int(1))])
            .unwrap();
        store
            .put_feature(1, &[("v".to_string(), Value::Int(2))])
            .unwrap();
        assert_eq!(
            store.feature(1).unwrap(),
            Some(vec![("v".to_string(), Value::Int(2))])
        );
    }

    #[test]
    fn attribute_order_is_preserved() {
        // The encoder writes tags in this order, so a stable order keeps tiles
        // reproducible.
        let mut store = SqliteStore::in_memory().unwrap();
        let properties = vec![
            ("z".to_string(), Value::Int(1)),
            ("a".to_string(), Value::Int(2)),
            ("m".to_string(), Value::Int(3)),
        ];
        store.put_feature(1, &properties).unwrap();
        assert_eq!(store.feature(1).unwrap(), Some(properties));
    }

    #[test]
    fn drop_ranks_round_trip() {
        // Every feature has a rank; only some have attributes.
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .put_ranks(&[(10, 0, 400), (20, 1, 250), (30, 2, 0)])
            .unwrap();
        assert_eq!(store.rank(20).unwrap(), Some((1, 250)));
        assert_eq!(store.rank(99).unwrap(), None);
    }

    #[test]
    fn recording_a_ranking_replaces_the_one_before_it() {
        // A re-rank mustn't leave half of an older ranking behind.
        let mut store = SqliteStore::in_memory().unwrap();
        store.put_ranks(&[(1, 0, 1), (2, 1, 1), (3, 2, 1)]).unwrap();
        store.put_ranks(&[(3, 0, 9), (1, 1, 9)]).unwrap();
        assert_eq!(store.rank(3).unwrap(), Some((0, 9)));
        assert_eq!(store.rank(1).unwrap(), Some((1, 9)));
        assert_eq!(store.rank(2).unwrap(), None, "a stale rank survived");
    }
}

#[cfg(test)]
mod batching {
    use super::tests::tile;
    use super::*;

    fn props() -> Vec<(String, Value)> {
        vec![("k".to_string(), Value::Int(1))]
    }

    #[test]
    fn a_write_is_readable_straight_back() {
        // Reads commit what's pending, so the open transaction is invisible.
        let mut s = SqliteStore::in_memory().unwrap();
        s.put_feature(7, &props()).unwrap();
        assert_eq!(s.feature(7).unwrap(), Some(props()));

        s.put(tile(3, 1, 2), b"tile").unwrap();
        assert_eq!(s.get(tile(3, 1, 2)).unwrap(), Some(b"tile".to_vec()));
    }

    #[test]
    fn dropping_the_store_commits_what_is_pending() {
        // Dropping the store must commit, not roll back the last rows.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let mut s = SqliteStore::open(&path).unwrap();
            for id in 0..100 {
                s.put_feature(id, &props()).unwrap();
            }
            // No read, so nothing has flushed.
        }
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(reopened.feature(99).unwrap(), Some(props()));
    }

    #[test]
    fn another_connection_sees_the_rows_after_a_flush() {
        // `flush` makes rows visible to other connections, as the parallel
        // encode pass uses.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut s = SqliteStore::open(&path).unwrap();
        s.put(tile(4, 2, 3), b"tile").unwrap();

        s.flush().unwrap();
        let other = SqliteStore::open(&path).unwrap();
        assert_eq!(other.get(tile(4, 2, 3)).unwrap(), Some(b"tile".to_vec()));
    }

    #[test]
    fn the_mapping_size_asked_for_is_the_one_sqlite_uses() {
        // Checked against SQLite, because an interpolated pragma it won't take
        // is silently ignored.
        let dir = tempfile::tempdir().unwrap();
        for asked in [0u64, 65_536, 2_000_000_000] {
            let path = dir.path().join(format!("{asked}.db"));
            let store = SqliteStore::open_mapping(&path, asked).unwrap();
            let got: i64 = store
                .conn
                .query_row("PRAGMA mmap_size", [], |row| row.get(0))
                .unwrap();
            // Rounded down to whole pages: never over what was asked.
            let got = got as u64;
            assert!(
                got <= asked && asked - got <= 65_536,
                "asked to map {asked} bytes, got {got}"
            );
        }
    }

    #[test]
    fn a_store_that_maps_nothing_still_reads_what_it_wrote() {
        // Mapping is a read path; turning it off mustn't change any answer.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unmapped.db");
        let mut store = SqliteStore::open_mapping(&path, 0).unwrap();
        store.put(tile(3, 1, 2), b"some geometry").unwrap();
        store
            .put_feature(7, &[("k".to_string(), Value::Int(1))])
            .unwrap();
        assert_eq!(
            store.get(tile(3, 1, 2)).unwrap().as_deref(),
            Some(&b"some geometry"[..])
        );
        assert_eq!(
            store.feature(7).unwrap(),
            Some(vec![("k".to_string(), Value::Int(1))])
        );
    }

    #[test]
    fn a_batch_larger_than_the_commit_interval_lands_whole() {
        // More than `BATCH` writes, so one commit happens mid-run and the rest
        // are pending. Both halves must be there.
        let mut s = SqliteStore::in_memory().unwrap();
        let total = BATCH + BATCH / 2;
        for id in 0..total as u64 {
            s.put_feature(id, &props()).unwrap();
        }
        assert_eq!(s.feature(0).unwrap(), Some(props()));
        assert_eq!(s.feature(total as u64 - 1).unwrap(), Some(props()));
    }
}

#[cfg(test)]
mod attribute_format {
    use super::*;

    /// One of everything, with values either side of the varint and zigzag
    /// boundaries.
    fn every_kind() -> Vec<(String, Value)> {
        vec![
            ("empty".into(), Value::Str(String::new())),
            ("text".into(), Value::Str("Feature Extraction".into())),
            ("unicode".into(), Value::Str("Ōtāhuhu — 1½".into())),
            ("small".into(), Value::Int(1)),
            ("at_127".into(), Value::Int(63)),
            ("at_128".into(), Value::Int(64)),
            ("negative".into(), Value::Int(-1)),
            ("very_negative".into(), Value::Int(i64::MIN)),
            ("huge".into(), Value::Uint(u64::MAX)),
            ("signed".into(), Value::Sint(-16_384)),
            ("float".into(), Value::Float(-0.5)),
            ("double".into(), Value::Double(1.0 / 3.0)),
            ("yes".into(), Value::Bool(true)),
            ("no".into(), Value::Bool(false)),
        ]
    }

    #[test]
    fn every_value_kind_survives_the_round_trip() {
        // `Int(1)`, `Uint(1)` and `Sint(1)` must each come back as themselves.
        let mut schema = Schema::default();
        let encoded = encode_properties(&mut schema, &every_kind());
        assert_eq!(decode_properties(&schema, &encoded), every_kind());
    }

    #[test]
    fn a_truncated_row_is_a_panic_not_fewer_attributes() {
        let mut schema = Schema::default();
        let encoded = encode_properties(&mut schema, &every_kind());
        let whole = every_kind().len();
        for cut in 1..encoded.len() {
            let part = &encoded[..cut];
            let decoding = std::panic::catch_unwind(|| {
                let mut decoded = 0;
                for_each_property(part, &mut |_, _| decoded += 1);
                decoded
            });
            // A cut can land exactly between two attributes, leaving a
            // shorter valid row: fewer attributes, but only whole ones.
            if let Ok(decoded) = decoding {
                assert!(decoded < whole, "truncated to {cut} bytes");
            }
        }
    }

    #[test]
    fn a_name_learned_late_leaves_earlier_rows_readable() {
        // A key first seen on the millionth feature mustn't shift any earlier
        // row's indices.
        let mut s = SqliteStore::in_memory().unwrap();
        let first = vec![("a".to_string(), Value::Int(1))];
        s.put_feature(1, &first).unwrap();

        let second = vec![
            ("b".to_string(), Value::Str("new".into())),
            ("a".to_string(), Value::Int(2)),
        ];
        s.put_feature(2, &second).unwrap();

        assert_eq!(s.feature(1).unwrap(), Some(first));
        assert_eq!(s.feature(2).unwrap(), Some(second));
    }

    #[test]
    fn the_schema_comes_back_with_the_store() {
        // Without the schema, reopened rows would read as no attributes at all.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let mut s = SqliteStore::open(&path).unwrap();
            s.put_feature(1, &every_kind()).unwrap();
        }
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(reopened.feature(1).unwrap(), Some(every_kind()));
        assert_eq!(reopened.schema().len(), every_kind().len());
    }

    #[test]
    fn the_schema_reaches_the_file_before_a_reader_does() {
        // The schema is saved per flush, so it could lag the rows. Only a second
        // connection can see that.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut s = SqliteStore::open(&path).unwrap();
        s.put_feature(1, &every_kind()).unwrap();
        s.flush().unwrap();

        let other = SqliteStore::open(&path).unwrap();
        assert_eq!(other.feature(1).unwrap(), Some(every_kind()));
    }

    #[test]
    fn the_whole_table_reads_back_as_stored() {
        // What `for_each_feature` hands over, `for_each_property` must read.
        let mut s = SqliteStore::in_memory().unwrap();
        for id in 0..5u64 {
            s.put_feature(id, &[("n".to_string(), Value::Uint(id))])
                .unwrap();
        }
        let mut seen = Vec::new();
        s.for_each_feature(&mut |id, attrs| {
            for_each_property(attrs, &mut |index, value| {
                seen.push((id, s.schema().name(index).unwrap().to_string(), value));
            });
            Ok(())
        })
        .unwrap();
        let expected: Vec<_> = (0..5u64)
            .map(|id| (id, "n".to_string(), Value::Uint(id)))
            .collect();
        assert_eq!(seen, expected);
    }

    #[test]
    #[should_panic(expected = "malformed")]
    fn an_unknown_tag_is_a_panic() {
        let mut schema = Schema::default();
        let mut encoded = encode_properties(&mut schema, &[("k".to_string(), Value::Int(1))]);
        encoded.extend_from_slice(&[0, 200, 200, 200]);
        decode_properties(&schema, &encoded);
    }

    #[test]
    fn a_feature_with_no_attributes_is_an_empty_row_not_a_missing_one() {
        // `None` (never recorded) and empty drive different branches.
        let mut s = SqliteStore::in_memory().unwrap();
        s.put_feature(4, &[]).unwrap();
        assert_eq!(s.feature(4).unwrap(), Some(Vec::new()));
        assert_eq!(s.feature(5).unwrap(), None);
    }

    #[test]
    fn the_binary_row_is_far_smaller_than_the_json_it_replaced() {
        // On floodplains the JSON table was 2.7 GB, about 63% repeated
        // structure.
        let attributes: Vec<(String, Value)> = vec![
            ("building_id".into(), Value::Int(1_731_105)),
            (
                "capture_method".into(),
                Value::Str("Feature Extraction".into()),
            ),
            (
                "capture_source_group".into(),
                Value::Str("NZ Aerial Imagery".into()),
            ),
            ("suburb_locality".into(), Value::Str("Ōtāhuhu".into())),
        ];
        let mut schema = Schema::default();
        let binary = encode_properties(&mut schema, &attributes).len();

        // What the old JSON would have cost: key, type tag and eight characters
        // of punctuation per attribute, plus the value.
        let json: usize = attributes
            .iter()
            .map(|(key, value)| {
                key.len()
                    + 8
                    + match value {
                        Value::Str(s) => s.len() + 7,
                        _ => 12,
                    }
            })
            .sum();
        assert!(
            binary * 2 < json,
            "expected the binary row to more than halve {json} bytes, got {binary}"
        );
    }
}
