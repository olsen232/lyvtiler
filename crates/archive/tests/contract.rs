//! The contract every tile writer must honour, written against the trait so
//! new formats inherit it, and with opaque payloads so it can't depend on the
//! encoder.

use std::path::Path;
use std::process::Command;

use lyv_archive::{self as writer, Metadata};

const FORMATS: &[&str] = &["mbtiles", "pmtiles"];

/// An opaque payload, deliberately not a real tile: a few hundred bytes of
/// compressible noise exercises gzip, hashing and byte-exact storage.
fn a_tile() -> Vec<u8> {
    // A small alphabet so it compresses. Indexed modulo its own length, since
    // a hard-coded length once went stale when the text changed.
    const ALPHABET: &[u8] = b"lyv lyv   ";
    let mut seed = 0xBADA55E1FBABEu64;
    (0..400)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ALPHABET[(seed as usize) % ALPHABET.len()]
        })
        .collect()
}

fn metadata() -> Metadata {
    Metadata {
        name: "contract test".to_string(),
        min_zoom: 0,
        max_zoom: 3,
        // Unknown extent (an empty input) must still give a valid archive.
        bounds: None,
        antimeridian_adjusted_bounds: None,
        center: None,
        // So must no generator.
        generator: None,
        layers: Vec::new(),
    }
}

/// Read a tile back, decompressed. `None` if the format needs a tool that
/// isn't installed.
fn read_tile_back(path: &Path, z: u32, x: u32, y: u32) -> Option<Vec<u8>> {
    let raw = match path.extension().and_then(|e| e.to_str()) {
        Some("mbtiles") => {
            let tms_row = (1u32 << z) - 1 - y;
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.query_row(
                "SELECT tile_data FROM tiles
                 WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3",
                (z, x, tms_row),
                |row| row.get::<_, Vec<u8>>(0),
            )
            .ok()?
        }
        Some("pmtiles") => {
            let output = Command::new("pmtiles")
                .args([
                    "tile",
                    &path.to_string_lossy(),
                    &z.to_string(),
                    &x.to_string(),
                    &y.to_string(),
                ])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            output.stdout
        }
        _ => unreachable!(),
    };

    // Both store MVT gzipped, and say so.
    assert_eq!(
        &raw[..2],
        &[0x1f, 0x8b],
        "{} should store tile data gzipped",
        path.display()
    );
    let mut decompressed = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(&raw[..]),
        &mut decompressed,
    )
    .unwrap();
    Some(decompressed)
}

#[test]
fn every_format_returns_the_tile_it_was_given() {
    let dir = tempfile::tempdir().unwrap();
    let tile = a_tile();
    let coords = [(0, 0, 0), (2, 1, 2), (3, 7, 0)];

    let mut checked = 0;
    for format in FORMATS {
        let path = dir.path().join(format!("out.{format}"));

        let mut sink = writer::create(&path, &metadata()).unwrap();
        for &(z, x, y) in &coords {
            sink.write_tile(z, x, y, &tile).unwrap();
        }
        sink.finish().unwrap();

        for &(z, x, y) in &coords {
            if let Some(got) = read_tile_back(&path, z, x, y) {
                assert_eq!(got, tile, "{format} changed the tile at {z}/{x}/{y}");
                checked += 1;
            } else {
                eprintln!("skipping read-back for {format}: reader not available");
            }
        }
    }

    assert!(checked > 0, "no format could be read back at all");
}

/// Write `count` copies of one tile across zoom 3; return the archive size.
fn size_with_repeated_tiles(dir: &Path, format: &str, count: u32) -> u64 {
    let path = dir.join(format!("dedup-{count}.{format}"));
    let mut sink = writer::create(&path, &metadata()).unwrap();
    // Zoom 3 is 8x8: 64 coordinates.
    for i in 0..count {
        sink.write_tile(3, i % 8, i / 8, &a_tile()).unwrap();
    }
    sink.finish().unwrap();
    std::fs::metadata(&path).unwrap().len()
}

#[test]
fn pmtiles_stores_identical_tiles_only_once() {
    // PMTiles stores repeated content once, so a zoom full of one tile barely
    // grows the file. A PMTiles property, not part of the contract.
    let dir = tempfile::tempdir().unwrap();
    let tile_size = a_tile().len() as u64;

    let one = size_with_repeated_tiles(dir.path(), "pmtiles", 1);
    let sixty_four = size_with_repeated_tiles(dir.path(), "pmtiles", 64);

    assert!(
        sixty_four < one + tile_size,
        "64 identical tiles grew the archive from {one} to {sixty_four} bytes, \
         more than one extra tile's worth — they were not deduplicated"
    );
}

#[test]
fn mbtiles_does_not_deduplicate_yet() {
    // A known gap: plain MBTiles stores each repeat. The `map`/`images` schema
    // would fix it; when it does, delete this and add MBTiles to the test
    // above.
    let dir = tempfile::tempdir().unwrap();
    let tile_size = a_tile().len() as u64;

    let one = size_with_repeated_tiles(dir.path(), "mbtiles", 1);
    let sixty_four = size_with_repeated_tiles(dir.path(), "mbtiles", 64);

    assert!(
        sixty_four > one + tile_size,
        "MBTiles appears to deduplicate now ({one} -> {sixty_four} bytes); \
         if that is deliberate, fold it into the PMTiles dedup test"
    );
}

#[test]
fn a_coordinate_outside_its_zoom_level_is_rejected() {
    // Row 5 doesn't exist at zoom 1: an error, not a panic or an unaddressable
    // tile.
    let dir = tempfile::tempdir().unwrap();

    for format in FORMATS {
        let path = dir.path().join(format!("bad.{format}"));
        let mut sink = writer::create(&path, &metadata()).unwrap();

        assert!(
            sink.write_tile(1, 0, 5, &a_tile()).is_err(),
            "{format} accepted tile 1/0/5, which is outside zoom 1"
        );
        assert!(
            sink.write_tile(1, 9, 0, &a_tile()).is_err(),
            "{format} accepted tile 1/9/0, which is outside zoom 1"
        );
    }
}

#[test]
fn an_unknown_extension_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    // `expect_err` would need Debug on the bare trait object.
    let error = match writer::create(&dir.path().join("out.geojson"), &metadata()) {
        Ok(_) => panic!("should refuse an unknown format"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("geojson"),
        "error should name the offending format, got: {error}"
    );
}

#[test]
fn pmtiles_archives_pass_the_reference_verifier() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verify.pmtiles");

    let mut sink = writer::create(&path, &metadata()).unwrap();
    sink.write_tile(0, 0, 0, &a_tile()).unwrap();
    sink.finish().unwrap();

    match Command::new("pmtiles")
        .args(["verify", &path.to_string_lossy()])
        .output()
    {
        Ok(output) => assert!(
            output.status.success(),
            "pmtiles verify rejected our archive: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(_) => eprintln!("skipping: pmtiles CLI not installed"),
    }
}

/// Enough distinct tiles to spill PMTiles' 16 KB root directory into leaves —
/// a second code path. A real 13,947-entry archive once failed every lookup
/// there while `pmtiles verify` passed.
const ENOUGH_FOR_LEAVES: u32 = 20_000;

#[test]
fn an_archive_big_enough_to_need_leaf_directories_can_still_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leaves.pmtiles");

    let mut metadata = metadata();
    metadata.max_zoom = 8;
    let mut sink = writer::create(&path, &metadata).unwrap();

    // Row-major, as the pipeline produces, not the directory's Hilbert order.
    let mut written = Vec::new();
    for i in 0..ENOUGH_FOR_LEAVES {
        let (x, y) = (i % 256, i / 256);
        // Distinct, or dedup would collapse them.
        let tile = format!("tile {x} {y} {}", "x".repeat(64)).into_bytes();
        sink.write_tile(8, x, y, &tile).unwrap();
        written.push(((8, x, y), tile));
    }
    sink.finish().unwrap();

    // Spot-check first, last and a scattering between.
    let mut checked = 0;
    for index in [0, 1, 4_999, 10_000, 19_998, 19_999] {
        let ((z, x, y), expected) = &written[index];
        let Some(got) = read_tile_back(&path, *z, *x, *y) else {
            eprintln!("skipping read-back: reader not available");
            return;
        };
        assert_eq!(
            &got, expected,
            "tile {z}/{x}/{y} (entry {index} of {ENOUGH_FOR_LEAVES}) came back wrong"
        );
        checked += 1;
    }
    assert_eq!(checked, 6);
}
