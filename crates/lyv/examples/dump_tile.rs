//! What's inside one encoded tile, for "this feature looks wrong — what's in
//! the bytes?"
//!
//! Usage:
//!   cargo run --release -p lyv --example dump_tile -- TILE.mvt [FEATURE_ID]
//!
//! Takes a decompressed tile. To get one from an archive:
//!   pmtiles tile out.pmtiles 7 33 51 | gunzip > t.mvt

use anyhow::Result;
use lyv_core::Chain;

fn main() -> Result<()> {
    let path = std::env::args().nth(1).expect("a .mvt file");
    let wanted: Option<u64> = std::env::args().nth(2).map(|a| a.parse().unwrap());
    let bytes = std::fs::read(&path)?;

    let chains = lyv_mvt::decode_chains(&bytes)?;
    let extent = lyv_mvt::decode_extent(&bytes)? as lyv_core::Local;
    let kinds = lyv_mvt::decode_geometry_kinds(&bytes)?;
    let attributes = lyv_mvt::decode_attributes(&bytes)?;

    let features: std::collections::BTreeSet<u64> = chains.iter().map(|c| c.feature).collect();
    println!(
        "{path}: {} bytes, {} features, {} chains",
        bytes.len(),
        features.len(),
        chains.len()
    );

    let Some(wanted) = wanted else {
        // Whole-tile rings: what a wrongly closed ring looks like.
        let mut covering: Vec<(u64, usize)> = Vec::new();
        for chain in &chains {
            let points = &chain.points;
            let span_x = points.iter().map(|p| p.x).max().unwrap_or(0)
                - points.iter().map(|p| p.x).min().unwrap_or(0);
            let span_y = points.iter().map(|p| p.y).max().unwrap_or(0)
                - points.iter().map(|p| p.y).min().unwrap_or(0);
            if span_x >= extent && span_y >= extent {
                covering.push((chain.feature, points.len()));
            }
        }
        println!(
            "\n{} chain(s) span the whole tile, over {} feature(s)",
            covering.len(),
            covering
                .iter()
                .map(|(id, _)| *id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        );
        for (id, points) in covering.iter().take(20) {
            println!("  feature {id}: {points} points");
        }
        println!("\npass a feature id to dump one");
        return Ok(());
    };

    let mine: Vec<&Chain> = chains.iter().filter(|c| c.feature == wanted).collect();
    if mine.is_empty() {
        println!("\nfeature {wanted} is not in this tile");
        return Ok(());
    }

    if let Some((_, kind)) = kinds.iter().find(|(id, _)| *id == wanted) {
        println!("\nfeature {wanted}: {kind:?}");
    }
    if let Some((_, properties)) = attributes.iter().find(|(id, _)| *id == wanted) {
        println!("attributes:");
        for (key, value) in properties {
            println!("  {key} = {value:?}");
        }
    }

    println!("\n{} ring(s)/chain(s):", mine.len());
    for (index, chain) in mine.iter().enumerate() {
        let points = &chain.points;
        let closed = points.first() == points.last() && points.len() > 2;
        // Doubled signed area: positive an exterior, negative a hole, zero
        // nothing.
        let mut double_area: i64 = 0;
        for pair in points.windows(2) {
            double_area += i64::from(pair[0].x) * i64::from(pair[1].y)
                - i64::from(pair[1].x) * i64::from(pair[0].y);
        }
        let (min_x, max_x) = (
            points.iter().map(|p| p.x).min().unwrap_or(0),
            points.iter().map(|p| p.x).max().unwrap_or(0),
        );
        let (min_y, max_y) = (
            points.iter().map(|p| p.y).min().unwrap_or(0),
            points.iter().map(|p| p.y).max().unwrap_or(0),
        );
        println!(
            "  [{index}] {} points, {}, double area {double_area} ({}), \
             x {min_x}..{max_x}, y {min_y}..{max_y}",
            points.len(),
            if closed { "closed" } else { "OPEN" },
            if double_area > 0 {
                "exterior"
            } else if double_area < 0 {
                "hole"
            } else {
                "ENCLOSES NOTHING"
            },
        );
        if points.len() <= 24 {
            let listed: Vec<String> = points.iter().map(|p| format!("{},{}", p.x, p.y)).collect();
            println!("       {}", listed.join(" "));
        }
    }
    Ok(())
}
