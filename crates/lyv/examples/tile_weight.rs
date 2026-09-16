//! What's in a tile: features, chains and vertices. Bytes don't say what a
//! renderer pays — a tile of attributes and a tile of geometry can be the same
//! size — so this counts vertices.
//!
//! Takes raw or gzipped MVT, several at once as a table:
//!
//! ```text
//! pmtiles tile out.pmtiles 4 4 6 > tile.mvt
//! cargo run --release -p lyv --example tile_weight -- tile.mvt
//! ```

use std::io::Read;

use anyhow::{Context, Result};
use lyv_core::Local;

struct Weight {
    name: String,
    stored: usize,
    raw: usize,
    features: usize,
    chains: usize,
    vertices: usize,
    longest: usize,
    margin: usize,
}

fn weigh(path: &str) -> Result<Weight> {
    let stored = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    // Gzipped or raw, by the magic number.
    let raw = if stored.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&stored[..]).read_to_end(&mut out)?;
        out
    } else {
        stored.clone()
    };

    let chains = lyv_mvt::decode_chains(&raw).context("decoding the tile")?;
    let extent = lyv_mvt::decode_extent(&raw)? as Local;
    let features: std::collections::HashSet<u64> = chains.iter().map(|c| c.feature).collect();
    Ok(Weight {
        name: path.to_string(),
        stored: stored.len(),
        raw: raw.len(),
        features: features.len(),
        chains: chains.len(),
        vertices: chains.iter().map(|c| c.points.len()).sum(),
        longest: chains.iter().map(|c| c.points.len()).max().unwrap_or(0),
        // Vertices outside the tile: the margin, which the vertex budget
        // doesn't see when it measures.
        margin: chains
            .iter()
            .flat_map(|c| c.points.iter())
            .filter(|p| p.x < 0 || p.y < 0 || p.x > extent || p.y > extent)
            .count(),
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: tile_weight <tile.mvt>...");
        std::process::exit(2);
    }

    println!(
        "{:<22}{:>10}{:>10}{:>10}{:>10}{:>12}{:>10}{:>9}{:>8}",
        "tile", "stored", "raw", "features", "chains", "vertices", "longest", "v/feat", "margin"
    );
    for path in &args {
        let w = weigh(path)?;
        let share = |part: usize| {
            if w.vertices == 0 {
                0.0
            } else {
                100.0 * part as f64 / w.vertices as f64
            }
        };
        let per = if w.features == 0 {
            0.0
        } else {
            w.vertices as f64 / w.features as f64
        };
        println!(
            "{:<22}{:>9}K{:>9}K{:>10}{:>10}{:>12}{:>10}{:>9.0}{:>7.1}%",
            w.name.rsplit('/').next().unwrap_or(&w.name),
            w.stored / 1024,
            w.raw / 1024,
            w.features,
            w.chains,
            w.vertices,
            w.longest,
            per,
            share(w.margin),
        );
    }
    Ok(())
}
