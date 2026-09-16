//! The pipeline: source in, tile archive out. The real work lives in other
//! crates; this one orders the stages and moves tiles between them.

use std::path::{Path, PathBuf};

pub use lyv_progress::{Progress, Verbosity, show_memory};
pub use lyv_rank::Limits;

use anyhow::{Context, Result, bail};
use lyv_core::{Local, Pieces, TileCoord};
use lyv_store::TileStore;

pub mod footprint;
pub mod timings;
use timings::{Spent, timed};

/// What to build.
#[derive(Clone, Debug)]
pub struct Options {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Where the intermediate store lives. Deleted after a successful run
    /// unless `keep_store`; kept after a failed one.
    pub store: PathBuf,
    pub min_zoom: u32,
    /// The deepest zoom to build, or `None` to choose from the data. Too
    /// shallow a maxzoom is the usual reason a tileset drops most of its
    /// features (§8).
    pub max_zoom: Option<u32>,
    /// The shallowest maxzoom automatic selection may choose. The probe only
    /// goes deeper, so this is the answer for any layer that fits.
    pub min_auto_zoom: u32,
    /// The deepest maxzoom automatic selection may choose. At z16 a tile unit
    /// is already 2.6 cm at mid latitudes; the real limit is tile count (NZ is
    /// 717k tiles at z16, 11.5M at z18) with a level held in memory.
    pub max_auto_zoom: u32,
    /// Layer name inside the tiles.
    pub output_layer: String,
    /// The tileset's name in the archive's metadata. `None` is the output
    /// file's stem.
    pub output_name: Option<String>,
    /// Attributes to carry: `None` for all, empty for none. A name no feature
    /// has is an error.
    pub attributes: Option<Vec<String>>,
    pub extent: u32,
    /// Tile resolution at maxzoom, if finer than `extent`. Maxzoom's detail
    /// can't be recovered later, so it's worth spending resolution on. Must be
    /// a power-of-two multiple of `extent`.
    pub maxzoom_extent: Option<u32>,
    /// Margin around each output tile, in 4096ths of a tile. Zero skips the
    /// neighbour reads.
    pub buffer: u32,
    /// How much detail to discard; its unit names the algorithm.
    pub simplification: lyv_simplify::Tolerance,
    /// The drop-order algorithm, by name (`lyv_rank::NAMES`).
    pub ranker: String,
    /// Per-tile limits before features are dropped. Zero means no limit.
    pub limits: lyv_rank::Limits,
    /// How many vertices a tile may hold before the climb simplifies ahead.
    /// Zero means no limit. A separate lever from `limits`: dropping hardly
    /// thins geometry (the ranking keeps the big features), and simplifying
    /// never reduces the feature count.
    pub max_tile_vertices: usize,
    /// Replace a feature that shrinks to nothing with the smallest mark the
    /// grid can hold (a unit square or segment) rather than dropping it.
    /// Points are unaffected.
    pub dust: bool,
    /// How many levels below maxzoom to bucket features for ranking. At zero,
    /// the order within a maxzoom tile falls back to a size lottery; four
    /// spreads survivors across 256 sub-tiles (§8).
    pub rank_depth: u32,
    /// Threads to use; zero chooses. See [`Options::workers`] and
    /// [`Options::cpu_workers`].
    pub threads: usize,
    /// Keep the intermediate store after a successful run, for inspecting the
    /// pyramid (`inspect`, `rank_stats`).
    pub keep_store: bool,
    /// Bytes of the store to memory-map, per connection. Zero maps nothing;
    /// see [`lyv_store::DEFAULT_MMAP_BYTES`].
    pub mmap_bytes: u64,
}

impl Options {
    /// These options with the maxzoom settled, for the stages after ingest.
    pub fn with_max_zoom(&self, max_zoom: u32) -> Self {
        Self {
            max_zoom: Some(max_zoom),
            ..self.clone()
        }
    }

    /// Threads for a stage that reads the store: zero means the core count,
    /// capped at four.
    ///
    /// Concurrent store reads contend. Measured once at 4,513 reads/s for one
    /// reader, 5,323 for four and 2,671 for ten (cause unknown). Ten workers are
    /// now faster on the clock, but at a CPU cost: on 3.5M parcels, 4 s of wall
    /// cost 61 s of CPU, all contention (IPC fell from 3.57 to 2.74); on
    /// floodplains it's closer, 21% faster for 9.4% more CPU. Many jobs share a
    /// machine, so throughput per core decides, and four wins it. A single-job
    /// machine should pass `--threads`, which is obeyed as given.
    ///
    /// One if the core count is unavailable.
    pub fn workers(&self) -> usize {
        /// Past this, store reads start losing to each other.
        const USEFUL: usize = 4;
        match self.threads {
            0 => std::thread::available_parallelism()
                .map_or(1, std::num::NonZero::get)
                .min(USEFUL),
            n => n,
        }
    }

    /// Threads for a stage that only computes: every core. The cap above is
    /// about store reads, which these stages don't do. `--threads` still wins.
    pub fn cpu_workers(&self) -> usize {
        match self.threads {
            0 => std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
            n => n,
        }
    }

    /// The deepest zoom this run could build: the chosen one, or the automatic
    /// cap. Also the grid everything is projected onto before the maxzoom is
    /// chosen, then scaled down to it (§4).
    pub fn deepest(&self) -> u32 {
        self.max_zoom.unwrap_or(self.max_auto_zoom)
    }

    /// The tile resolution at a given zoom.
    pub fn extent_at(&self, zoom: u32) -> u32 {
        match self.maxzoom_extent {
            Some(finer) if zoom == self.deepest() => finer,
            _ => self.extent,
        }
    }

    /// The tileset's name: `output_name`, else the output file's stem.
    pub fn tileset_name(&self) -> Result<String> {
        match &self.output_name {
            Some(name) => Ok(name.clone()),
            None => self
                .output
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .with_context(|| {
                    format!(
                        "could not derive a tileset name from {}; pass --output-name",
                        self.output.display()
                    )
                }),
        }
    }

    fn validate(&self) -> Result<()> {
        // Up front, rather than after the whole build when the archive opens.
        self.tileset_name()?;
        if self.min_zoom > self.deepest() {
            bail!(
                "min_zoom {} is above max_zoom {}",
                self.min_zoom,
                self.deepest()
            );
        }
        // Subdividing halves the extent and merging halves coordinates, so
        // anything but a power of two drifts.
        if !self.extent.is_power_of_two() {
            bail!("extent {} is not a power of two", self.extent);
        }
        if let Some(finer) = self.maxzoom_extent {
            if !finer.is_power_of_two() {
                bail!("maxzoom extent {finer} is not a power of two");
            }
            if finer < self.extent {
                bail!(
                    "maxzoom extent {finer} is coarser than the extent {} used above it",
                    self.extent
                );
            }
        }
        // Coordinates are `Local` (i16), and the merge's doubled frame holds
        // up to twice an extent. Renderers can't use more resolution anyway.
        for (what, extent) in [
            ("extent", self.extent),
            ("maxzoom extent", self.maxzoom_extent.unwrap_or(self.extent)),
        ] {
            if extent > lyv_core::MAX_EXTENT {
                bail!(
                    "{what} {extent} is above the maximum of {}",
                    lyv_core::MAX_EXTENT
                );
            }
        }
        if self.min_auto_zoom > self.max_auto_zoom {
            bail!(
                "min_auto_zoom {} is below max_auto_zoom {}",
                self.min_auto_zoom,
                self.max_auto_zoom
            );
        }
        if !self.input.exists() {
            bail!("{} does not exist", self.input.display());
        }
        // Check output directories exist before the work, not after minutes
        // of it. Existence only: writability can change before the write, so
        // permissions and full disks fail where they happen.
        let output_dir = containing(&self.output);
        let store_dir = containing(&self.store);
        if !output_dir.is_dir() {
            bail!(
                "the output directory {} does not exist",
                output_dir.display()
            );
        }
        // Only worth its own message when `--store` points elsewhere.
        if store_dir != output_dir && !store_dir.is_dir() {
            bail!("the store directory {} does not exist", store_dir.display());
        }
        Ok(())
    }
}

/// Read the source and build the maxzoom level.
///
/// Nothing holds the whole layer: features go into a batch bounded by vertex
/// count, the batch is cut and bucketed across every core, and its coordinates
/// are dropped. (Holding the layer used to cost 1.7x the input.) A second probe
/// round re-reads the source — 53 s on floodplains, and only when the first
/// candidate zoom is over budget.
pub fn ingest(
    store: &mut dyn TileStore,
    options: &Options,
    progress: &Progress,
) -> Result<Ingested> {
    // The quantization grid, fixed before the read because it has to be: the
    // maxzoom may not be chosen yet, so everything is projected onto the
    // deepest grid this run could want and cut down to whichever is chosen.
    let grid_zoom = options.deepest();
    let detail = options.extent_at(grid_zoom);
    let cap = options.max_auto_zoom;

    // Gathered on the first pass and reused by any that follow, because none
    // of it depends on the zoom: a feature's size, its attribute cost, its
    // attributes, the layer's field types and its extent.
    let mut facts = Facts::default();
    let mut fields: std::collections::BTreeMap<String, lyv_archive::FieldType> =
        std::collections::BTreeMap::new();
    let mut extent_of_data: Option<Extent> = None;
    let mut polygons: std::collections::HashSet<u64> = std::collections::HashSet::new();

    let mut z = match options.max_zoom {
        Some(given) => given,
        None => options.min_zoom.max(options.min_auto_zoom).min(cap),
    };

    let (max_zoom, mut pieces, layer, covered, simplified, tangled) = loop {
        let first = extent_of_data.is_none() && facts.features == 0;
        let pass = stream_pass(
            store,
            options,
            Candidate {
                zoom: z,
                grid_zoom,
                detail,
            },
            first,
            &mut facts,
            &mut fields,
            &mut extent_of_data,
            &mut polygons,
            progress,
        )?;

        // Said rather than counted silently: a tileset with fewer features
        // than the source is a surprise worth one line, and the collapsed
        // count is the signal that maxzoom is too shallow for part of the
        // data.
        if first && let Some(report) = pass.skipped.report(grid_zoom) {
            progress.say(&format!("  {report}"));
        }

        // A given maxzoom is not a candidate; there is nothing to decide.
        if options.max_zoom.is_some() {
            break (
                z,
                pass.pieces,
                pass.layer,
                pass.covered,
                pass.simplified,
                pass.tangled,
            );
        }

        let worst = worst_tile(&pass.pieces, &facts.attr_bytes);
        let overflow = options.limits.overflow(worst);
        progress.say(&format!(
            "  z{z} would hold {} tiles, worst about {} KB / {} features{}",
            pass.pieces.len(),
            worst.bytes / 1024,
            worst.features,
            if overflow > 1.0 {
                format!(", {overflow:.1}x over")
            } else {
                String::new()
            },
        ));

        if overflow <= 1.0 || z >= cap {
            if overflow > 1.0 {
                progress.say(&format!(
                    "  capped at z{cap}; the rest comes off by dropping features"
                ));
            }
            break (
                z,
                pass.pieces,
                pass.layer,
                pass.covered,
                pass.simplified,
                pass.tangled,
            );
        }
        // A level down quarters a tile's features, so log4 of the overflow is
        // the number of levels wanted. At least one, or an overflow under 4
        // would round to no move at all and loop forever.
        let levels = (overflow.log2() / 2.0).ceil().max(1.0) as u32;
        z = (z + levels).min(cap);
    };

    // Only for the kept pass: each probe's tile boundaries give a different
    // count.
    if tangled > 0 {
        progress.say(&format!(
            "  dropped {tangled} self-intersecting {}",
            if tangled == 1 { "feature" } else { "features" },
        ));
    }

    // Unprojected to lon/lat here, while the grid it was measured on is in
    // scope. Kept in grid units, it was later unprojected at the chosen zoom
    // rather than the deeper grid zoom, silently doubling every coordinate so
    // the bounds pointed at open ocean.
    let bounds = extent_of_data.map(|extent| {
        let [min_x, min_y, max_x, max_y] = extent.narrower();
        let (min_lon, max_lat) =
            lyv_core::mercator::unproject_global(min_x, min_y, grid_zoom, detail);
        let (max_lon, min_lat) =
            lyv_core::mercator::unproject_global(max_x, max_y, grid_zoom, detail);
        [min_lon, min_lat, max_lon, max_lat]
    });

    // From here on, everything must agree which zoom is maxzoom. Before
    // this, `extent_at` answered for the automatic cap, and cutting with
    // it doubled every tile address.
    let settled = options.with_max_zoom(max_zoom);
    let options = &settled;
    let extent = options.extent_at(max_zoom);

    // Tiles and features added together: the units don't match, but the bar
    // only needs to move at roughly the right speed.
    progress.phase(
        lyv_progress::Phase::Ingest,
        (pieces.len() * 2 + facts.features) as u64,
    );

    if !polygons.is_empty() {
        // Close each tile's ring chains along its edge. Only polygon features'
        // chains: a line crossing a tile looks just like a clipped ring.
        let addresses: Vec<TileCoord> = pieces.keys().copied().collect();
        progress.stage(&format!("closing z{max_zoom}"), Some("tiles"));
        progress.expect(addresses.len() as u64);
        for coord in addresses {
            progress.item();
            // Taken out of the map, not cloned: cloning copied every chain.
            let chains = pieces.remove(&coord).expect("a tile listed a moment ago");
            let (rings, lines): (Vec<_>, Vec<_>) = chains
                .into_iter()
                .partition(|c| polygons.contains(&c.feature));
            if rings.is_empty() {
                // Nothing to close; put the tile back as it was.
                pieces.insert(coord, lines);
                continue;
            }
            let mut closed = lyv_close::close_rings(&rings, extent);
            closed.extend(lines);
            // Closing grows vectors by pushing; without shrinking, the slack
            // came back (+1.5 GB on floodplains, the run's peak).
            shrink_tile(&mut closed);
            pieces.insert(coord, closed);
        }
    }

    // Then the tiles swallowed whole. After closing, so each tile's chain
    // order (and bytes) is unchanged.
    if !covered.is_empty() {
        progress.stage(&format!("filling z{max_zoom}"), Some("tiles"));
        progress.expect(covered.len() as u64);
        let square = lyv_coverage::full_tile(extent as Local);
        for (coord, feature) in covered {
            progress.item();
            pieces.entry(coord).or_default().push(lyv_core::Chain {
                feature,
                points: square.clone(),
            });
        }
    }

    // Again: closing and filling both rebuild vectors. This one matters most,
    // since the ranking below is the run's peak and holds the level.
    shrink_the_cut(&mut pieces);

    // The ranking was bucketed during the read; draw it and store it.
    store_ranks(store, options, layer, &facts, progress)?;

    let count = pieces.len();
    progress.stage(&format!("store z{max_zoom}"), Some("tiles"));
    progress.expect(count as u64);
    // Decided here, where both what the cut did and maxzoom's scale are known.
    // Usually the same scale, leaving only the residue (see `Simplified`).
    let thinning = simplified.still_wants(
        options.simplification,
        lyv_simplify::Scale::new(max_zoom, extent),
    );
    let (worst, level) = store_level_in_parallel(
        store,
        options,
        pieces,
        extent,
        thinning,
        &facts.attr_bytes,
        options.cpu_workers(),
        progress,
    )?;
    Ok(Ingested {
        max_zoom,
        worst,
        level,
        fields: fields.into_iter().collect(),
        bounds,
    })
}

/// The data's extent on the grid, measured twice: with longitude wrapped into
/// -180..180, and into 0..360. Data crossing the antimeridian is narrow only in
/// the second, and data crossing the prime meridian only in the first.
#[derive(Clone, Copy)]
struct Extent {
    plain: [i64; 4],
    shifted: [i64; 4],
}

impl Default for Extent {
    fn default() -> Self {
        let empty = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
        Self {
            plain: empty,
            shifted: empty,
        }
    }
}

impl Extent {
    /// Take in one vertex. `world` is the grid's width.
    fn add(&mut self, point: geo_types::Coord<i64>, world: i64) {
        // Nearly every vertex is already in range; `rem_euclid` divides.
        let x = if (0..world).contains(&point.x) {
            point.x
        } else {
            point.x.rem_euclid(world)
        };
        let shifted = if x < world / 2 { x + world } else { x };
        for (bounds, x) in [(&mut self.plain, x), (&mut self.shifted, shifted)] {
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(point.y);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(point.y);
        }
    }

    /// Whichever box is narrower, the -180..180 one on a tie. The other's
    /// longitudes run past 180.
    fn narrower(self) -> [i64; 4] {
        if self.shifted[2] - self.shifted[0] < self.plain[2] - self.plain[0] {
            self.shifted
        } else {
            self.plain
        }
    }
}

/// The zoom a pass is testing, and the grid everything is measured on.
#[derive(Clone, Copy)]
struct Candidate {
    /// The candidate maxzoom: what the tiles are addressed at.
    zoom: u32,
    /// The quantization zoom the source was projected onto.
    grid_zoom: u32,
    /// The extent of that grid. Used instead of `options.extent_at(zoom)`,
    /// which answers wrongly while the choice is pending.
    detail: u32,
}

/// What the reader and cutter hand back: the reader's skip counts, then what
/// the cutter accumulated.
type Cut = (
    lyv_source::Skipped,
    Pieces,
    lyv_rank::Layer,
    Vec<(TileCoord, u64)>,
    u64,
);

/// What one pass over the source produced.
struct Pass {
    pieces: Pieces,
    layer: lyv_rank::Layer,
    /// Tiles a polygon swallowed whole, in feature order: found during the cut,
    /// filled after closing, to keep each tile's chain order.
    covered: Vec<(TileCoord, u64)>,
    skipped: lyv_source::Skipped,
    /// What the cut already simplified at, so maxzoom doesn't do it again.
    simplified: lyv_simplify::Simplified,
    /// Features dropped because their pieces wouldn't close back to their own
    /// area. See `closing_would_lose_it`.
    tangled: u64,
}

/// Read the whole source once, cutting and bucketing as it goes.
///
/// `record` is true only for the first pass: attributes, field types, sizes and
/// the data extent don't depend on the zoom, so later passes skip them.
#[allow(clippy::too_many_arguments)]
fn stream_pass(
    store: &mut dyn TileStore,
    options: &Options,
    at: Candidate,
    record: bool,
    facts: &mut Facts,
    fields: &mut std::collections::BTreeMap<String, lyv_archive::FieldType>,
    extent_of_data: &mut Option<Extent>,
    polygons: &mut std::collections::HashSet<u64>,
    progress: &Progress,
) -> Result<Pass> {
    // Bounded by vertices, not features: polygons and parcels differ by three
    // orders of magnitude. 2M vertices is 32 MB, plenty of work per worker.
    const BATCH_VERTICES: usize = 2_000_000;
    const BATCH_FEATURES: usize = 20_000;

    let depth = options
        .rank_depth
        .min(at.detail.trailing_zeros().saturating_sub(4));
    let ranking = Bucketing {
        zoom: at.zoom + depth,
        extent: at.detail >> depth,
    };

    progress.stage("reading", Some("features"));
    let workers = options.cpu_workers();
    let mut batch: Vec<Feature> = Vec::new();
    let mut held = 0usize;

    // Reading and cutting overlap, one batch in flight: the reader fills the
    // next while the cutter works on the last, and blocks if the cutter falls
    // behind, so memory stays at two batches. (Taking turns left nine cores
    // idle through the parse.) Batches keep their order, so output is
    // unchanged.
    let (hand_over, take) = std::sync::mpsc::sync_channel::<Vec<Feature>>(1);
    // When the source last handed over a feature. The gap to the next is time
    // inside `lyv_source`, which can't be timed from here otherwise.
    let mut handed_back: Option<std::time::Instant> = None;

    let (skipped, mut pieces, mut layer, mut covered, tangled) =
        std::thread::scope(|scope| -> Result<Cut> {
            let cutting = scope.spawn(move || {
                let mut pieces = Pieces::new();
                let mut layer = lyv_rank::Layer::default();
                let mut covered: Vec<(TileCoord, u64)> = Vec::new();
                let mut tangled = 0u64;
                for batch in take {
                    let vertices: usize = batch
                        .iter()
                        .map(|feature| feature.parts.iter().map(Vec::len).sum::<usize>())
                        .sum();
                    let bytes = vertices * std::mem::size_of::<geo_types::Coord<i64>>();
                    timed(Spent::Cutting, bytes, || {
                        cut_batch(
                            &batch,
                            at,
                            ranking,
                            options.simplification,
                            workers,
                            &mut pieces,
                            &mut layer,
                            &mut covered,
                            &mut tangled,
                        )
                    });
                }
                (pieces, layer, covered, tangled)
            });

            let skipped = lyv_source::read(
                &options.input,
                at.grid_zoom,
                at.detail,
                &lyv_source::Selection {
                    attributes: options.attributes.clone(),
                },
                // Known before the first feature for FlatGeobuf (header) and
                // GeoJSON (parsed whole).
                &mut |total| {
                    progress.expect(total);
                    // Twice when the probe goes deeper; the bar doesn't fall.
                    progress.phase(lyv_progress::Phase::Reading, total);
                },
                &mut |feature| {
                    if let Some(since) = handed_back.take() {
                        timings::add(Spent::SourceParse, since.elapsed());
                    }
                    // Every pass: the cut carries it, and it's a pure function
                    // of the geometry.
                    let size = size_of(&feature);
                    if record {
                        facts.features += 1;
                        facts.attr_bytes.insert(
                            feature.id,
                            lyv_mvt::attribute_cost(&feature.properties) as u32,
                        );
                        if feature.kind == lyv_source::Kind::Rings {
                            polygons.insert(feature.id);
                        }
                        if !feature.properties.is_empty() {
                            timed(
                                Spent::PutFeature,
                                lyv_mvt::attribute_cost(&feature.properties),
                                || store.put_feature(feature.id, &feature.properties),
                            )?;
                            for (key, value) in &feature.properties {
                                let kind = field_type(value);
                                fields
                                    .entry(key.clone())
                                    // A key seen with two types is declared
                                    // `String`, the one type every value can be
                                    // read as.
                                    .and_modify(|seen| {
                                        if *seen != kind {
                                            *seen = lyv_archive::FieldType::String;
                                        }
                                    })
                                    .or_insert(kind);
                            }
                        }
                        let world = i64::from(at.detail) << at.grid_zoom;
                        for part in &feature.parts {
                            for point in part {
                                extent_of_data
                                    .get_or_insert_with(Extent::default)
                                    .add(*point, world);
                            }
                        }
                    }

                    held += feature.parts.iter().map(Vec::len).sum::<usize>();
                    batch.push(Feature {
                        id: feature.id,
                        kind: feature.kind,
                        parts: feature.parts,
                        size,
                    });
                    if held >= BATCH_VERTICES || batch.len() >= BATCH_FEATURES {
                        // The coordinates go now, handed to the cutter, not at
                        // the end of the read.
                        let full = std::mem::take(&mut batch);
                        timed(Spent::WaitingToCut, 0, || hand_over.send(full))
                            .map_err(|_| anyhow::anyhow!("the cutting thread stopped early"))?;
                        held = 0;
                    }
                    progress.item();
                    handed_back = Some(std::time::Instant::now());
                    Ok(())
                },
            )?;
            if !batch.is_empty() {
                timed(Spent::WaitingToCut, 0, || hand_over.send(batch))
                    .map_err(|_| anyhow::anyhow!("the cutting thread stopped early"))?;
            }
            // Closed so the cutter's loop ends.
            drop(hand_over);
            let (pieces, layer, covered, tangled) = cutting.join().expect("the cutting thread");
            Ok((skipped, pieces, layer, covered, tangled))
        })?;

    // Nothing appends again.
    shrink_the_cut(&mut pieces);
    layer.shrink();
    covered.shrink_to_fit();
    if record {
        facts.attr_bytes.shrink_to_fit();
        polygons.shrink_to_fit();
    }

    // Everything this pass built is settled here: the moment to report what
    // `reading` holds. First pass only.
    if record
        && let Some(report) = footprint::Report::at(
            "reading",
            vec![
                footprint::of_pieces(&pieces),
                footprint::of_layer(&layer),
                footprint::of_facts(facts.attr_bytes.capacity()),
            ],
        )
    {
        report.show(progress);
    }

    Ok(Pass {
        pieces,
        layer,
        covered,
        skipped,
        tangled,
        // Applied on maxzoom's own grid (`scale_down` runs first). `at.detail`,
        // not `extent_at`, which is wrong while the choice is pending.
        simplified: lyv_simplify::Simplified::at(
            options.simplification,
            lyv_simplify::Scale::new(at.zoom, at.detail),
        ),
    })
}

/// Hand back the capacity the cut over-reserved.
///
/// Tile chain lists and chain point lists both grow by pushing, so carry about
/// a third slack (36% measured). The cut already shrinks each chain as it
/// finishes it (`lyv_cut::emit`), so here that's a free no-op except for
/// chains built by closing and filling. The per-tile lists can only be shrunk
/// here, once no batch will append to them. Never call it earlier: the next
/// append would reallocate.
fn shrink_the_cut(pieces: &mut Pieces) {
    for chains in pieces.values_mut() {
        shrink_tile(chains);
    }
}

/// As [`shrink_the_cut`], for one tile.
fn shrink_tile(chains: &mut Vec<lyv_core::Chain>) {
    for chain in chains.iter_mut() {
        chain.points.shrink_to_fit();
    }
    chains.shrink_to_fit();
}

/// The grid the ranking buckets into: deeper than maxzoom, so evenstar spreads
/// survivors within a maxzoom tile. `z + k` with `extent >> k` is the same
/// grid subdivided; no rescaling.
#[derive(Clone, Copy)]
struct Bucketing {
    zoom: u32,
    extent: u32,
}

/// Cut and bucket one batch across every core, appending to the pass.
///
/// Cut shares merge in feature order, because a tile's chain order decides its
/// bytes; ranking shares merge in any order, since `draw` sorts. Features are
/// simplified before they are cut (see `lyv_simplify::simplify_part`).
#[allow(clippy::too_many_arguments)]
fn cut_batch(
    batch: &[Feature],
    at: Candidate,
    ranking: Bucketing,
    tolerance: lyv_simplify::Tolerance,
    workers: usize,
    pieces: &mut Pieces,
    layer: &mut lyv_rank::Layer,
    covered: &mut Vec<(TileCoord, u64)>,
    tangled: &mut u64,
) {
    let share = batch.len().div_ceil(workers.max(1)).max(1);
    let chunks: Vec<&[Feature]> = batch.chunks(share).collect();
    type Part = (Pieces, lyv_rank::Layer, Vec<(TileCoord, u64)>, u64);
    let mut parts: Vec<Option<Part>> = (0..chunks.len()).map(|_| None).collect();

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in &chunks {
            handles.push(scope.spawn(move || {
                let mut cut = Pieces::new();
                let mut bucketed = lyv_rank::Layer::default();
                let mut covered: Vec<(TileCoord, u64)> = Vec::new();
                let mut buffer = Vec::new();
                let shift = at.grid_zoom - at.zoom;
                // One feature's pieces at a time, so the area check has them to
                // itself; moved into `cut` if it passes. Moving a tile's chain
                // list keeps its order, so the bytes don't change.
                let mut alone = Pieces::new();
                let mut mine: Vec<(TileCoord, u64)> = Vec::new();
                let mut tangled = 0u64;
                for feature in *chunk {
                    // Scaled and simplified once, shared by the cut, coverage
                    // and bucketing. `scale_down` rounds, so each must see the
                    // same copy: a vertex near a tile edge could otherwise land
                    // on different sides and change which features survive.
                    let thinning = feature.kind != lyv_source::Kind::Points && !tolerance.is_none();
                    let moved;
                    let feature = if shift == 0 && !thinning {
                        feature
                    } else {
                        let closed = feature.kind == lyv_source::Kind::Rings;
                        moved = Feature {
                            id: feature.id,
                            kind: feature.kind,
                            // Measured before simplifying, so it ranks the same.
                            size: feature.size,
                            parts: feature
                                .parts
                                .iter()
                                .map(|part| {
                                    let scaled = if shift == 0 {
                                        &part[..]
                                    } else {
                                        buffer.clear();
                                        buffer.extend(
                                            part.iter().map(|point| scale_down(*point, shift)),
                                        );
                                        &buffer[..]
                                    };
                                    if thinning {
                                        lyv_simplify::simplify_part(scaled, tolerance, closed)
                                    } else {
                                        scaled.to_vec()
                                    }
                                })
                                .collect(),
                        };
                        &moved
                    };
                    cut_feature(feature, at.zoom, at.detail, &mut alone);
                    // Right after the cut, while only this feature's chains are
                    // here.
                    mine.clear();
                    if feature.kind == lyv_source::Kind::Rings {
                        lyv_coverage::for_each_covered(
                            feature.id,
                            &feature.parts,
                            at.zoom,
                            at.detail,
                            &alone,
                            &mut |coord| mine.push((coord, feature.id)),
                        );
                    }

                    // Also now: the only moment what closing will produce can
                    // be compared with the feature itself.
                    if feature.kind == lyv_source::Kind::Rings
                        && closing_would_lose_it(feature, &alone, &mine, at.detail)
                    {
                        alone.clear();
                        tangled += 1;
                        continue;
                    }

                    covered.append(&mut mine);
                    for (coord, chains) in std::mem::take(&mut alone) {
                        cut.entry(coord).or_default().extend(chains);
                    }
                    bucket_feature(feature, ranking.zoom, ranking.extent, &mut bucketed);
                }
                (cut, bucketed, covered, tangled)
            }));
        }
        for (slot, handle) in parts.iter_mut().zip(handles) {
            *slot = Some(handle.join().expect("a cutting thread"));
        }
    });

    for (cut, bucketed, found, lost) in parts.into_iter().flatten() {
        absorb(pieces, cut);
        layer.merge(bucketed);
        covered.extend(found);
        *tangled += lost;
    }
}

/// Simplify, encode and weigh every maxzoom tile, across threads. Workers
/// compute; the calling thread writes. Weighing here saves reading the level
/// back (16.5 s on floodplains).
#[allow(clippy::too_many_arguments)]
fn store_level_in_parallel(
    store: &mut dyn TileStore,
    options: &Options,
    pieces: Pieces,
    extent: u32,
    thinning: lyv_simplify::Tolerance,
    attr_bytes: &std::collections::HashMap<u64, u32>,
    workers: usize,
    progress: &Progress,
) -> Result<(lyv_rank::Worst, Pieces)> {
    // A stack the workers pop from, not a vector they index: popping frees each
    // tile's input once encoded, so the level isn't held twice (that was the
    // run's peak). Order doesn't matter: `worst` is a maximum, `level` is keyed
    // by coordinate, the store by address. Reversed so pops go in address
    // order, which the store's index prefers.
    let tiles = std::sync::Mutex::new({
        let mut remaining: Vec<(TileCoord, Vec<lyv_core::Chain>)> = pieces.into_iter().collect();
        remaining.reverse();
        remaining
    });

    // A few tiles per worker: one slow tile doesn't stall the rest, and memory
    // stays flat while ingest still holds the cut level.
    #[allow(clippy::type_complexity)]
    let (send, receive) = std::sync::mpsc::sync_channel::<(
        TileCoord,
        Vec<u8>,
        Vec<lyv_core::Chain>,
        lyv_rank::Worst,
    )>(workers * 4);

    let mut worst = lyv_rank::Worst::default();
    let mut level = Pieces::new();
    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            let send = send.clone();
            let tiles = &tiles;
            scope.spawn(move || {
                loop {
                    // Held only for the pop; `chains` is owned and dropped at
                    // the end of the iteration.
                    let taken = tiles.lock().expect("the tile stack").pop();
                    let Some((coord, chains)) = taken else {
                        return;
                    };
                    // Usually just the residue: the cut already simplified on
                    // this grid, leaving zero-length segments from clipping.
                    let chains = drop_collapsed(
                        lyv_simplify::simplify_chains(&chains, extent, thinning),
                        extent,
                        options.dust,
                    );
                    let encoded = lyv_mvt::encode_chains(&options.output_layer, extent, &chains);
                    let weight = weigh_with(&chains, &encoded, attr_bytes);
                    // Decoded here, on a worker, so the climb needn't read the
                    // level back (28.9 s on floodplains). Decoded rather than
                    // kept as `chains`, because the encoder reorders chains
                    // (grouping by feature, assembling polygons) and the merge
                    // reads that order.
                    // `scope` re-raises a worker's panic once the others finish.
                    let canonical = lyv_mvt::decode_chains(&encoded)
                        .expect("a tile this pass just encoded should decode");
                    // A closed channel means the writer gave up.
                    if send.send((coord, encoded, canonical, weight)).is_err() {
                        return;
                    }
                }
            });
        }
        // Dropped so the loop below ends with the last worker.
        drop(send);

        for (coord, encoded, canonical, weight) in receive {
            timed(Spent::PutTile, encoded.len(), || store.put(coord, &encoded))?;
            // A maximum, so arrival order can't change it.
            worst = worst.max(weight);
            level.insert(coord, canonical);
            progress.item();
        }
        Ok(())
    })?;

    Ok((worst, level))
}

/// What the read learned about each feature besides its geometry, measured on
/// the whole feature before it was cut.
#[derive(Default)]
struct Facts {
    /// How many features the recording pass saw. (Sizes live in
    /// `lyv_rank::Layer`.)
    features: usize,
    /// Each feature's attribute cost in an encoded tile.
    attr_bytes: std::collections::HashMap<u64, u32>,
}

/// A feature's projected geometry, kept between the read and the cut.
struct Feature {
    id: u64,
    kind: lyv_source::Kind,
    parts: Vec<Vec<geo_types::Coord<i64>>>,
    /// Carried, since the cutter runs beside the reader and can't borrow from
    /// it. Handed to `lyv_rank::Layer`, the only place sizes are kept.
    size: lyv_rank::Size,
}

/// What `ingest` settled on.
pub struct Ingested {
    /// The maxzoom, given or chosen.
    pub max_zoom: u32,
    /// The maxzoom level, decoded, so the climb doesn't read it back.
    pub level: Pieces,
    /// The heaviest maxzoom tile, measured as written, so the climb can thin
    /// maxzoom without re-reading it.
    pub worst: lyv_rank::Worst,
    /// Every attribute and its type, for the archive's metadata.
    pub fields: Vec<(String, lyv_archive::FieldType)>,
    /// The data's extent in lon/lat, or `None` if it held no geometry. Data
    /// crossing the antimeridian has its eastern longitude past 180.
    pub bounds: Option<[f64; 4]>,
}

/// Move a coordinate to a grid `shift` levels coarser (not zero: it would
/// underflow). Rounds to nearest (§4); a second rounding costs at most one more
/// unit, paid only when the chosen maxzoom is shallower than the cap.
fn scale_down(point: geo_types::Coord<i64>, shift: u32) -> geo_types::Coord<i64> {
    let half = 1i64 << (shift - 1);
    geo_types::Coord {
        x: (point.x + half) >> shift,
        y: (point.y + half) >> shift,
    }
}

/// Cut one feature into tiles. Its parts are already on the grid of `z`.
fn cut_feature(feature: &Feature, z: u32, extent: u32, pieces: &mut Pieces) {
    for part in &feature.parts {
        match feature.kind {
            // A point's part is one coordinate in one tile.
            lyv_source::Kind::Points => {
                if let Some(at) = part.first() {
                    lyv_cut::cut_point(feature.id, *at, z, extent, pieces);
                }
            }
            lyv_source::Kind::Lines => {
                lyv_cut::cut_linestring(feature.id, part, z, extent, pieces);
            }
            lyv_source::Kind::Rings => {
                lyv_cut::cut_ring(feature.id, part, z, extent, pieces);
            }
        }
    }
}

/// Append one share's chains to another's, tile by tile. Shares arrive in
/// feature order, so this matches a single-threaded cut.
fn absorb(into: &mut Pieces, from: Pieces) {
    for (coord, chains) in from {
        match into.entry(coord) {
            std::collections::btree_map::Entry::Occupied(mut held) => {
                held.get_mut().extend(chains);
            }
            // Moved, for tiles only one share touched.
            std::collections::btree_map::Entry::Vacant(empty) => {
                empty.insert(chains);
            }
        }
    }
}

/// The largest tile in a level, by each limit independently.
fn worst_tile(
    pieces: &Pieces,
    attr_bytes: &std::collections::HashMap<u64, u32>,
) -> lyv_rank::Worst {
    let mut worst = lyv_rank::Worst::default();
    for chains in pieces.values() {
        let features: std::collections::HashSet<u64> =
            chains.iter().map(|chain| chain.feature).collect();
        let attributes: usize = features
            .iter()
            .map(|id| attr_bytes.get(id).copied().unwrap_or(0) as usize)
            .sum();
        worst.bytes = worst.bytes.max(lyv_mvt::geometry_cost(chains) + attributes);
        worst.features = worst.features.max(features.len());
    }
    worst
}

/// How much geometry a finished level holds per tile: `p90` catches a level
/// that's uniformly dense; `worst` catches one far-too-heavy tile.
///
/// Not the mean, which on floodplains rises up the pyramid (19k at z10 to 197k
/// at z2) while the heavy tiles first appear at z8, where the mean is 65k.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Density {
    p90: usize,
    worst: usize,
}

impl Density {
    /// Measure a level from its tiles' vertex counts.
    fn of(counts: impl Iterator<Item = usize>) -> Self {
        let mut counts: Vec<usize> = counts.collect();
        if counts.is_empty() {
            return Self::default();
        }
        counts.sort_unstable();
        let index = ((counts.len() as f64 - 1.0) * 0.9).round() as usize;
        Self {
            p90: counts[index],
            worst: counts[counts.len() - 1],
        }
    }

    /// The level as it will ship, with each tile's margin added.
    ///
    /// The merge measures tiles before the buffer is added, which is 4.7–6.4%
    /// more vertices. That once left a tile 3% over the trigger unnoticed.
    fn buffered(self, growth: f64) -> Self {
        let grow = |count: usize| (count as f64 * growth) as usize;
        Self {
            p90: grow(self.p90),
            worst: grow(self.worst),
        }
    }

    /// Whether this level should make the next one coarser. The per-tile
    /// trigger is twice the limit: a few heavy tiles are normal, and coarsening
    /// a whole level costs detail everywhere.
    fn over(self, limit: usize) -> bool {
        limit != 0 && (self.p90 > limit || self.worst > 2 * limit)
    }
}

/// How much the encode pass grows a tile's vertex count: the margin covers
/// `1 + 2·buffer/4096` of a tile each way (6.3% more area at the default 64).
/// An over-estimate for lines, which aren't buffered — the safe direction.
fn buffer_growth(buffer: u32) -> f64 {
    (1.0 + 2.0 * f64::from(buffer) / 4096.0).powi(2)
}

/// How far ahead the climb may simplify. Each level doubles the tolerance; at
/// three (eightfold) chains are too short to simplify further. Capped because
/// every level re-measures, and a layer simplification can't help would
/// otherwise escalate to zoom 0.
const MAX_LEAD: u32 = 3;

/// Build every level from max_zoom up to min_zoom.
///
/// Each level is built in memory, measured, thinned, encoded and stored, then
/// handed straight to the next iteration as its children — never read back.
/// Merging runs per parent across every core; the grouping is one serial pass.
pub fn merge_up(
    store: &mut dyn TileStore,
    options: &Options,
    maxzoom_worst: lyv_rank::Worst,
    mut children: Pieces,
    progress: &Progress,
) -> Result<()> {
    progress.stage("ranks", None);
    let ranks = load_ranks(store)?;
    // The keep-count: narrowed up the climb, never widened. Maxzoom itself is
    // checked first, since it has no level below to be fixed from.
    let mut cutoff = u32::try_from(ranks.len()).unwrap_or(u32::MAX);

    // The whole climb's work, estimated as maxzoom's tile count: the levels
    // above hold about 40% of it, merged and written always and thinned when
    // over, so about 2.5 passes over 0.4. Measured at 0.94–1.16 of it.
    progress.phase(lyv_progress::Phase::Climb, children.len() as u64);

    // Maxzoom is thinned like any level, in memory, on the level ingest handed
    // over.
    let deepest = options.deepest();
    let overflow = options.limits.overflow(maxzoom_worst);
    let narrowed = lyv_rank::narrow(cutoff, overflow);
    if narrowed < cutoff {
        progress.stage(&format!("thin z{deepest}"), Some("tiles"));
        progress.expect(children.len() as u64);
        let extent = options.extent_at(deepest);
        for (coord, chains) in &mut children {
            progress.item();
            chains.retain(|chain| {
                ranks
                    .get(&chain.feature)
                    .is_none_or(|(rank, _)| *rank < narrowed)
            });
            store.put(
                *coord,
                &lyv_mvt::encode_chains(&options.output_layer, extent, chains),
            )?;
        }
        progress.say(&format!(
            "  z{deepest} worst tile {} KB / {} features, {overflow:.1}x over: keeping {narrowed} of {cutoff} features",
            maxzoom_worst.bytes / 1024,
            maxzoom_worst.features,
        ));
        cutoff = narrowed;
    }

    // How many levels ahead the climb simplifies. Raised when a level comes out
    // too dense and never lowered: discarded detail can't come back. Maxzoom is
    // measured but not re-simplified; its density sets the first merge's
    // tolerance.
    let mut lead = 0;
    let mut density = Density::of(
        children
            .values()
            .map(|chains| chains.iter().map(|chain| chain.points.len()).sum::<usize>()),
    )
    .buffered(buffer_growth(options.buffer));

    for z in (options.min_zoom..options.deepest()).rev() {
        let child_extent = options.extent_at(z + 1);
        let parent_extent = options.extent_at(z);

        if density.over(options.max_tile_vertices) && lead < MAX_LEAD {
            lead += 1;
            progress.say(&format!(
                "  z{} p90 {} / worst {} vertices a tile, over {}: simplifying z{z} and up {}x",
                z + 1,
                density.p90,
                density.worst,
                options.max_tile_vertices,
                1 << lead,
            ));
        }

        progress.stage(&format!("merge z{z}"), Some("tiles"));
        // Taken: the merge consumes the level, leaving an empty map to refill.
        let (mut parents, worst) = merge_level_in_parallel(
            std::mem::take(&mut children),
            options,
            options.simplification.coarsened(lead),
            child_extent,
            parent_extent,
            &ranks,
            options.cpu_workers(),
            progress,
        );

        // Measured as merged, then thinned before storing, so dropping makes
        // the rest of the climb cheaper.
        let overflow = options.limits.overflow(worst);
        let narrowed = lyv_rank::narrow(cutoff, overflow);
        if narrowed < cutoff {
            progress.stage(&format!("thin z{z}"), Some("tiles"));
            progress.expect(parents.len() as u64);
            for parent in &mut parents {
                progress.item();
                parent.chains.retain(|chain| {
                    // A feature the ranking never saw shouldn't happen; keeping
                    // it is the safe way to be wrong.
                    ranks
                        .get(&chain.feature)
                        .is_none_or(|(rank, _)| *rank < narrowed)
                });
                parent.encoded =
                    lyv_mvt::encode_chains(&options.output_layer, parent_extent, &parent.chains);
            }
            progress.say(&format!(
                "  z{z} worst tile {} KB / {} features, {overflow:.1}x over: keeping {narrowed} of {cutoff} features",
                worst.bytes / 1024,
                worst.features,
            ));
            cutoff = narrowed;
        }

        // Measured after thinning: the next level's tolerance should follow
        // the tile that will actually be served.
        density = Density::of(
            parents
                .iter()
                .map(|parent| parent.chains.iter().map(|c| c.points.len()).sum()),
        )
        .buffered(buffer_growth(options.buffer));

        // Stored once; the next round needs no read.
        progress.stage(&format!("write z{z}"), Some("tiles"));
        progress.expect(parents.len() as u64);
        for parent in parents {
            progress.item();
            timed(Spent::PutTile, parent.encoded.len(), || {
                store.put(parent.coord, &parent.encoded)
            })?;
            // Decoded rather than passed along as chains: the encoder reorders
            // them (grouping by feature, assembling polygons), and the next
            // merge reads that order. Skipping the round trip changed the
            // output. A sort mimicking it was tried and diverged on real data;
            // matching it would mean a second copy of the encoder's polygon
            // logic. The decode costs about 10 s on 3.5M parcels.
            let coord = parent.coord;
            children.insert(
                coord,
                lyv_mvt::decode_chains(&parent.encoded).with_context(|| {
                    format!("re-reading tile {}/{}/{}", coord.z, coord.x, coord.y)
                })?,
            );
        }
        // Expect about this level's tile count again before the climb ends:
        // each level holds about 0.29 of the one below, so everything above is
        // about 0.41 of it, times about 2.5 passes. Re-estimating per level
        // keeps the bar moving (one estimate at the start was 16% out).
        progress.expect_more(children.len() as u64);
    }
    Ok(())
}

/// One finished parent, held until the level's keep-count is known: the bytes
/// to store, and the chains for thinning.
struct Parent {
    coord: TileCoord,
    chains: Vec<lyv_core::Chain>,
    encoded: Vec<u8>,
}

/// Merge one level into the next across every core, measuring as it goes.
/// Each parent reads only its four children; the calling thread collects.
#[allow(clippy::too_many_arguments)]
fn merge_level_in_parallel(
    // By value: the grouping moves chains rather than copying the level.
    children: Pieces,
    options: &Options,
    // Not `options.simplification`: the climb raises it when a level is too
    // dense.
    tolerance: lyv_simplify::Tolerance,
    child_extent: u32,
    parent_extent: u32,
    ranks: &std::collections::HashMap<u64, (u32, u32)>,
    workers: usize,
    progress: &Progress,
) -> (Vec<Parent>, lyv_rank::Worst) {
    // Where each parent's tiny polygons are, gathered before merging: a ring
    // small enough to round into a sliver is cancelled by the seam sweep and
    // never reaches `drop_collapsed` (64 features in, 12 out, none dropped).
    progress.region("dust");
    let mut at_risk = at_risk_of_vanishing(&children, child_extent, parent_extent, options);

    progress.region("grouping");
    let mut grouped = lyv_merge::group(children, child_extent, parent_extent, options.dust);
    // `children` is gone from here on, which is the point.
    progress.expect(grouped.parents.len() as u64);

    // Each parent with its dust, so workers share nothing mutable. Taken out
    // of `grouped`, not cloned: cloning held the level twice (9 GB at merge
    // z11 on floodplains).
    type Dust = std::collections::HashMap<u64, geo_types::Coord<Local>>;
    let mut work: Vec<(TileCoord, Vec<lyv_core::Chain>, Dust)> =
        std::mem::take(&mut grouped.parents)
            .into_iter()
            .map(|(coord, chains)| {
                let risked = at_risk.remove(&coord).unwrap_or_default();
                (coord, chains, risked)
            })
            .collect();
    // Parents whose every feature vanished aren't in the merge's output, so
    // they're built here or not at all.
    work.extend(
        std::mem::take(&mut at_risk)
            .into_iter()
            .map(|(coord, risked)| (coord, Vec::new(), risked)),
    );

    progress.region("merging");
    let queue = std::sync::Mutex::new(work.into_iter());
    let (send, receive) = std::sync::mpsc::sync_channel::<(Parent, lyv_rank::Worst)>(workers * 4);

    let mut parents = Vec::new();
    let mut worst = lyv_rank::Worst::default();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let send = send.clone();
            let queue = &queue;
            let grouped = &grouped;
            scope.spawn(move || {
                loop {
                    // A lock per parent is negligible against a merge taking
                    // milliseconds, and balances parents of very different
                    // sizes better than static shares.
                    let Some((coord, chains, risked)) =
                        queue.lock().expect("the parent queue").next()
                    else {
                        return;
                    };
                    let merged = if chains.is_empty() {
                        Vec::new()
                    } else {
                        grouped.merge(chains)
                    };
                    // After merging: former seam vertices are now interior, and
                    // so may be simplified.
                    let simplified =
                        lyv_simplify::simplify_chains(&merged, parent_extent, tolerance);
                    // Simplification collapses rings constantly (a ring keeps
                    // its start point, so can shrink to a two-point stub), and
                    // a ring rounded inside out would show as a hole.
                    let mut chains = drop_collapsed(simplified, parent_extent, options.dust);
                    if !risked.is_empty() {
                        add_dust(&mut chains, risked, parent_extent);
                    }
                    // An empty result is still stored (see `lyv_store`).
                    let encoded =
                        lyv_mvt::encode_chains(&options.output_layer, parent_extent, &chains);
                    let weight = weigh(&chains, &encoded, ranks);
                    progress.item();
                    if send
                        .send((
                            Parent {
                                coord,
                                chains,
                                encoded,
                            },
                            weight,
                        ))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        // Dropped so the loop below ends with the last worker.
        drop(send);

        for (parent, weight) in receive {
            // A maximum, so arrival order can't change it.
            worst = worst.max(weight);
            parents.push(parent);
        }
    });

    // Sorted, so the result doesn't depend on thread completion order.
    parents.sort_unstable_by_key(|parent| parent.coord);
    (parents, worst)
}

/// Add each tile's margin and write it straight into the archive.
///
/// Levels go in ascending zoom, each in Hilbert order — the order PMTiles
/// wants — so no staging is needed; a reorder buffer absorbs the workers'
/// completion order. (Staging through the store cost 46 s of reads on
/// floodplains.)
///
/// Reads each tile's eight neighbours: the one pass that isn't local to a
/// tile. With `buffer == 0`, tiles pass through and no neighbour is read.
pub fn encode(
    store: &mut dyn TileStore,
    options: &Options,
    writer: &mut dyn lyv_archive::TileWriter,
    progress: &Progress,
) -> Result<()> {
    let batch_size = writer.batch_size();
    let batch_bytes = writer.batch_bytes();
    let mut batch: Vec<lyv_archive::FinishedTile> = Vec::new();
    let mut held = 0usize;

    // Once, before the first level. Its own stage, or its seconds get booked
    // to whatever stage is open.
    progress.stage("attributes", None);
    let attributes = timed(Spent::ReadFeatures, 0, || {
        load_attributes(store, options.cpu_workers())
    })?;

    progress.stage("encoding", Some("tiles"));
    let all = store.count()?;
    progress.expect(all);
    progress.phase(lyv_progress::Phase::Encoding, all);
    for z in store.zooms()? {
        let extent = options.extent_at(z);
        let margin = lyv_buffer::margin_units(options.buffer, extent);
        // Addresses first: the neighbour lookups use the same connection, and a
        // nested walk would hold two statements open.
        let mut coords = Vec::new();
        store.for_each_at(z, &mut |coord, _| {
            coords.push(coord);
            Ok(())
        })?;

        let job = Encode {
            layer: &options.output_layer,
            extent,
            margin,
            attributes: &attributes,
        };
        let mut give = |coord: TileCoord, bytes: Vec<u8>| -> Result<()> {
            progress.item();
            if batch_size == 0 {
                return writer.write_tile(coord.z, coord.x, coord.y, &bytes);
            }
            held += bytes.len();
            batch.push(((coord.z, coord.x, coord.y), bytes));
            if batch_is_full(batch.len(), held, batch_size, batch_bytes) {
                timed(Spent::WriteTiles, held, || writer.write_tiles(&batch))?;
                batch.clear();
                held = 0;
            }
            Ok(())
        };

        let workers = options.workers();
        if workers > 1 {
            encode_level_in_parallel(options, &coords, &job, workers, &mut give)?;
        } else {
            let mut recent = Neighbourhood::default();
            for &coord in &coords {
                let bytes = encode_tile(store, coord, &job, &mut recent)?;
                give(coord, bytes)?;
            }
        }
    }
    if !batch.is_empty() {
        timed(Spent::WriteTiles, held, || writer.write_tiles(&batch))?;
    }
    Ok(())
}

/// One decoding worker's share, flat: a `Vec` and `String`s per feature peaked
/// at 7.2 GB on floodplains.
#[derive(Default)]
struct Share {
    /// `(schema index, value)`, every feature's run after the last.
    entries: Vec<(u32, lyv_core::Value)>,
    /// `(feature, start in `entries`, count)`.
    spans: Vec<(u64, u32, u32)>,
}

/// Every feature's attributes, decoded once and held for the encode pass.
///
/// Held because per-tile queries decoded each feature once per tile it touched.
/// Indexed by id, since ids are dense from zero: a `Vec` beats a hash map.
/// Keys aren't stored — entries hold indices into the store's
/// [`Schema`](lyv_store::Schema) — and entries are one flat vector with a span
/// per feature, not an allocation each.
#[derive(Default)]
struct Loaded {
    schema: lyv_store::Schema,
    entries: Vec<(u32, lyv_core::Value)>,
    /// By feature id: start in `entries`, and count. `(0, 0)` for none.
    spans: Vec<(u32, u32)>,
}

impl lyv_core::Attributes for Loaded {
    fn for_each(&self, feature: u64, visit: &mut dyn FnMut(&str, &lyv_core::Value)) {
        let Some(&(start, len)) = self.spans.get(feature as usize) else {
            return;
        };
        for (key, value) in &self.entries[start as usize..start as usize + len as usize] {
            let name = self
                .schema
                .name(*key)
                .expect("a stored index is in the schema");
            visit(name, value);
        }
    }
}

/// Read the whole attribute table into memory, decoding across every core.
///
/// Each worker fills its own exactly-sized flat arena (sized by counting
/// first; guessing left 64% slack), and they're concatenated into one, so the
/// only copy is the final one. The read itself is one serial statement.
fn load_attributes(store: &dyn TileStore, workers: usize) -> Result<Loaded> {
    // Stored rows concatenated into one buffer, not millions of `Vec`s.
    let mut blobs: Vec<u8> = Vec::new();
    let mut at: Vec<(u64, u32, u32)> = Vec::new();
    store.for_each_feature(&mut |id, attrs| {
        let start = u32::try_from(blobs.len()).context("the attribute table is over 4 GB")?;
        blobs.extend_from_slice(attrs);
        at.push((id, start, attrs.len() as u32));
        Ok(())
    })?;
    if at.is_empty() {
        return Ok(Loaded::default());
    }

    let share = at.len().div_ceil(workers.max(1)).max(1);
    let borrowed = &blobs;
    let shares: Vec<Share> = std::thread::scope(|scope| {
        let handles: Vec<_> = at
            .chunks(share)
            .map(|part| {
                scope.spawn(move || {
                    let row = |&(_, start, len): &(u64, u32, u32)| {
                        &borrowed[start as usize..start as usize + len as usize]
                    };
                    let mut share = Share {
                        entries: Vec::with_capacity(
                            part.iter()
                                .map(|at| lyv_store::count_properties(row(at)))
                                .sum(),
                        ),
                        spans: Vec::with_capacity(part.len()),
                    };
                    for entry in part {
                        let began = share.entries.len() as u32;
                        lyv_store::for_each_property(row(entry), &mut |index, value| {
                            share.entries.push((index, value));
                        });
                        let count = share.entries.len() as u32 - began;
                        if count > 0 {
                            share.spans.push((entry.0, began, count));
                        }
                    }
                    share
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("an attribute decoding thread"))
            .collect()
    });
    // Dropped before the merge, or the stored table sits on top of the new one.
    drop(blobs);

    let highest = at.last().map_or(0, |&(id, _, _)| id);
    drop(at);

    let mut loaded = Loaded {
        // Exact, so the arena never doubles: a doubling realloc holds old and
        // new at once.
        entries: Vec::with_capacity(shares.iter().map(|s| s.entries.len()).sum()),
        spans: vec![(0, 0); highest as usize + 1],
        // Workers decoded against the store's schema, so indices already agree.
        schema: store.schema().clone(),
    };
    for share in shares {
        let base = loaded.entries.len() as u32;
        loaded.entries.extend(share.entries);
        for (id, start, count) in share.spans {
            loaded.spans[id as usize] = (base + start, count);
        }
    }
    Ok(loaded)
}

/// What encoding one tile needs to know beyond the tile itself.
struct Encode<'a> {
    layer: &'a str,
    extent: u32,
    margin: Local,
    /// Every feature's attributes, loaded once, shared read-only.
    attributes: &'a Loaded,
}

/// Buffer and encode one tile. Read-only, so many run at once on their own
/// connections.
fn encode_tile(
    store: &dyn TileStore,
    coord: TileCoord,
    job: &Encode,
    recent: &mut Neighbourhood,
) -> Result<Vec<u8>> {
    let own = recent.get(store, coord)?;
    let own = own.as_slice();

    // Only a tile holding a polygon needs a margin: lines aren't buffered, and
    // a polygon absent from this tile contributes only outside it. Saves eight
    // lookups per tile, and costs nothing since the tile is decoded anyway.
    let areal = own.iter().any(|c| lyv_seam::is_ring(&c.points));

    let chains = if job.margin <= 0 || !areal {
        // Copied only here; the buffered path builds its own vector.
        own.to_vec()
    } else {
        // Kept alive so the slices below can borrow from them.
        let mut held = Vec::new();
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                if (dx, dy) == (0, 0) {
                    continue;
                }
                // The address wraps but `(dx, dy)` doesn't: the neighbour is
                // placed one tile away whichever column it was read from.
                let Some(address) = offset(coord, dx, dy) else {
                    continue; // above the top row or below the bottom
                };
                let chains = recent.get(store, address)?;
                if !chains.is_empty() {
                    held.push(((dx, dy), chains));
                }
            }
        }
        let neighbours: Vec<lyv_buffer::Neighbour<'_>> = held
            .iter()
            // Narrowed: the buffer multiplies it by the extent.
            .map(|((dx, dy), chains)| ((*dx as Local, *dy as Local), chains.as_slice()))
            .collect();
        timed(Spent::Margin, 0, || {
            lyv_buffer::add_margin(own, &neighbours, job.extent, job.margin)
        })
    };

    // Attributes are attached here, at the end: the pyramid never carries
    // them.
    Ok(timed(Spent::Mvt, 0, || {
        lyv_mvt::encode_chains_with(job.layer, job.extent, &chains, job.attributes)
    }))
}

/// Encode one level across threads, handing tiles on in order.
///
/// PMTiles deduplicates against the previous tile and marks the archive
/// unclustered when an id goes backwards, so order matters. A reorder buffer
/// holds finished tiles until every earlier one has gone. Workers claim blocks,
/// so at most a block per worker waits.
fn encode_level_in_parallel(
    options: &Options,
    coords: &[TileCoord],
    job: &Encode,
    workers: usize,
    give: &mut dyn FnMut(TileCoord, Vec<u8>) -> Result<()>,
) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Consecutive tiles a worker claims at once. Consecutive in Hilbert order
    /// means neighbours on the ground, sharing most of their neighbours, which
    /// the cache below exploits; striding one at a time would waste it.
    const BLOCK: usize = 64;

    let next = AtomicUsize::new(0);
    // A few tiles per worker: a slow tile doesn't stall the rest, memory stays
    // flat.
    let (send, receive) =
        std::sync::mpsc::sync_channel::<Result<(usize, TileCoord, Vec<u8>)>>(workers * 4);

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            let send = send.clone();
            let next = &next;
            scope.spawn(move || {
                // One connection for the worker's whole share.
                let reader = match lyv_store::SqliteStore::open_mapping(
                    &options.store,
                    options.mmap_bytes,
                ) {
                    Ok(reader) => reader,
                    Err(error) => {
                        let _ = send.send(Err(error));
                        return;
                    }
                };
                // Recently decoded tiles: consecutive tiles share six of their
                // nine reads.
                let mut recent = Neighbourhood::default();
                loop {
                    let first = next.fetch_add(BLOCK, Ordering::Relaxed);
                    if first >= coords.len() {
                        return;
                    }
                    for (offset, &coord) in coords[first..(first + BLOCK).min(coords.len())]
                        .iter()
                        .enumerate()
                    {
                        let result = encode_tile(&reader, coord, job, &mut recent)
                            .map(|bytes| (first + offset, coord, bytes));
                        let failed = result.is_err();
                        // A closed channel means the writer gave up.
                        if send.send(result).is_err() || failed {
                            return;
                        }
                    }
                }
            });
        }
        // Dropped so the loop below ends with the last worker.
        drop(send);

        // Held until their turn; the channel is drained regardless, so workers
        // never block on a tile that isn't next.
        let mut waiting: std::collections::BTreeMap<usize, (TileCoord, Vec<u8>)> =
            Default::default();
        let mut wanted = 0usize;
        for result in receive {
            let (index, coord, bytes) = result?;
            waiting.insert(index, (coord, bytes));
            while let Some((coord, bytes)) = waiting.remove(&wanted) {
                give(coord, bytes)?;
                wanted += 1;
            }
        }
        Ok(())
    })
}

fn read_chains(store: &dyn TileStore, coord: TileCoord) -> Result<Vec<lyv_core::Chain>> {
    let held = timed(Spent::GetTile, 0, || store.get(coord))?;
    match held {
        Some(data) => {
            timings::moved(Spent::GetTile, data.len());
            lyv_mvt::decode_chains(&data)
                .with_context(|| format!("reading tile {}/{}/{}", coord.z, coord.x, coord.y))
        }
        None => Ok(Vec::new()),
    }
}

/// Tiles one worker decoded lately. Buffering a tile reads nine, and
/// consecutive tiles in Hilbert order share six, so most reads repeat.
///
/// First-in eviction, not LRU: the access pattern is a sweep, so insertion
/// order and recency agree.
#[derive(Default)]
struct Neighbourhood {
    /// Shared, since each tile goes to nine callers; cloning it for each was a
    /// deep copy nine times per tile.
    held: std::collections::HashMap<TileCoord, std::rc::Rc<Vec<lyv_core::Chain>>>,
    order: std::collections::VecDeque<TileCoord>,
}

impl Neighbourhood {
    /// A tile's nine plus the block around them; tens of MB across workers.
    const KEEP: usize = 96;

    /// A tile's chains, decoding it only if not held.
    fn get(
        &mut self,
        store: &dyn TileStore,
        coord: TileCoord,
    ) -> Result<std::rc::Rc<Vec<lyv_core::Chain>>> {
        if let Some(held) = self.held.get(&coord) {
            return Ok(held.clone());
        }
        let chains = std::rc::Rc::new(read_chains(store, coord)?);
        self.held.insert(coord, chains.clone());
        self.order.push_back(coord);
        if self.order.len() > Self::KEEP
            && let Some(oldest) = self.order.pop_front()
        {
            self.held.remove(&oldest);
        }
        Ok(chains)
    }
}

/// A neighbouring address, or `None` if there's no such tile.
///
/// Longitude wraps (the tile east of the last column is the first); at zoom 0
/// the single tile is its own neighbour, which is right. Latitude doesn't wrap.
fn offset(coord: TileCoord, dx: i32, dy: i32) -> Option<TileCoord> {
    let span = 1i64 << coord.z;
    let y = i64::from(coord.y) + i64::from(dy);
    (0..span).contains(&y).then_some(())?;
    let x = (i64::from(coord.x) + i64::from(dx)).rem_euclid(span);
    Some(TileCoord {
        z: coord.z,
        x: x as u32,
        y: y as u32,
    })
}

/// Drop rings that are no longer usable:
///
/// - **collapsed** ones, which simplification leaves as two-point stubs (a
///   ring keeps its start point);
/// - **orphan holes**: a small ring that rounding folds inside out reads as a
///   hole, which renderers fill. A hole with no exterior of its feature in the
///   tile is dropped.
///
/// Open lines are left alone however short.
fn drop_collapsed(chains: Vec<lyv_core::Chain>, extent: u32, dust: bool) -> Vec<lyv_core::Chain> {
    use std::collections::HashSet;

    let mut collapsed: Vec<lyv_core::Chain> = Vec::new();
    let kept: Vec<lyv_core::Chain> = chains
        .into_iter()
        .filter(|chain| {
            // A point has nothing to collapse. Checked first, since a
            // one-point chain trivially "closes".
            if chain.points.len() == 1 {
                return true;
            }
            if chain.points.is_empty() {
                return false;
            }
            if chain.points.first() != chain.points.last() {
                return true; // an open line, however short
            }
            let ring =
                lyv_seam::is_ring(&chain.points) && lyv_seam::double_area(&chain.points) != 0;
            if !ring && dust {
                collapsed.push(chain.clone());
            }
            ring
        })
        .collect();

    // Features that still have an exterior here. Rings only: an open chain's
    // shoelace sum is negative half the time, and treating those as holes
    // once deleted lines at random.
    let has_exterior: HashSet<u64> = kept
        .iter()
        .filter(|c| lyv_seam::is_ring(&c.points) && lyv_seam::double_area(&c.points) > 0)
        .map(|c| c.feature)
        .collect();

    let mut out: Vec<lyv_core::Chain> = kept
        .into_iter()
        .filter(|chain| {
            if !lyv_seam::is_ring(&chain.points) {
                return true;
            }
            lyv_seam::double_area(&chain.points) > 0 || has_exterior.contains(&chain.feature)
        })
        .collect();

    // Polygon dust: a ring that shrank to nothing becomes a unit square, but
    // only if the feature has nothing else left here — which keeps the count
    // down, and settles whether a windingless speck was exterior or hole.
    let mut given: HashSet<u64> = has_exterior;
    for chain in collapsed {
        if given.contains(&chain.feature) {
            continue;
        }
        let Some(&at) = chain.points.first() else {
            continue;
        };
        given.insert(chain.feature);
        out.push(lyv_core::Chain {
            feature: chain.feature,
            points: unit_square(at, extent),
        });
    }
    out
}

/// Where each parent's smallest rings sit, in case they vanish in the merge.
/// Only rings small enough to be at risk; tracking every feature would cost as
/// much as the level.
fn at_risk_of_vanishing(
    children: &Pieces,
    child_extent: u32,
    parent_extent: u32,
    options: &Options,
) -> std::collections::BTreeMap<TileCoord, std::collections::HashMap<u64, geo_types::Coord<Local>>>
{
    let mut at_risk: std::collections::BTreeMap<
        TileCoord,
        std::collections::HashMap<u64, geo_types::Coord<Local>>,
    > = Default::default();
    if !options.dust {
        return at_risk;
    }

    // The same scaling the merge does.
    let divisor = i64::from(2 * child_extent) / i64::from(parent_extent);
    for (child, chains) in children.iter() {
        let (qx, qy) = child.quadrant();
        let place = |p: geo_types::Coord<Local>| geo_types::Coord {
            x: ((i64::from(p.x) + i64::from(qx * child_extent)) / divisor) as Local,
            y: ((i64::from(p.y) + i64::from(qy * child_extent)) / divisor) as Local,
        };
        for chain in chains {
            // Rings only.
            if chain.points.len() < 4 || chain.points.first() != chain.points.last() {
                continue;
            }
            let (mut min_x, mut max_x) = (Local::MAX, Local::MIN);
            let (mut min_y, mut max_y) = (Local::MAX, Local::MIN);
            for p in &chain.points {
                min_x = min_x.min(p.x);
                max_x = max_x.max(p.x);
                min_y = min_y.min(p.y);
                max_y = max_y.max(p.y);
            }
            let wide = i64::from(max_x - min_x) / divisor;
            let tall = i64::from(max_y - min_y) / divisor;
            if wide > AT_RISK_UNITS || tall > AT_RISK_UNITS {
                continue;
            }
            at_risk
                .entry(child.parent())
                .or_default()
                .entry(chain.feature)
                .or_insert_with(|| place(chain.points[0]));
        }
    }
    at_risk
}

/// How small a ring must be, in the parent's units, to be at risk. One unit is
/// the most that can round away; two is generous.
const AT_RISK_UNITS: i64 = 2;

/// Add a speck for each feature that is not in `chains` already.
fn add_dust(
    chains: &mut Vec<lyv_core::Chain>,
    risked: std::collections::HashMap<u64, geo_types::Coord<Local>>,
    extent: u32,
) {
    let present: std::collections::HashSet<u64> = chains.iter().map(|c| c.feature).collect();
    let mut missing: Vec<(u64, geo_types::Coord<Local>)> = risked
        .into_iter()
        .filter(|(feature, _)| !present.contains(feature))
        .collect();
    // Sorted, so bytes don't depend on hash order.
    missing.sort_unstable_by_key(|(feature, _)| *feature);
    for (feature, at) in missing {
        chains.push(lyv_core::Chain {
            feature,
            points: unit_square(at, extent),
        });
    }
}

/// The smallest square the grid can hold, at `at`, as an exterior. Placed back
/// from the far edges, so it stays inside the tile.
fn unit_square(at: geo_types::Coord<Local>, extent: u32) -> Vec<geo_types::Coord<Local>> {
    let extent = extent as Local;
    let low = |v: Local| v.clamp(0, extent - 1);
    let (x, y) = (low(at.x), low(at.y));
    // Positive shoelace area: an exterior. The wrong sign would read as a hole.
    [(x, y), (x + 1, y), (x + 1, y + 1), (x, y + 1), (x, y)]
        .into_iter()
        .map(|(x, y)| geo_types::Coord { x, y })
        .collect()
}

/// How big a feature is, for ranking: area, length, or nothing for points,
/// measured on the whole feature before it's cut.
///
/// In `f64`: it only decides an order, and exact integer shoelace sums at
/// maxzoom 22 would overflow `i64`. Coordinates are taken relative to the
/// ring's first point to keep the products small.
fn size_of(feature: &lyv_source::SourceFeature) -> lyv_rank::Size {
    match feature.kind {
        // Points race each other on equal terms.
        lyv_source::Kind::Points => lyv_rank::Size::point(),
        lyv_source::Kind::Rings => {
            // Signed and summed across rings, so a donut is credited with the
            // area it covers.
            let mut double_area = 0.0f64;
            for ring in &feature.parts {
                let Some(origin) = ring.first() else { continue };
                for pair in ring.windows(2) {
                    let (ax, ay) = ((pair[0].x - origin.x) as f64, (pair[0].y - origin.y) as f64);
                    let (bx, by) = ((pair[1].x - origin.x) as f64, (pair[1].y - origin.y) as f64);
                    double_area += ax * by - bx * ay;
                }
            }
            lyv_rank::Size::polygon((double_area / 2.0).abs())
        }
        lyv_source::Kind::Lines => {
            let mut length = 0.0f64;
            for part in &feature.parts {
                for pair in part.windows(2) {
                    let dx = (pair[1].x - pair[0].x) as f64;
                    let dy = (pair[1].y - pair[0].y) as f64;
                    length += dx.hypot(dy);
                }
            }
            lyv_rank::Size::line(length)
        }
    }
}

/// What one tile weighs against the limits: geometry plus attributes, since
/// the stored tile has none and the shipped one will.
fn weigh(
    chains: &[lyv_core::Chain],
    encoded: &[u8],
    ranks: &std::collections::HashMap<u64, (u32, u32)>,
) -> lyv_rank::Worst {
    weigh_by(chains, encoded, &|id| {
        ranks.get(&id).map_or(0, |(_, bytes)| *bytes as usize)
    })
}

/// As [`weigh`], with attribute costs given on their own: ingest has them
/// before there's a ranking.
fn weigh_with(
    chains: &[lyv_core::Chain],
    encoded: &[u8],
    attr_bytes: &std::collections::HashMap<u64, u32>,
) -> lyv_rank::Worst {
    weigh_by(chains, encoded, &|id| {
        attr_bytes.get(&id).copied().unwrap_or(0) as usize
    })
}

/// Record which ranking buckets one feature occupies. Pure, so it runs across
/// every core.
fn bucket_feature(
    feature: &Feature,
    rank_zoom: u32,
    rank_extent: u32,
    layer: &mut lyv_rank::Layer,
) {
    let size = feature.size;
    let mut seen = false;
    for part in &feature.parts {
        // Closed first, or the closing segment's tiles are missed.
        let mut points = part.clone();
        if feature.kind == lyv_source::Kind::Rings
            && points.first() != points.last()
            && let Some(first) = points.first().copied()
        {
            points.push(first);
        }
        lyv_cut::for_each_tile(&points, rank_zoom, rank_extent, &mut |tile| {
            seen = true;
            layer.add(tile, feature.id, size);
        });
    }
    if !seen {
        // Every feature must be ranked, or thinning keeps it unconditionally.
        // A part too short to sweep takes its first vertex's tile.
        if let Some(point) = feature.parts.iter().flatten().next() {
            let side = i64::from(rank_extent);
            layer.add(
                lyv_core::TileCoord {
                    z: rank_zoom,
                    x: point.x.div_euclid(side) as u32,
                    y: point.y.div_euclid(side) as u32,
                },
                feature.id,
                size,
            );
        }
    }
}

/// What one tile weighs, given a way to price attributes. Features are
/// deduplicated first: a feature in several chains carries its attributes once.
fn weigh_by(
    chains: &[lyv_core::Chain],
    encoded: &[u8],
    cost: &dyn Fn(u64) -> usize,
) -> lyv_rank::Worst {
    let features: std::collections::HashSet<u64> =
        chains.iter().map(|chain| chain.feature).collect();
    let attributes: usize = features.iter().map(|id| cost(*id)).sum();
    lyv_rank::Worst {
        bytes: encoded.len() + attributes,
        features: features.len(),
    }
}

/// Every feature's rank and attribute cost, held for the whole climb.
fn load_ranks(store: &dyn TileStore) -> Result<std::collections::HashMap<u64, (u32, u32)>> {
    let mut ranks = std::collections::HashMap::new();
    // Counts nothing: it's quick, and counting it spent a stage's share of the
    // bar on almost no time.
    timed(Spent::Ranks, 0, || {
        store.for_each_rank(&mut |id, rank, attr_bytes| {
            ranks.insert(id, (rank, attr_bytes));
            Ok(())
        })
    })?;
    timings::moved(Spent::Ranks, ranks.len() * 16);
    Ok(ranks)
}

/// Draw the ranking (bucketed during the read) and store it.
fn store_ranks(
    store: &mut dyn TileStore,
    options: &Options,
    layer: lyv_rank::Layer,
    facts: &Facts,
    progress: &Progress,
) -> Result<()> {
    let ranker = lyv_rank::by_name(&options.ranker)
        .with_context(|| format!("unknown ranker {:?}", options.ranker))?;

    progress.stage("ranking", Some("features"));
    progress.expect(layer.features() as u64);
    let tiles = layer.tiles();
    let ranks = ranker.rank(&layer);
    // Sorted: inserts walk the primary key, and the stored ranking is identical
    // between runs.
    let mut listed: Vec<(u64, u32, u32)> = ranks
        .into_iter()
        .map(|(id, rank)| {
            // The only counting in this stage; without it the bar sits still.
            progress.item();
            (id, rank, facts.attr_bytes.get(&id).copied().unwrap_or(0))
        })
        .collect();
    listed.sort_unstable();
    timed(Spent::Ranks, listed.len() * 16, || store.put_ranks(&listed))?;

    progress.say(&format!(
        "  ranked {} features over {tiles} buckets by {}",
        listed.len(),
        ranker.name()
    ));
    Ok(())
}

/// Which of TileJSON's three field types an attribute value is.
fn field_type(value: &lyv_core::Value) -> lyv_archive::FieldType {
    use lyv_archive::FieldType;
    use lyv_core::Value;
    match value {
        Value::Str(_) => FieldType::String,
        Value::Bool(_) => FieldType::Boolean,
        Value::Float(_) | Value::Double(_) | Value::Int(_) | Value::Uint(_) | Value::Sint(_) => {
            FieldType::Number
        }
    }
}

/// What the archive should declare as its extent and opening view: bounds,
/// the antimeridian-adjusted bounds when they differ, and the centre.
///
/// All `None` when the input held no geometry, in which case the writers keep
/// their defaults rather than inventing a location.
struct Declared {
    bounds: Option<[f64; 4]>,
    adjusted: Option<[f64; 4]>,
    center: Option<(f64, f64, u32)>,
}

/// `bounds` has its eastern longitude past 180 when the data crosses the
/// antimeridian. Bounds may not do that (TileJSON), and MapLibre clamps them to
/// ±180 and requests no tile outside, so they claim every longitude and the
/// real extent goes in the adjusted bounds.
fn declared_extent(bounds: Option<[f64; 4]>, options: &Options) -> Declared {
    let Some([min_lon, min_lat, max_lon, max_lat]) = bounds else {
        return Declared {
            bounds: None,
            adjusted: None,
            center: None,
        };
    };

    // A zoom where the extent is about one tile wide, so the opening view shows
    // the data.
    let width = (max_lon - min_lon).max(1e-9);
    let fitting = (360.0 / width).log2().floor().max(0.0) as u32;
    let zoom = fitting.clamp(options.min_zoom, options.deepest());
    let center = Some((
        wrap_longitude((min_lon + max_lon) / 2.0),
        (min_lat + max_lat) / 2.0,
        zoom,
    ));

    if max_lon > 180.0 {
        Declared {
            bounds: Some([-180.0, min_lat, 180.0, max_lat]),
            adjusted: bounds,
            center,
        }
    } else {
        Declared {
            bounds,
            adjusted: None,
            center,
        }
    }
}

/// A longitude brought into `-180..180`, as the cut wraps it.
fn wrap_longitude(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

/// Whether closing this feature's pieces would enclose something other than
/// the feature.
///
/// Cutting and closing conserve area: closing's artificial edges along a
/// boundary are traversed opposite ways by the two tiles sharing them, so they
/// cancel, and a ring's pieces sum to the ring's area. Swallowed tiles add
/// their squares, as the fill will.
///
/// A self-intersecting ring can leave a piece whose ends sit on the perimeter
/// in a hole's order while the ring is an exterior; closing then returns the
/// tile's complement. `orient` can't catch it (it checks net area), and finding
/// self-intersections would cost a sweep over every ring.
///
/// The threshold is half a tile: the failure misplaces a whole tile, while
/// legitimate slack is a unit or two.
fn closing_would_lose_it(
    feature: &Feature,
    alone: &Pieces,
    covered: &[(TileCoord, u64)],
    extent: u32,
) -> bool {
    let tile = 2 * i128::from(extent) * i128::from(extent);

    let mut had = 0i128;
    for part in &feature.parts {
        had += part
            .windows(2)
            .map(|w| {
                i128::from(w[0].x) * i128::from(w[1].y) - i128::from(w[1].x) * i128::from(w[0].y)
            })
            .sum::<i128>();
    }

    let mut closes_to = covered.len() as i128 * tile;
    for chains in alone.values() {
        closes_to += lyv_close::closed_area_within(chains, 0, i64::from(extent));
    }

    (closes_to - had).abs() > tile / 2
}

/// This crate's name and version, recorded as the tileset's generator. From
/// `CARGO_PKG_VERSION`, the same as `lyv --version`.
pub fn generator() -> String {
    format!("lyv {}", env!("CARGO_PKG_VERSION"))
}

/// Create the archive the encode pass writes into. Everything the header needs
/// (fields, extent, zoom range) is known by now, so tiles go straight in.
fn open_archive(
    fields: Vec<(String, lyv_archive::FieldType)>,
    bounds: Option<[f64; 4]>,
    options: &Options,
) -> Result<Box<dyn lyv_archive::TileWriter>> {
    let declared = declared_extent(bounds, options);
    let metadata = lyv_archive::Metadata {
        name: options.tileset_name()?,
        generator: Some(generator()),
        min_zoom: options.min_zoom,
        max_zoom: options.deepest(),
        bounds: declared.bounds,
        antimeridian_adjusted_bounds: declared.adjusted,
        center: declared.center,
        layers: vec![lyv_archive::LayerInfo {
            id: options.output_layer.clone(),
            min_zoom: options.min_zoom,
            max_zoom: options.deepest(),
            fields,
        }],
    };
    lyv_archive::create(&options.output, &metadata)
}

/// Whether a batch of tiles is ready for the writer. Either limit closes it;
/// zero means no limit. The count is work per worker; the bytes bound memory,
/// since tile sizes vary a thousandfold.
pub fn batch_is_full(tiles: usize, bytes: usize, max_tiles: usize, max_bytes: usize) -> bool {
    (max_tiles > 0 && tiles >= max_tiles) || (max_bytes > 0 && bytes >= max_bytes)
}

/// The whole job, silently.
pub fn run(options: &Options) -> Result<()> {
    // Silent: a library shouldn't write to stderr unasked. The binary calls
    // `run_with`.
    run_with(options, &Progress::silent())
}

/// Build the pyramid, handing every finished tile to `writer`, which the
/// caller finishes. How tests see finished tiles without parsing a PMTiles
/// file.
pub fn run_into(
    options: &Options,
    writer: &mut dyn lyv_archive::TileWriter,
    progress: &Progress,
) -> Result<()> {
    build(options, Some(writer), progress)
}

/// Every finished tile, kept in memory. For tests.
#[derive(Default)]
pub struct Collected {
    pub tiles: std::collections::BTreeMap<TileCoord, Vec<u8>>,
}

impl Collected {
    /// Run the pipeline and keep every finished tile.
    pub fn from(options: &Options) -> Result<Self> {
        let mut collected = Self::default();
        run_into(options, &mut collected, &Progress::silent())?;
        Ok(collected)
    }

    /// One tile, or `None` if the pyramid does not hold it.
    pub fn get(&self, coord: TileCoord) -> Option<&Vec<u8>> {
        self.tiles.get(&coord)
    }

    /// Every tile at one zoom, in Hilbert order.
    pub fn at(&self, z: u32) -> impl Iterator<Item = (TileCoord, &Vec<u8>)> {
        self.tiles
            .iter()
            .filter(move |(coord, _)| coord.z == z)
            .map(|(coord, data)| (*coord, data))
    }
}

impl lyv_archive::TileWriter for Collected {
    fn write_tile(&mut self, z: u32, x: u32, y: u32, mvt: &[u8]) -> Result<()> {
        self.tiles.insert(TileCoord { z, x, y }, mvt.to_vec());
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

/// The whole job, reporting to the given sink.
pub fn run_with(options: &Options, progress: &Progress) -> Result<()> {
    build(options, None, progress)
}

/// The pipeline, into `writer` if given, else into the archive `options` names.
fn build(
    options: &Options,
    writer: Option<&mut dyn lyv_archive::TileWriter>,
    progress: &Progress,
) -> Result<()> {
    options.validate()?;

    let mut store = open_fresh_store(options)?;

    // `ingest` may choose the maxzoom; later stages get the settled options.
    let ingested = ingest(&mut store, options, progress)?;
    let options = &options.with_max_zoom(ingested.max_zoom);
    merge_up(
        &mut store,
        options,
        ingested.worst,
        ingested.level,
        progress,
    )?;
    match writer {
        Some(writer) => encode(&mut store, options, writer, progress)?,
        None => {
            // Opened before encoding, which writes straight into it. A failed
            // run leaves a partial archive.
            let mut writer = open_archive(ingested.fields, ingested.bounds, options)?;
            encode(&mut store, options, writer.as_mut(), progress)?;
            progress.stage("closing", None);
            writer.finish()?;
        }
    }

    // Only after success: a failed job's store is kept.
    if options.keep_store {
        // Here, since `Drop` can't report a failed commit.
        store.flush()?;
    } else {
        drop(store);
        remove_store(&options.store)?;
    }
    Ok(())
}

/// Open the store empty. One left by `--keep-store` or a failed run holds
/// another job's tiles, and every stored tile gets encoded.
fn open_fresh_store(options: &Options) -> Result<lyv_store::SqliteStore> {
    remove_store(&options.store)?;
    lyv_store::SqliteStore::open_mapping(&options.store, options.mmap_bytes)
}

/// Delete the store and its `-wal`/`-shm` files. The connection must be
/// closed; missing files are fine.
fn remove_store(store: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = store.as_os_str().to_owned();
        name.push(suffix);
        let path = PathBuf::from(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("removing {}", path.display()));
            }
        }
    }
    Ok(())
}

/// The directory a file will be created in: `.` for a bare name (where
/// `Path::parent` gives `""`), and `/` for `/`.
fn containing(file: &Path) -> &Path {
    match file.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => file,
    }
}

/// Where the intermediate store goes if the caller does not say.
pub fn default_store_path(output: &Path) -> PathBuf {
    let mut name = output.as_os_str().to_owned();
    name.push(".lyv-store");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(z: u32, x: u32, y: u32) -> TileCoord {
        TileCoord { z, x, y }
    }

    #[test]
    fn a_bare_filename_is_created_in_the_working_directory() {
        // `parent` would give `""`, rejecting every relative path.
        assert_eq!(containing(Path::new("out.pmtiles")), Path::new("."));
        assert_eq!(
            containing(Path::new("tiles/out.pmtiles")),
            Path::new("tiles")
        );
        assert_eq!(
            containing(Path::new("/var/tmp/out.pmtiles")),
            Path::new("/var/tmp")
        );
        // A path with no file name is already its own directory.
        assert_eq!(containing(Path::new("/")), Path::new("/"));
    }

    /// Options good enough to validate, pointing at a real input file.
    fn validatable(input: &Path, output: PathBuf, store: PathBuf) -> Options {
        Options {
            input: input.to_path_buf(),
            output,
            store,
            min_zoom: 0,
            max_zoom: Some(4),
            min_auto_zoom: 12,
            max_auto_zoom: 16,
            output_layer: "l".to_string(),
            output_name: None,
            attributes: None,
            extent: 4096,
            maxzoom_extent: None,
            ranker: "evenstar".to_string(),
            rank_depth: 4,
            dust: true,
            threads: 1,
            keep_store: false,
            mmap_bytes: lyv_store::DEFAULT_MMAP_BYTES,
            limits: Limits {
                bytes: 0,
                features: 0,
            },
            max_tile_vertices: 0,
            buffer: 0,
            simplification: lyv_simplify::Tolerance::NONE,
        }
    }

    #[test]
    fn a_missing_output_directory_is_caught_before_any_work() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let input = dir.path().join("in.geojson");
        std::fs::write(&input, "{}").expect("the input should write");

        let missing = dir.path().join("nowhere");
        let output = missing.join("out.pmtiles");
        let options = validatable(&input, output.clone(), default_store_path(&output));

        let error = options
            .validate()
            .expect_err("a missing directory is an error");
        let said = error.to_string();
        assert!(
            said.contains("output directory") && said.contains("nowhere"),
            "unhelpful message: {said}"
        );

        // Passes once the directory exists; the file needn't.
        std::fs::create_dir(&missing).expect("the dir should create");
        options.validate().expect("an existing directory is fine");
    }

    #[test]
    fn a_store_sent_somewhere_missing_is_named_separately() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let input = dir.path().join("in.geojson");
        std::fs::write(&input, "{}").expect("the input should write");

        // Only `--store` is wrong, so the message must say so.
        let options = validatable(
            &input,
            dir.path().join("out.pmtiles"),
            dir.path().join("elsewhere").join("s.lyv-store"),
        );
        let said = options
            .validate()
            .expect_err("a missing store directory is an error")
            .to_string();
        assert!(
            said.contains("store directory") && said.contains("elsewhere"),
            "unhelpful message: {said}"
        );
    }

    #[test]
    fn a_neighbour_in_the_middle_of_the_world_is_just_an_addition() {
        assert_eq!(offset(at(4, 8, 8), 1, 0), Some(at(4, 9, 8)));
        assert_eq!(offset(at(4, 8, 8), -1, 1), Some(at(4, 7, 9)));
    }

    #[test]
    fn the_column_east_of_the_last_one_is_the_first() {
        assert_eq!(offset(at(4, 15, 8), 1, 0), Some(at(4, 0, 8)));
        assert_eq!(offset(at(4, 0, 8), -1, 0), Some(at(4, 15, 8)));
        // Diagonally too, or a margin corner stays empty.
        assert_eq!(offset(at(4, 15, 8), 1, -1), Some(at(4, 0, 7)));
    }

    #[test]
    fn at_zoom_zero_the_only_tile_is_its_own_neighbour() {
        // The world is one tile wide at zoom 0.
        assert_eq!(offset(at(0, 0, 0), 1, 0), Some(at(0, 0, 0)));
        assert_eq!(offset(at(0, 0, 0), -1, 0), Some(at(0, 0, 0)));
    }

    #[test]
    fn there_is_no_row_beyond_the_poles() {
        assert_eq!(offset(at(4, 8, 0), 0, -1), None);
        assert_eq!(offset(at(4, 8, 15), 0, 1), None);
        assert_eq!(offset(at(0, 0, 0), 0, 1), None);
        // Latitude rejects even where longitude wrapped.
        assert_eq!(offset(at(4, 15, 0), 1, -1), None);
    }

    #[test]
    fn every_neighbour_of_every_tile_at_a_small_zoom_is_addressable() {
        // All eight neighbours are real except off the top or bottom row.
        let span = 1u32 << 3;
        for y in 0..span {
            for x in 0..span {
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let expected_row =
                            (0..span as i64).contains(&(i64::from(y) + i64::from(dy)));
                        let found = offset(at(3, x, y), dx, dy);
                        assert_eq!(found.is_some(), expected_row, "{x},{y} + {dx},{dy}");
                        if let Some(address) = found {
                            assert!(address.x < span, "column {} is off the world", address.x);
                            assert!(address.y < span, "row {} is off the world", address.y);
                        }
                    }
                }
            }
        }
    }

    /// A level of `count` tiles, all `light` vertices except one of `heavy`.
    fn level(count: usize, light: usize, heavy: usize) -> Density {
        Density::of((0..count).map(|i| if i == 0 { heavy } else { light }))
    }

    #[test]
    fn one_heavy_tile_among_many_is_tolerated_until_it_is_twice_over() {
        // A few tiles over budget is normal; don't coarsen a level for them.
        assert!(!level(100, 10_000, 300_000).over(200_000));
        assert!(level(100, 10_000, 500_000).over(200_000));
    }

    #[test]
    fn a_uniformly_dense_level_is_caught_with_no_tile_extreme() {
        // Nothing twice over, but the level as a whole is too heavy.
        let dense = Density::of(std::iter::repeat_n(250_000, 100));
        assert!(dense.over(200_000));
        assert_eq!(dense.worst, 250_000);
    }

    /// A level where `dense` tiles in a hundred are over budget and the rest
    /// nearly empty, as real levels are.
    fn dense_fraction(dense: usize) -> Vec<usize> {
        (0..100)
            .map(|i| if i < dense { 250_000 } else { 2_000 })
            .collect()
    }

    #[test]
    fn a_long_tail_of_empty_tiles_does_not_hide_a_dense_middle() {
        // Why p90, not a mean: fifteen dense tiles among eighty-five near-empty
        // ones average a fifth of the budget.
        let counts = dense_fraction(15);
        assert_eq!(counts.iter().sum::<usize>() / counts.len(), 39_200);
        assert!(Density::of(counts.into_iter()).over(200_000));
    }

    #[test]
    fn p90_needs_more_than_a_tenth_of_the_level_over_budget() {
        // At exactly a tenth, p90 is the last tile under and nothing fires;
        // such a level is caught by the per-tile trigger or not at all.
        assert!(!Density::of(dense_fraction(10).into_iter()).over(200_000));
        assert!(Density::of(dense_fraction(11).into_iter()).over(200_000));
    }

    #[test]
    fn the_budget_judges_a_tile_as_it_will_ship_not_as_it_was_merged() {
        // A tile of 389k as merged, 412k as shipped, against a 400k trigger:
        // measured unbuffered, z7 was left uncorrected.
        let merged = level(600, 30_000, 389_000);
        assert!(!merged.over(200_000), "the raw measurement is under");
        assert!(
            merged.buffered(buffer_growth(64)).over(200_000),
            "and the tile that ships is over"
        );
    }

    #[test]
    fn no_buffer_means_nothing_to_allow_for() {
        let merged = level(600, 30_000, 389_000);
        assert_eq!(merged.buffered(buffer_growth(0)), merged);
    }

    #[test]
    fn a_zero_budget_is_no_budget() {
        assert!(!level(100, 10_000_000, 10_000_000).over(0));
    }

    #[test]
    fn a_level_with_no_tiles_is_not_over_budget() {
        assert!(!Density::of(std::iter::empty()).over(200_000));
    }
}
