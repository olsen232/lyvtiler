//! The lyv command line.
//!
//! Flags are named for what they do rather than inherited from anything. In
//! particular there is no `-z`/`-Z` pair, where the case of a letter decides
//! whether you mean the top or the bottom of the pyramid.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser};
use lyv::Options;

#[derive(Parser)]
#[command(
    name = "lyv",
    // From `CARGO_PKG_VERSION`, the workspace's single version.
    version,
    about = env!("CARGO_PKG_DESCRIPTION"),
    long_about = None,
    // Replaced by the three flags below: clap's own gives `-h` and `--help`
    // different output, and its version flag's wording doesn't match.
    disable_help_flag = true,
    disable_version_flag = true,
)]
struct Cli {
    /// Source file. FlatGeobuf for `.fgb`, GeoParquet for `.parquet`,
    /// GeoJSON for anything else.
    ///
    /// Each feature's geometry is read as it is, whatever type the file
    /// declares, and a layer may mix points, lines and polygons. MVT doesn't
    /// support single features of mixed type, so a GeometryCollection becomes
    /// either a multipolygon, multiline or multipoint (highest-dimension gets
    /// priority).
    input: PathBuf,

    /// Tileset to write. `.pmtiles` or `.mbtiles`, chosen by extension.
    output: PathBuf,

    /// Layer name inside the tiles. Defaults to the output file's stem.
    #[arg(short = 'l', long, alias = "layer")]
    output_layer: Option<String>,

    /// Human-readable name for the tileset, in the archive's metadata.
    /// Defaults to the output file's stem.
    #[arg(short = 'n', long, alias = "name")]
    output_name: Option<String>,

    /// Carry only these attributes into the tiles. Comma separated list.
    ///
    /// Filtered during the read, so an attribute left out is never parsed,
    /// allocated or carried — on an attribute-heavy layer this is the single
    /// largest thing you can do about memory and tile size.
    ///
    /// A name here that **no** feature has is an error, because a typo would
    /// otherwise produce a tileset quietly missing a field. A name that only
    /// some features have is fine.
    #[arg(long, value_delimiter = ',', conflicts_with = "no_attributes")]
    attributes: Option<Vec<String>>,

    /// Carry every attribute except these. Comma separated list.
    ///
    /// The other way round from `--attributes`, for when the few to drop are
    /// easier to name than the many to keep. As there, a name that no feature
    /// has is an error, or a typo would keep the field it meant to drop.
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "SKIP_ATTRIBUTES",
        conflicts_with_all = ["attributes", "no_attributes"]
    )]
    skip_attributes: Option<Vec<String>>,

    /// Carry no attributes at all: geometry only.
    #[arg(long)]
    no_attributes: bool,

    /// The geometry column to read, for a GeoParquet file with more than one.
    ///
    /// Defaults to the file's primary geometry column. Other formats have one
    /// geometry per feature and refuse this.
    #[arg(long, value_name = "NAME")]
    geometry_field: Option<String>,

    /// What the input's coordinates are, whatever CRS it declares:
    /// `EPSG:4326` (or `OGC:CRS84`) for lon/lat, `EPSG:3857` for Web Mercator.
    ///
    /// Nothing is reprojected. For input whose coordinates already are one of
    /// those, but whose declared CRS is missing, wrong, or one lyv can't read.
    /// GDA2020 is within two metres of WGS 84, for instance, and can be read
    /// as `EPSG:4326` if that's close enough.
    #[arg(long, value_name = "CRS", value_parser = input_crs)]
    input_crs: Option<lyv_source::Crs>,

    /// Output one line per stage instead of rewriting a line continuously to
    /// show progress updates.
    ///
    /// Makes for much shorter log files, otherwise the log has at least
    /// one line for every half-second progress update.
    #[arg(short, long)]
    quiet: bool,

    /// Output nothing at all on stderr.
    #[arg(long, conflicts_with = "quiet")]
    silent: bool,

    /// How many threads to use. Zero chooses for you.
    ///
    /// Zero gives every stage a worker per core (except the encode pass, which
    /// is capped at four: it is bound by store reads rather than by CPU).
    ///
    /// As you increase the number of threads, you'll first reach the point where
    /// lyv is as efficient as possible, and then the point where lyv is as fast
    /// as possible but less efficient in terms of CPU resources used, and beyond
    /// that it will be neither fast nor efficient.
    #[arg(short = 'j', long, default_value_t = 0)]
    threads: usize,

    /// The shallowest zoom to build.
    #[arg(long, default_value_t = 0)]
    min_zoom: u32,

    /// The deepest zoom to build. Chosen from the data when not set.
    ///
    /// Setting too low will mean the maxzoom tiles won't contain all features.
    /// Setting too high bloats the output unnecessarily with extra tiles that
    /// don't contain any more detail.
    #[arg(short = 'z', long)]
    max_zoom: Option<u32>,

    /// The shallowest maxzoom that automatic selection is allowed to select.
    ///
    /// This default ensures maxzoom tiles are at least accurate to the nearest metre.
    /// Increasing this will ensure higher accuracy of maxzoom tiles, if one metre
    /// accuracy is not sufficient for your usecase - lyv assumes this would be enough
    /// for most usecases but has no way of knowing.
    #[arg(long, default_value_t = 12)]
    min_auto_zoom: u32,

    /// The deepest maxzoom that automatic selection is allowed to select.
    ///
    /// Increase this if features are still dropping in maxzoom tiles - this shouldn't
    /// generally be an issue in practice.
    #[arg(long, default_value_t = 16)]
    max_auto_zoom: u32,

    /// Tile grid resolution for all zooms except maxzoom. Must be a power of two.
    ///
    /// Raising this above 8192 will have no effect for certain renderers,
    /// which store 16 bit numbers but with a few bits reserved for flags.
    #[arg(long, default_value_t = 4096)]
    extent: u32,

    /// Tile grid resolution at maxzoom.
    /// Must be a power-of-two, must be at least `--extent`.
    ///
    /// Raising this above 8192 will have no effect for certain renderers,
    /// which store 16 bit numbers but with a few bits reserved for flags.
    ///
    /// Defaults to 8192: setting the maxzoom extent as high as possible is
    /// recommended since this extra resolution will be used when the viewer
    /// overzooms the maxzoom tiles.
    #[arg(long, default_value_t = 8192)]
    maxzoom_extent: u32,

    /// Drop features that shrink to nothing, instead of keeping the smallest
    /// mark the grid can hold.
    ///
    /// Zooming out shrinks a feature until its vertices all snap onto one
    /// point. Keeping a speck means areas of small features remain visible,
    /// dropping it makes an area of small features evaporate into an empty
    /// area as you zoom out.
    ///
    /// A polygon becomes the smallest square the grid can hold, a line the
    /// shortest segment. Points are always dust anyway so they can't shrink;
    /// they are unaffected by this flag.
    #[arg(long)]
    no_dust: bool,

    /// The algorithm that decides which features are dropped.
    ///
    /// `evenstar` takes one feature from every maxzoom tile, then one more
    /// from each, and so on — so no region is ever emptied, while features
    /// spanning many tiles still rise to the top. `by-area` is the plain
    /// global sort by size, kept for comparison.
    #[arg(long, default_value = "evenstar", value_parser = clap::builder::PossibleValuesParser::new(lyv_rank::NAMES))]
    ranker: String,

    /// How many levels below maxzoom to bucket features into when ranking
    /// them for dropping. Zero buckets at maxzoom itself.
    ///
    /// The evenstar ranker spreads features across the tiles it buckets
    /// into, and no more finely than that. The default of four breaks each
    /// maxzoom tile into 256 subtiles and tries to keep features from each.
    #[arg(long, default_value_t = 4)]
    rank_depth: u32,

    /// Margin of polygon overlap into neighbouring tiles, in 4096ths of a tile.
    ///
    /// This much polygon geometry is copied into each tile from neighbouring tiles
    /// so that polygons don't stop abruptly at the edges of each tile - otherwise
    /// drawing polygon outlines would also show tile boundaries everywhere.
    ///
    /// In 4096ths, so that at the default `--extent` one unit of buffer is one
    /// unit of tile grid. The margin is a fixed fraction of a tile, so it does
    /// not change when the extent does.
    #[arg(long, default_value_t = 64)]
    buffer: u32,

    /// Tile size limit, in estimated uncompressed bytes, before
    /// features are dropped to make it fit. Zero means no limit.
    ///
    /// This can be increased if tiles are looking too sparse,
    /// or it can be decreased if tiles are too heavy - taking up too much
    /// space on disk, or taking too long to fetch and render.
    #[arg(long, default_value_t = 4_000_000)]
    max_tile_bytes: usize,

    /// Tile size limit in vertices before feature geometry is simplified more
    /// aggressively. Zero means no limit.
    ///
    /// This limit can be decreased if tiles are too heavy - taking too long
    /// to load onto the GPU and render.
    #[arg(long, default_value_t = 200_000)]
    max_tile_vertices: usize,

    /// Tile size limit in features before features are dropped to make it fit.
    /// Zero means no limit.
    ///
    /// The default here is one quarter of max_tile_vertices - exceeding this
    /// ratio can prevent geometry simplification from performing well if there
    /// are many simple linestrings and polygons that cannot be simplified.
    #[arg(long, default_value_t = 50_000)]
    max_tile_features: usize,

    /// Simplify using Douglas-Peucker: discard
    /// vertices closer than D tile units to the chord through their
    /// neighbours.
    ///
    /// One unit is a whole tile's width divided by the extent, so the same
    /// number means proportionally less detail at every zoom.
    #[arg(long, default_value_t = 1.0, value_name = "D")]
    simplify_distance: f64,

    /// Simplify using Visvalingam: discard vertices
    /// whose triangle with their neighbours is smaller than A square tile
    /// units.
    ///
    /// The two criteria are not interchangeable since this is an area
    /// and the other is a distance.
    #[arg(long, conflicts_with = "simplify_distance", value_name = "A")]
    simplify_area: Option<f64>,

    /// Where to keep the intermediate pyramid.
    ///
    /// Deleted when the run succeeds, and kept when it fails. Emptied at the
    /// start of every run. Defaults to the output path with a suffix.
    #[arg(long)]
    store: Option<PathBuf>,

    /// Leave the store behind instead of deleting it on success.
    ///
    /// For inspecting what was built. It is usually larger than the archive
    /// itself, so it is removed by default.
    #[arg(long)]
    keep_store: bool,

    /// How much of the intermediate store to memory-map.
    ///
    /// Turning on mapped pages can quickly use a lot of memory, since each
    /// worker gets its own mapped pages. However, these pages are easily
    /// reclaimed when memory runs tight. However, repeatedly allocating and
    /// reclaiming pages when memory is tight can cause lyv to run slower, and
    /// even when memory is abundant this may or may not lead to a measurable
    /// speedup. Defaulted off since it may or may not be useful.
    #[arg(long, default_value_t = 0.0, value_name = "GB")]
    mmap: f64,

    /// Show memory use on every progress line.
    ///
    /// Bytes allocated and not yet freed — what lyv is holding, rather than
    /// what the operating system has given it. Needs a binary built with
    /// `--features memory-stats`, since counting them costs a little on every
    /// allocation and is not worth paying for when nobody is asking.
    #[arg(long)]
    memory: bool,

    /// Break each timing stage into even smaller sub-stages.
    ///
    /// Extra timing detail for helping debug what is taking up time.
    #[arg(long)]
    trace: bool,

    /// Print every flag, one line each.
    #[arg(short = 'h', long = "help", action = clap::ArgAction::HelpShort)]
    help: Option<bool>,

    /// Print every flag with the reasoning behind its default.
    #[arg(long = "help-full", action = clap::ArgAction::HelpLong)]
    help_full: Option<bool>,

    /// Print the version and exit.
    #[arg(short = 'V', long = "version", action = clap::ArgAction::Version)]
    version: Option<bool>,
}

/// Counted, so `--memory` can report live bytes rather than resident set. Only
/// with `memory-stats`: the counting costs a few percent.
#[cfg(all(feature = "memory-stats", not(feature = "mimalloc")))]
#[global_allocator]
static ALLOCATOR: lyv_alloc::Counting<std::alloc::System> = lyv_alloc::system();

/// mimalloc, the default: 15% faster and 41% fewer instructions on a 3.5M
/// feature build than macOS's allocator. `--no-default-features` gives the
/// system allocator back.
#[cfg(all(feature = "mimalloc", not(feature = "memory-stats")))]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Both features: mimalloc, counted — live bytes are only meaningful beside
/// the allocator that ships.
#[cfg(all(feature = "mimalloc", feature = "memory-stats"))]
#[global_allocator]
static ALLOCATOR: lyv_alloc::Counting<mimalloc::MiMalloc> = lyv_alloc::Counting(mimalloc::MiMalloc);

/// Write `elapsed live_bytes` to the file named by `LYV_ALLOC_TRACE`, to
/// plot against RSS sampled from outside. Only when the variable is set.
#[cfg(feature = "memory-stats")]
fn trace_allocations(started: std::time::Instant) -> Result<()> {
    let Ok(path) = std::env::var("LYV_ALLOC_TRACE") else {
        return Ok(());
    };
    let mut file = std::fs::File::create(&path)
        .with_context(|| format!("creating the LYV_ALLOC_TRACE file {path}"))?;
    std::thread::spawn(move || {
        use std::io::Write;
        loop {
            let line = format!(
                "{:.3} {}",
                started.elapsed().as_secs_f64(),
                lyv_alloc::live()
            );
            if let Err(error) = writeln!(file, "{line}") {
                eprintln!("lyv: stopped writing {path}: {error}");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    Ok(())
}

#[cfg(not(feature = "memory-stats"))]
fn trace_allocations(_started: std::time::Instant) -> Result<()> {
    Ok(())
}

/// Memory for the closing line, only in a `memory-stats` build.
#[cfg(feature = "memory-stats")]
fn peak_live() -> String {
    format!(", {} peak live", lyv_alloc::mb(lyv_alloc::peak()))
}

#[cfg(not(feature = "memory-stats"))]
fn peak_live() -> String {
    String::new()
}

/// The banner: `lyv.ans` in 256-colour escapes, or `lyv.txt` in plain block
/// characters. The one place output depends on the terminal, since escapes in
/// a log file are 40 KB of noise.
fn banner() -> &'static str {
    // `NO_COLOR`, set to anything, by convention.
    if std::env::var_os("NO_COLOR").is_some() {
        return include_str!("lyv.txt");
    }
    if !std::io::stderr().is_terminal() {
        return include_str!("lyv.txt");
    }
    // `TERM=dumb` wins, checked before `COLORTERM`, which is often left set by
    // whatever launched the shell.
    let term = std::env::var("TERM").unwrap_or_default();
    if term.is_empty() || term == "dumb" {
        return include_str!("lyv.txt");
    }
    // `COLORTERM`, or a `TERM` naming 256 or direct colour. Otherwise plain: a
    // missing star beats a screen of escapes.
    let colourful = std::env::var_os("COLORTERM").is_some()
        || term.contains("256color")
        || term.contains("direct");
    if colourful {
        include_str!("lyv.ans")
    } else {
        include_str!("lyv.txt")
    }
}

/// What to say when someone types just `lyv`: how extensions choose formats,
/// and the version. Other parse errors keep clap's specific wording.
fn usage() -> String {
    format!(
        "\
{banner}
{name} {version} — {about}

Usage: lyv [OPTIONS] <INPUT> <OUTPUT>

  <INPUT>   .fgb (FlatGeobuf), .parquet (GeoParquet), or .geojson
  <OUTPUT>  .pmtiles or .mbtiles

Both formats are chosen by extension. `lyv --help` lists every flag, and
`lyv --help-full` gives the reasoning behind each default.",
        banner = banner(),
        name = env!("CARGO_PKG_NAME"),
        version = env!("CARGO_PKG_VERSION"),
        about = env!("CARGO_PKG_DESCRIPTION"),
    )
}

/// The default layer name: the output file's stem, matching the tileset's own
/// name. `lyv a.fgb b.pmtiles` gives a layer `b`.
fn default_layer_name(output: &std::path::Path) -> Result<String> {
    output
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .context("could not derive a layer name; pass --output-layer")
}

/// `--input-crs`, parsed as the readers parse a declared CRS.
fn input_crs(name: &str) -> Result<lyv_source::Crs, String> {
    lyv_source::parse_crs(name).map_err(|error| error.to_string())
}

/// Wire the allocator's live-byte counter into the progress line.
#[cfg(feature = "memory-stats")]
fn show_memory(_progress: &lyv::Progress) {
    lyv::show_memory(lyv_alloc::live);
}

/// Without the counting allocator there's nothing to read, so say so rather
/// than silently omit the column.
#[cfg(not(feature = "memory-stats"))]
fn show_memory(progress: &lyv::Progress) {
    progress.say("--memory needs a binary built with --features memory-stats");
}

fn main() -> Result<()> {
    let started = std::time::Instant::now();
    trace_allocations(started)?;
    // Via `ArgMatches`, to tell a typed value from a default: one default below
    // follows another flag unless it was given.
    let matches = match Cli::command().try_get_matches() {
        Ok(matches) => matches,
        Err(error) if error.kind() == clap::error::ErrorKind::MissingRequiredArgument => {
            eprintln!("{}", usage());
            // Clap's exit code for a usage error.
            std::process::exit(2);
        }
        Err(error) => error.exit(),
    };
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());

    let output_layer = match cli.output_layer {
        Some(name) => name,
        None => default_layer_name(&cli.output)?,
    };

    // Built first, so the notice below respects `--silent`.
    let progress = lyv::Progress::to_stderr(
        match (cli.silent, cli.quiet) {
            (true, _) => lyv::Verbosity::Silent,
            (_, true) => lyv::Verbosity::Quiet,
            _ => lyv::Verbosity::Normal,
        },
        cli.trace,
    );

    // Maxzoom is the finest level, so its extent can't be below `--extent`.
    // Raised, and said, rather than refused.
    let maxzoom_extent = cli.maxzoom_extent.max(cli.extent);
    if maxzoom_extent != cli.maxzoom_extent {
        progress.say(&format!("Maxzoom-extent changed to {maxzoom_extent}"));
    }

    // Refused, not clamped: a negative mapping is a typo.
    if cli.mmap.is_nan() || cli.mmap < 0.0 {
        anyhow::bail!("--mmap {} is not a size in GB", cli.mmap);
    }

    // The feature limit follows the vertex limit (a quarter of it) unless
    // given. Below about a quarter, tiles are mostly features too short to
    // simplify, so a higher feature limit never binds. It follows both ways,
    // but not when given explicitly (zero included), and not when there's no
    // vertex limit to follow.
    let mut max_tile_features = cli.max_tile_features;
    let asked_for =
        matches.value_source("max_tile_features") == Some(clap::parser::ValueSource::CommandLine);
    if !asked_for && cli.max_tile_vertices > 0 {
        // Never zero, which would mean no limit.
        let quarter = (cli.max_tile_vertices / 4).max(1);
        if quarter != max_tile_features {
            max_tile_features = quarter;
            progress.say(&format!("Max-tile-features changed to {max_tile_features}"));
        }
    }

    let options = Options {
        store: cli
            .store
            .unwrap_or_else(|| lyv::default_store_path(&cli.output)),
        input: cli.input,
        output: cli.output,
        min_zoom: cli.min_zoom,
        max_zoom: cli.max_zoom,
        min_auto_zoom: cli.min_auto_zoom,
        max_auto_zoom: cli.max_auto_zoom,
        output_layer,
        output_name: cli.output_name,
        // `--no-attributes` is `--attributes` with nothing in it. Clap refuses
        // any two of the three together.
        attributes: match (cli.no_attributes, cli.attributes, cli.skip_attributes) {
            (true, _, _) => lyv::Attributes::Only(Vec::new()),
            (_, Some(names), _) => lyv::Attributes::Only(names),
            (_, _, Some(names)) => lyv::Attributes::Except(names),
            _ => lyv::Attributes::All,
        },
        geometry_field: cli.geometry_field,
        input_crs: cli.input_crs,
        extent: cli.extent,
        maxzoom_extent: Some(maxzoom_extent),
        buffer: cli.buffer,
        ranker: cli.ranker,
        rank_depth: cli.rank_depth,
        dust: !cli.no_dust,
        keep_store: cli.keep_store,
        // Decimal GB. Non-negative, checked above.
        mmap_bytes: (cli.mmap * 1e9) as u64,
        threads: cli.threads,
        limits: lyv::Limits {
            bytes: cli.max_tile_bytes,
            features: max_tile_features,
        },
        max_tile_vertices: cli.max_tile_vertices,
        simplification: match cli.simplify_area {
            Some(area) => lyv_simplify::Tolerance::Area(area),
            None => lyv_simplify::Tolerance::Distance(cli.simplify_distance),
        },
    };

    if cli.memory {
        show_memory(&progress);
    }
    // Printed once at the end, so `--quiet` doesn't suppress it.
    if cli.trace {
        lyv::timings::enable();
    }
    lyv::run_with(&options, &progress)?;
    // Closed first, so the last stage is recorded and nothing draws over what
    // follows.
    progress.done();
    if cli.trace {
        progress.say(&progress.accounting());
        for line in lyv::timings::report() {
            progress.say(&line);
        }
    }
    println!(
        "wrote {} in {}{}",
        options.output.display(),
        elapsed(started.elapsed()),
        peak_live()
    );
    Ok(())
}

/// A duration in the largest useful unit: tenths of a second for a short run,
/// hours and minutes for a long one.
fn elapsed(taken: std::time::Duration) -> String {
    let seconds = taken.as_secs_f64();
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let whole = taken.as_secs();
    let (hours, minutes, seconds) = (whole / 3600, (whole % 3600) / 60, whole % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else {
        format!("{minutes}m {seconds:02}s")
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, default_layer_name, elapsed};
    use clap::Parser;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn attributes_can_be_listed_or_skipped_but_not_both() {
        let parse = |flags: &[&str]| {
            let mut args = vec!["lyv", "in.fgb", "out.pmtiles"];
            args.extend_from_slice(flags);
            Cli::try_parse_from(args)
        };
        assert!(parse(&["--attributes", "a"]).is_ok());
        assert!(parse(&["--skip-attributes", "a,b"]).is_ok());
        for both in [
            &["--attributes", "a", "--skip-attributes", "b"][..],
            &["--skip-attributes", "b", "--no-attributes"][..],
            &["--attributes", "a", "--no-attributes"][..],
        ] {
            assert!(parse(both).is_err(), "{both:?} should be refused");
        }
    }

    #[test]
    fn the_layer_is_named_after_the_output() {
        // The layer is named after the output file.
        assert_eq!(default_layer_name(Path::new("b.pmtiles")).unwrap(), "b");
        assert_eq!(default_layer_name(Path::new("b.mbtiles")).unwrap(), "b");
        // The final component, up to the last dot.
        assert_eq!(
            default_layer_name(Path::new("/tmp/out/hydro.v2.pmtiles")).unwrap(),
            "hydro.v2"
        );
        // No stem: the one case that needs the flag.
        assert!(default_layer_name(Path::new("..")).is_err());
    }

    #[test]
    fn a_duration_is_shown_in_the_largest_unit_that_says_something() {
        assert_eq!(elapsed(Duration::from_millis(0)), "0.0s");
        assert_eq!(elapsed(Duration::from_millis(1500)), "1.5s");
        assert_eq!(elapsed(Duration::from_secs_f64(59.94)), "59.9s");
        // The boundary: a tenth of a second stops being worth a column.
        assert_eq!(elapsed(Duration::from_secs(60)), "1m 00s");
        assert_eq!(elapsed(Duration::from_secs(253)), "4m 13s");
        assert_eq!(elapsed(Duration::from_secs(3600)), "1h 00m 00s");
        assert_eq!(
            elapsed(Duration::from_secs(7 * 3600 + 8 * 60 + 9)),
            "7h 08m 09s"
        );
    }

    #[test]
    fn the_seconds_field_never_reads_as_a_whole_minute() {
        // `{:.0}` on 59.7 gives "60", so a 119.7-second run would print
        // "1m 60s". Truncating whole seconds is what avoids it.
        assert_eq!(elapsed(Duration::from_secs_f64(119.7)), "1m 59s");
    }
}
