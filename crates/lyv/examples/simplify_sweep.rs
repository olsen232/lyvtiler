//! How much does a heavier tolerance buy on a tile? Used to check that the
//! vertex budget could work by simplifying rather than dropping.
//!
//! ```text
//! pmtiles tile floodplains.pmtiles 4 4 6 > /tmp/t.mvt
//! cargo run --release -p lyv --example simplify_sweep -- /tmp/t.mvt
//! ```
//!
//! It re-simplifies a tile already built at `--simplify-distance 1.0`, which
//! isn't the same as building at a coarser tolerance (Douglas-Peucker isn't
//! nested), but it answers whether the geometry has slack at all, cheaply.

use std::io::{Read, Write};

use anyhow::{Context, Result};
use lyv_core::Chain;
use lyv_simplify::Tolerance;

/// Douglas-Peucker distances to try. 1.0 built the tile, so its row shows the
/// second-pass effect, not a real saving.
const DISTANCES: &[f64] = &[0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0];

/// Visvalingam areas, as a separate sweep.
const AREAS: &[f64] = &[1.0, 4.0, 16.0, 64.0, 256.0];

fn read_tile(path: &str) -> Result<Vec<u8>> {
    let stored = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    if stored.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&stored[..]).read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(stored)
    }
}

fn gzipped(bytes: &[u8]) -> usize {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap().len()
}

struct Row {
    label: String,
    chains: usize,
    vertices: usize,
    longest: usize,
    raw: usize,
    gz: usize,
}

fn measure(label: String, extent: u32, chains: &[Chain]) -> Row {
    // One layer name for every row, so byte columns compare with each other.
    let encoded = lyv_mvt::encode_chains("l", extent, chains);
    Row {
        label,
        chains: chains.len(),
        vertices: chains.iter().map(|c| c.points.len()).sum(),
        longest: chains.iter().map(|c| c.points.len()).max().unwrap_or(0),
        raw: encoded.len(),
        gz: gzipped(&encoded),
    }
}

fn main() -> Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: simplify_sweep <tile.mvt>");
        std::process::exit(2);
    });

    let bytes = read_tile(&path)?;
    let original = lyv_mvt::decode_chains(&bytes).context("decoding the tile")?;
    // For boundary protection, and so re-encoding matches the original.
    let extent = lyv_mvt::decode_extent(&bytes)?;
    let base = measure("as built".to_string(), extent, &original);

    let mut rows = vec![base];
    for &d in DISTANCES {
        rows.push(measure(
            format!("dp {d}"),
            extent,
            &lyv_simplify::simplify_chains(&original, extent, Tolerance::Distance(d)),
        ));
    }
    for &a in AREAS {
        rows.push(measure(
            format!("vw {a}"),
            extent,
            &lyv_simplify::simplify_chains(&original, extent, Tolerance::Area(a)),
        ));
    }

    let full = rows[0].vertices as f64;
    println!(
        "{:<12}{:>10}{:>12}{:>8}{:>10}{:>10}{:>9}",
        "tolerance", "chains", "vertices", "kept", "longest", "raw KB", "gz KB"
    );
    for row in &rows {
        println!(
            "{:<12}{:>10}{:>12}{:>7.0}%{:>10}{:>9}K{:>8}K",
            row.label,
            row.chains,
            row.vertices,
            100.0 * row.vertices as f64 / full,
            row.longest,
            row.raw / 1024,
            row.gz / 1024,
        );
    }

    // Where the vertices are: only chains long enough to have interior points
    // can be simplified; short ones need dropping.
    println!();
    let buckets = [2usize, 5, 10, 50, 200, 1000, usize::MAX];
    let mut previous = 0;
    for &limit in &buckets {
        let (count, vertices) = original
            .iter()
            .filter(|c| c.points.len() > previous && c.points.len() <= limit)
            .fold((0usize, 0usize), |(n, v), c| (n + 1, v + c.points.len()));
        println!(
            "chains of {:>4}..{:<6} {:>8} chains {:>10} vertices ({:>3.0}%)",
            previous + 1,
            if limit == usize::MAX {
                "more".to_string()
            } else {
                limit.to_string()
            },
            count,
            vertices,
            100.0 * vertices as f64 / full,
        );
        previous = limit;
    }
    Ok(())
}
