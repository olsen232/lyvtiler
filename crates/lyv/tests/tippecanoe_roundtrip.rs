//! `tippecanoe-decode`, an independent implementation, must agree about what we
//! wrote. Skipped if it isn't installed.

use std::path::PathBuf;
use std::process::Command;

/// A line down New Zealand with a distinctive kink, so a mirrored or shifted
/// result is obvious.
const SOURCE: &str = r#"{"type":"FeatureCollection","features":[
  {"type":"Feature","id":42,"properties":{},"geometry":{"type":"LineString","coordinates":
   [[172.6,-34.4],[174.8,-36.9],[176.0,-41.0],[174.6,-41.3],[171.2,-42.5],[167.0,-45.0]]}}
]}"#;

fn find_tippecanoe_decode() -> Option<PathBuf> {
    [
        PathBuf::from("tippecanoe-decode"),
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Library/Python/3.12/lib/python/site-packages/tippecanoe/bin/tippecanoe-decode"),
    ]
    .into_iter()
    .find(|path| {
        Command::new(path)
            .arg("--help")
            .output()
            .is_ok_and(|out| out.status.success() || !out.stderr.is_empty())
    })
}

/// Every coordinate tippecanoe reads back, walking the nesting: indexing
/// `coordinates[i][0]` once silently counted nothing for MultiLineStrings.
fn decoded_coordinates(json: &serde_json::Value) -> Vec<(f64, f64)> {
    /// Descend until reaching `[number, number]`.
    fn coordinates(node: &serde_json::Value, out: &mut Vec<(f64, f64)>) {
        let Some(items) = node.as_array() else { return };
        match (
            items.first().and_then(|v| v.as_f64()),
            items.get(1).and_then(|v| v.as_f64()),
        ) {
            (Some(lon), Some(lat)) => out.push((lon, lat)),
            _ => {
                for item in items {
                    coordinates(item, out);
                }
            }
        }
    }

    fn walk(node: &serde_json::Value, out: &mut Vec<(f64, f64)>) {
        if let Some(geometry) = node.get("geometry") {
            coordinates(&geometry["coordinates"], out);
        }
        if let Some(features) = node.get("features").and_then(|f| f.as_array()) {
            for child in features {
                walk(child, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(json, &mut out);
    out
}

#[test]
fn tippecanoe_reads_back_the_tileset_we_built() {
    let Some(decode) = find_tippecanoe_decode() else {
        eprintln!("skipping: tippecanoe-decode not installed");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("spine.geojson");
    std::fs::write(&input, SOURCE).unwrap();
    let output = dir.path().join("spine.mbtiles");

    let run = Command::new(env!("CARGO_BIN_EXE_lyv"))
        .arg(&input)
        .arg(&output)
        .args(["-z", "6", "--min-zoom", "0", "-l", "spine"])
        .output()
        .expect("lyv should run");
    assert!(
        run.status.success(),
        "lyv failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let decoded = Command::new(&decode)
        .arg(&output)
        .output()
        .expect("tippecanoe-decode should run");

    // It reports malformed geometry on stderr while exiting zero, so stderr
    // must be empty.
    assert_eq!(
        String::from_utf8_lossy(&decoded.stderr),
        "",
        "tippecanoe-decode complained about our tileset"
    );

    let json: serde_json::Value =
        serde_json::from_slice(&decoded.stdout).expect("decoded output should be JSON");
    let coordinates = decoded_coordinates(&json);
    assert!(
        coordinates.len() > 20,
        "expected coordinates across many tiles, got {}",
        coordinates.len()
    );

    // Everything near New Zealand. Loose bounds because margins extend past
    // each tile. Longitude modulo 360: at zoom 0 the root tile's western margin
    // holds a copy of the line near -187, the same ground as 173.
    for (lon, lat) in &coordinates {
        let wrapped = lon.rem_euclid(360.0);
        assert!(
            (150.0..=195.0).contains(&wrapped) && (-55.0..=-25.0).contains(lat),
            "({lon}, {lat}) is nowhere near New Zealand, even allowing for the wrap"
        );
    }

    // The extremes must be the line's own, not shifted or mirrored.
    let north = coordinates
        .iter()
        .map(|(_, lat)| *lat)
        .fold(f64::MIN, f64::max);
    let south = coordinates
        .iter()
        .map(|(_, lat)| *lat)
        .fold(f64::MAX, f64::min);
    assert!((north - -34.4).abs() < 0.5, "northern end is at {north}");
    assert!((south - -45.0).abs() < 0.5, "southern end is at {south}");
}

#[test]
fn the_feature_id_survives_into_the_tileset() {
    // Ids must survive: the merge joins pieces by id.
    let Some(decode) = find_tippecanoe_decode() else {
        eprintln!("skipping: tippecanoe-decode not installed");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("spine.geojson");
    std::fs::write(&input, SOURCE).unwrap();
    let output = dir.path().join("spine.mbtiles");

    Command::new(env!("CARGO_BIN_EXE_lyv"))
        .arg(&input)
        .arg(&output)
        .args(["-z", "4", "-l", "spine"])
        .output()
        .unwrap();

    let decoded = Command::new(&decode).arg(&output).output().unwrap();
    let text = String::from_utf8_lossy(&decoded.stdout);
    assert!(
        text.contains("\"id\": 42"),
        "feature id 42 is missing from the decoded output"
    );
}
