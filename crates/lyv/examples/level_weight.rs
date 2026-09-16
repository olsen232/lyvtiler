//! What each zoom level weighs in vertices, from a finished archive: mean,
//! percentiles and counts over thresholds. Used to choose the vertex budget's
//! trigger.
//!
//! Takes a directory of `z_x_y.mvt` tiles, raw or gzipped:
//!
//! ```text
//! pmtiles tile archive.pmtiles $z $x $y > tiles/${z}_${x}_${y}.mvt
//! cargo run --release -p lyv --example level_weight -- tiles/
//! ```
//!
//! Tiles the archive doesn't hold are skipped, not counted as zero. The CLI
//! writes `Tile not found in archive.` to stdout for them, so they're
//! recognised by content.

use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result};

/// Thresholds to count tiles against.
const MEAN_LIMIT: usize = 200_000;
const TILE_LIMIT: usize = 400_000;

struct Tile {
    name: String,
    vertices: usize,
}

fn vertices_in(path: &std::path::Path) -> Result<usize> {
    let stored = std::fs::read(path)?;
    let raw = if stored.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&stored[..]).read_to_end(&mut out)?;
        out
    } else {
        stored
    };
    if raw.is_empty() || raw.starts_with(b"Tile not found") {
        return Ok(0);
    }
    let chains =
        lyv_mvt::decode_chains(&raw).with_context(|| format!("decoding {}", path.display()))?;
    Ok(chains.iter().map(|c| c.points.len()).sum())
}

fn percentile(sorted: &[usize], p: f64) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index]
}

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: level_weight <dir of z_x_y.mvt>");
        std::process::exit(2);
    });

    let mut levels: BTreeMap<u32, Vec<Tile>> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mvt") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let z: u32 = stem
            .split('_')
            .next()
            .and_then(|z| z.parse().ok())
            .with_context(|| format!("{stem} is not named z_x_y"))?;
        let vertices = vertices_in(&path)?;
        // Not a real tile.
        if vertices == 0 {
            continue;
        }
        levels.entry(z).or_default().push(Tile {
            name: stem.replace('_', "/"),
            vertices,
        });
    }

    println!(
        "{:>3}{:>8}{:>12}{:>10}{:>10}{:>10}{:>10}{:>8}{:>8}  worst tile",
        "z", "tiles", "total", "mean", "median", "p90", "max", ">200k", ">400k",
    );
    for (z, tiles) in &levels {
        let mut sizes: Vec<usize> = tiles.iter().map(|t| t.vertices).collect();
        sizes.sort_unstable();
        let total: usize = sizes.iter().sum();
        let worst = tiles.iter().max_by_key(|t| t.vertices).unwrap();
        println!(
            "{z:>3}{:>8}{:>12}{:>10}{:>10}{:>10}{:>10}{:>8}{:>8}  {}",
            tiles.len(),
            total,
            total / tiles.len(),
            percentile(&sizes, 0.5),
            percentile(&sizes, 0.9),
            sizes[sizes.len() - 1],
            sizes.iter().filter(|&&v| v > MEAN_LIMIT).count(),
            sizes.iter().filter(|&&v| v > TILE_LIMIT).count(),
            worst.name,
        );
    }
    Ok(())
}
