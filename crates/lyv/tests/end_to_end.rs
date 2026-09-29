//! The whole pipeline on lines: `mercator`, `cut`, `merge`, `mvt`, `store` and
//! `archive` agreeing about coordinates, addresses and bytes on real input.

use std::path::{Path, PathBuf};
use std::process::Command;

use lyv::{Options, run};
use lyv_core::Local;
use lyv_store::{SqliteStore, TileStore};

const EXTENT: u32 = 4096;

/// A diagonal across the Pacific, crossing many tiles at maxzoom, and a short
/// line inside one tile.
fn source() -> String {
    r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":1,"properties":{},"geometry":{"type":"LineString",
         "coordinates":[[166.0,-47.0],[178.5,-34.0]]}},
        {"type":"Feature","id":2,"properties":{},"geometry":{"type":"LineString",
         "coordinates":[[174.70,-41.30],[174.85,-41.25]]}},
        {"type":"Feature","id":3,"properties":{},"geometry":{"type":"MultiLineString",
         "coordinates":[[[170.0,-45.0],[171.0,-44.0]],[[172.0,-43.0],[173.0,-42.0]]]}}
    ]}"#
    .to_string()
}

struct Built {
    _dir: tempfile::TempDir,
    options: Options,
}

fn build(max_zoom: u32, min_zoom: u32) -> Built {
    build_with(max_zoom, min_zoom, 0)
}

fn build_with(max_zoom: u32, min_zoom: u32, buffer: u32) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.geojson");
    std::fs::write(&input, source()).unwrap();
    let output = dir.path().join("out.pmtiles");

    let options = Options {
        store: lyv::default_store_path(&output),
        input,
        output,
        min_zoom,
        max_zoom: Some(max_zoom),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "lines".to_string(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        extent: EXTENT,
        maxzoom_extent: None,
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        dust: true,
        threads: 1,
        // Kept, since these tests read the store.
        keep_store: true,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        // No limits: these tests check exact geometry.
        limits: lyv::Limits {
            bytes: 0,
            features: 0,
        },
        // No vertex budget, which would coarsen the geometry.
        max_tile_vertices: 0,
        buffer,
        simplification: lyv_simplify::Tolerance::NONE,
    };
    run(&options).expect("the pipeline should run");
    Built { _dir: dir, options }
}

fn store_of(built: &Built) -> SqliteStore {
    SqliteStore::open(&built.options.store).unwrap()
}

fn tiles_at(store: &SqliteStore, z: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    store
        .for_each_at(z, &mut |coord, _| {
            out.push((coord.x, coord.y));
            Ok(())
        })
        .unwrap();
    out
}

fn find_pmtiles_cli() -> Option<PathBuf> {
    let path = PathBuf::from("pmtiles");
    Command::new(&path)
        .arg("--help")
        .output()
        .ok()
        .map(|_| path)
}

#[test]
fn the_pipeline_produces_a_pyramid_covering_every_requested_zoom() {
    let built = build(8, 0);
    let store = store_of(&built);

    assert_eq!(
        store.zooms().unwrap(),
        (0..=8).collect::<Vec<_>>(),
        "every zoom from min_zoom to max_zoom should hold tiles"
    );
}

#[test]
fn each_level_has_at_most_a_quarter_of_the_tiles_below_it() {
    // Parents only exist with children, and up to four children share one, so
    // each level has between a quarter of and as many tiles as the one below.
    let built = build(7, 0);
    let store = store_of(&built);

    let counts: Vec<usize> = (0..=7).map(|z| tiles_at(&store, z).len()).collect();

    for (z, pair) in counts.windows(2).enumerate() {
        let (parents, children) = (pair[0], pair[1]);
        assert!(parents > 0, "zoom {z} is empty");
        assert!(
            parents <= children,
            "zoom {z} has {parents} tiles but zoom {} has only {children}",
            z + 1
        );
        assert!(
            parents * 4 >= children,
            "zoom {z} has {parents} tiles, too few to be the parents of {children}"
        );
    }
}

#[test]
fn every_tile_is_a_readable_vector_tile_holding_our_layer() {
    let built = build(6, 0);
    let store = store_of(&built);

    let mut checked = 0;
    for z in 0..=6 {
        store
            .for_each_at(z, &mut |coord, data| {
                let chains = lyv_mvt::decode_chains(&data).unwrap_or_else(|e| {
                    panic!(
                        "tile {}/{}/{} did not decode: {e}",
                        coord.z, coord.x, coord.y
                    )
                });
                assert!(
                    !chains.is_empty(),
                    "tile {}/{}/{} was stored but holds nothing",
                    coord.z,
                    coord.x,
                    coord.y
                );
                for chain in &chains {
                    assert!(
                        (1..=3).contains(&chain.feature),
                        "unexpected feature id {} in {}/{}/{}",
                        chain.feature,
                        coord.z,
                        coord.x,
                        coord.y
                    );
                    assert!(chain.points.len() >= 2, "a chain needs two points");
                }
                checked += 1;
                Ok(())
            })
            .unwrap();
    }
    assert!(checked > 10, "only checked {checked} tiles");
}

#[test]
fn geometry_stays_inside_its_tile() {
    // No buffer here, so nothing lies outside 0..extent.
    let built = build(6, 0);
    let store = store_of(&built);

    for z in 0..=6 {
        store
            .for_each_at(z, &mut |coord, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    for point in chain.points {
                        assert!(
                            (0..=EXTENT as Local).contains(&point.x)
                                && (0..=EXTENT as Local).contains(&point.y),
                            "{point:?} is outside tile {}/{}/{}",
                            coord.z,
                            coord.x,
                            coord.y
                        );
                    }
                }
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn a_feature_confined_to_one_tile_at_maxzoom_reaches_the_root() {
    // The short line must survive every merge to z0.
    let built = build(8, 0);
    let store = store_of(&built);

    let root = store
        .get(lyv_core::TileCoord { z: 0, x: 0, y: 0 })
        .unwrap()
        .expect("z0 must exist");
    let features: Vec<u64> = lyv_mvt::decode_chains(&root)
        .unwrap()
        .iter()
        .map(|c| c.feature)
        .collect();

    for id in [1, 2, 3] {
        assert!(features.contains(&id), "feature {id} is missing from z0");
    }
}

#[test]
fn a_zoom_range_narrower_than_the_pyramid_is_respected() {
    let built = build(6, 3);
    let store = store_of(&built);
    assert_eq!(store.zooms().unwrap(), vec![3, 4, 5, 6]);
}

#[test]
fn the_archive_holds_exactly_the_tiles_the_store_does() {
    let built = build(5, 0);
    let store = store_of(&built);

    let expected: usize = (0..=5).map(|z| tiles_at(&store, z).len()).sum();

    // Counted with the `pmtiles` CLI, when installed.
    let Some(cli) = find_pmtiles_cli() else {
        eprintln!("skipping archive count: pmtiles CLI not installed");
        return;
    };
    let shown = Command::new(&cli)
        .args(["show", &built.options.output.to_string_lossy()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&shown.stdout);
    let addressed: usize = text
        .lines()
        .find_map(|line| line.strip_prefix("addressed tiles count: "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("could not read the tile count from:\n{text}"));

    assert_eq!(addressed, expected, "archive and store disagree");
}

#[test]
fn a_built_tileset_records_which_lyv_made_it() {
    // The version says which lyv produced the tiles.
    let built = build(3, 0);

    let bytes = std::fs::read(&built.options.output).unwrap();
    let at =
        |offset: usize| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize;
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

    assert_eq!(
        json["generator"],
        serde_json::Value::from(format!("lyv {}", env!("CARGO_PKG_VERSION"))),
        "should stamp the crate's own version"
    );
    // The literal form, so `lyv/0.1.0` or a bare version would fail.
    let generator = json["generator"].as_str().unwrap();
    assert!(
        generator.starts_with("lyv ") && generator[4..].starts_with(char::is_numeric),
        "should read as `lyv <version>`, got: {generator}"
    );
}

#[test]
fn the_archive_passes_the_reference_verifier() {
    let built = build(5, 0);
    let Some(cli) = find_pmtiles_cli() else {
        eprintln!("skipping: pmtiles CLI not installed");
        return;
    };
    let output = Command::new(&cli)
        .args(["verify", &built.options.output.to_string_lossy()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "pmtiles verify rejected the archive: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_independent_reader_can_read_a_tile_out_of_our_archive() {
    // An independent reader: the `pmtiles` CLI finds the tile; we only decode
    // it.
    let built = build(5, 0);
    let Some(cli) = find_pmtiles_cli() else {
        eprintln!("skipping: pmtiles CLI not installed");
        return;
    };
    let raw = Command::new(&cli)
        .args([
            "tile",
            &built.options.output.to_string_lossy(),
            "0",
            "0",
            "0",
        ])
        .output()
        .unwrap();
    assert!(raw.status.success(), "could not fetch 0/0/0");

    let mut bytes = raw.stdout;
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&bytes[..]), &mut out)
            .unwrap();
        bytes = out;
    }
    let chains = lyv_mvt::decode_chains(&bytes).expect("0/0/0 should decode");
    assert!(!chains.is_empty(), "0/0/0 came back empty");
}

#[test]
fn a_kept_store_is_emptied_before_the_next_run() {
    // Every stored tile gets encoded, so the first input's tiles would leak
    // into the second's archive.
    let built = build(4, 0);
    let one_line = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":2,"properties":{},"geometry":{"type":"LineString",
         "coordinates":[[174.70,-41.30],[174.85,-41.25]]}}]}"#;
    std::fs::write(&built.options.input, one_line).unwrap();

    let again = lyv::Collected::from(&built.options).unwrap();
    assert_eq!(again.tiles.len(), 5, "one short line is one tile a zoom");
}

#[test]
fn a_missing_input_is_reported_before_anything_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pmtiles");
    let options = Options {
        input: dir.path().join("absent.geojson"),
        store: lyv::default_store_path(&output),
        output: output.clone(),
        min_zoom: 0,
        max_zoom: Some(4),
        min_auto_zoom: 12,
        max_auto_zoom: 16,
        output_layer: "lines".into(),
        output_name: None,
        attributes: lyv::Attributes::All,
        geometry_field: None,
        extent: EXTENT,
        maxzoom_extent: None,
        ranker: "evenstar".to_string(),
        rank_depth: 4,
        dust: true,
        threads: 1,
        // Kept, since these tests read the store.
        keep_store: true,
        mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
        // No limits: these tests check exact geometry.
        limits: lyv::Limits {
            bytes: 0,
            features: 0,
        },
        // No vertex budget, which would coarsen the geometry.
        max_tile_vertices: 0,
        buffer: 64,
        simplification: lyv_simplify::Tolerance::NONE,
    };

    assert!(run(&options).is_err());
    assert!(
        !Path::new(&output).exists(),
        "no archive should be left behind"
    );
}

// ---------------------------------------------------------------------------
// Buffering
// ---------------------------------------------------------------------------

/// Every coordinate in a stage, across every zoom.
fn all_points(store: &SqliteStore) -> Vec<(u32, Local, Local)> {
    let mut out = Vec::new();
    for z in store.zooms().unwrap() {
        store
            .for_each_at(z, &mut |coord, data| {
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    for point in chain.points {
                        out.push((coord.z, point.x, point.y));
                    }
                }
                Ok(())
            })
            .unwrap();
    }
    out
}

/// Every point of every finished tile, by re-running the pipeline (finished
/// tiles go straight to the archive).
fn all_emitted_points(options: &Options) -> Vec<(u32, Local, Local)> {
    let tiles = lyv::Collected::from(options).unwrap();
    let mut out = Vec::new();
    for (coord, data) in &tiles.tiles {
        for chain in lyv_mvt::decode_chains(data).unwrap() {
            for point in chain.points {
                out.push((coord.z, point.x, point.y));
            }
        }
    }
    out
}

#[test]
fn without_a_buffer_no_geometry_leaves_its_tile() {
    let built = build_with(6, 0, 0);
    let outside = all_emitted_points(&built.options)
        .into_iter()
        .filter(|(_, x, y)| {
            !(0..=EXTENT as Local).contains(x) || !(0..=EXTENT as Local).contains(y)
        })
        .count();
    assert_eq!(outside, 0, "buffer 0 must leave tiles exactly clipped");
}

#[test]
fn a_buffer_does_nothing_to_a_lines_only_tileset() {
    // Lines aren't buffered, so for this all-lines fixture, buffer 64 and 0
    // give the same pyramid. Polygons are covered in `polygons.rs`.
    let buffered = build_with(6, 0, 64);
    let plain = build_with(6, 0, 0);

    assert_eq!(
        all_emitted_points(&buffered.options),
        all_emitted_points(&plain.options),
        "buffering changed a tileset that holds no polygons"
    );
}

#[test]
fn buffering_leaves_the_internal_pyramid_untouched() {
    // Internal tiles stay exactly clipped whatever the buffer.
    let built = build_with(6, 0, 64);
    let store = store_of(&built);

    let outside = all_points(&store)
        .into_iter()
        .filter(|(_, x, y)| {
            !(0..=EXTENT as Local).contains(x) || !(0..=EXTENT as Local).contains(y)
        })
        .count();
    assert_eq!(outside, 0, "an internal tile grew a margin");
}

#[test]
fn no_line_geometry_ever_leaves_its_tile() {
    // So with lines, nothing lies outside `0..extent` whatever the buffer.
    for buffer in [0, 5] {
        let built = build_with(6, 0, buffer);
        let outside = all_emitted_points(&built.options)
            .into_iter()
            .filter(|(_, x, y)| {
                !(0..=EXTENT as Local).contains(x) || !(0..=EXTENT as Local).contains(y)
            })
            .count();
        assert_eq!(
            outside, 0,
            "buffer {buffer} pushed line geometry out of its tile"
        );
    }
}

#[test]
fn a_buffer_adds_and_loses_no_tiles() {
    // Encoding must not lose or invent a tile.
    let built = build_with(5, 0, 64);
    let store = store_of(&built);
    let emitted = lyv::Collected::from(&built.options).unwrap();

    for z in 0..=5 {
        // Sorted: this asks which tiles exist, not their order.
        let mut written: Vec<(u32, u32)> = emitted.at(z).map(|(c, _)| (c.x, c.y)).collect();
        written.sort_unstable();
        let mut held = tiles_at(&store, z);
        held.sort_unstable();
        assert_eq!(
            held, written,
            "zoom {z} differs between the pyramid and what was written"
        );
    }
}

/// The PMTiles v3 `clustered` flag.
fn header_clustered(path: &std::path::Path) -> bool {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..7], b"PMTiles");
    assert_eq!(bytes[7], 3);
    bytes[96] == 1
}

#[test]
fn the_archive_comes_out_clustered() {
    // Clustered means nearby tiles are nearby in the file, which is what makes
    // serving over range requests efficient. It needs levels in Hilbert order
    // and zooms ascending; losing it is silent, just slow.
    let built = build(6, 0);
    assert!(
        header_clustered(&built.options.output),
        "the archive was written unclustered"
    );
}
