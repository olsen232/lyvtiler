//! Print a store's attribute table, one `id<TAB>key<TAB>type<TAB>value` line
//! per attribute. The rows are binary, so `sqlite3` can't read them.
//!
//! Also a direct way to compare attributes between two builds: dump a store
//! from each and diff.
//!
//! ```sh
//! lyv input.fgb out.pmtiles --store s.db --keep-store
//! cargo run --release -p lyv-store --example dump_attributes -- s.db
//! ```

use lyv_store::{SqliteStore, TileStore, Value};
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: dump_attributes <store.db>"))?;
    let store = SqliteStore::open(std::path::Path::new(&path))?;
    let schema = store.schema().clone();

    // Written as it scans: collecting first could hold a gigabyte of text.
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let mut failed = Ok(());
    store.for_each_feature(&mut |id, attrs| {
        lyv_store::for_each_property(attrs, &mut |index, value| {
            if failed.is_err() {
                return;
            }
            let name = schema.name(index).unwrap_or("<not in the schema>");
            let (kind, text) = match &value {
                Value::Str(s) => ("str", s.clone()),
                // `{:?}`: the shortest text that reads back as the same number.
                Value::Float(f) => ("float", format!("{f:?}")),
                Value::Double(d) => ("double", format!("{d:?}")),
                Value::Int(i) => ("int", i.to_string()),
                Value::Uint(u) => ("uint", u.to_string()),
                Value::Sint(i) => ("sint", i.to_string()),
                Value::Bool(b) => ("bool", b.to_string()),
            };
            failed = writeln!(out, "{id}\t{name}\t{kind}\t{text}");
        });
        Ok(())
    })?;
    failed?;
    out.flush()?;
    Ok(())
}
