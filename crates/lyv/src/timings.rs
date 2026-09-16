//! Where time goes inside a stage, split between the store and the work, for
//! `--trace`. See `docs/speed.md`.
//!
//! Figures are summed thread-time, not wall clock: encoding's workers add up
//! to far more than the stage took, so compare shares.
//!
//! Off unless `--trace`; on, it costs two `Instant::now()` calls and two
//! atomic adds per measured call, about 0.2% of the busiest stage.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// One thing worth timing separately. Coarse on purpose: store versus work.
/// Finer questions are for a profiler.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Spent {
    /// `reading`: inside the source — parsing, projecting, reading attributes.
    /// Serial.
    SourceParse,
    /// `reading`: one attribute row per feature.
    PutFeature,
    /// `reading`: cutting a batch across every core, on its own thread,
    /// overlapping the parse.
    Cutting,
    /// `reading`: the reader blocked handing a batch to the cutter. Near zero
    /// means the parse is the bottleneck.
    WaitingToCut,
    /// `store z12` and `write zN`: one tile each.
    PutTile,
    /// `encoding`: a tile and its neighbours, less what the worker's cache
    /// held.
    GetTile,
    /// `encoding`: loading the whole attribute table, once.
    ReadFeatures,
    /// `encoding`: welding each tile's margin. Pure CPU.
    Margin,
    /// `encoding`: turning chains into MVT. Pure CPU.
    Mvt,
    /// `encoding`: gzip and the archive append. Pure CPU, in the writer.
    WriteTiles,
    /// `ranking` and `ranks`: storing the ranking and loading it back.
    Ranks,
}

impl Spent {
    const ALL: [Self; 11] = [
        Self::SourceParse,
        Self::PutFeature,
        Self::Cutting,
        Self::WaitingToCut,
        Self::PutTile,
        Self::GetTile,
        Self::ReadFeatures,
        Self::Margin,
        Self::Mvt,
        Self::WriteTiles,
        Self::Ranks,
    ];

    fn slot(self) -> usize {
        match self {
            Self::SourceParse => 0,
            Self::PutFeature => 1,
            Self::Cutting => 2,
            Self::WaitingToCut => 3,
            Self::PutTile => 4,
            Self::GetTile => 5,
            Self::ReadFeatures => 6,
            Self::Margin => 7,
            Self::Mvt => 8,
            Self::WriteTiles => 9,
            Self::Ranks => 10,
        }
    }

    fn stage(self) -> &'static str {
        match self {
            Self::SourceParse | Self::PutFeature | Self::Cutting | Self::WaitingToCut => "reading",
            Self::PutTile => "ingest + climb",
            Self::GetTile | Self::ReadFeatures | Self::Margin | Self::Mvt | Self::WriteTiles => {
                "encoding"
            }
            Self::Ranks => "ranking",
        }
    }

    fn what(self) -> &'static str {
        match self {
            Self::SourceParse => "cpu: parse + project",
            Self::PutFeature => "store: put_feature",
            Self::Cutting => "cpu: cut (parallel)",
            Self::WaitingToCut => "wait: reader on cutter",
            Self::PutTile => "store: put tile",
            Self::GetTile => "store: get tile",
            Self::ReadFeatures => "store: load attributes",
            Self::Margin => "cpu: add margin",
            Self::Mvt => "cpu: encode mvt",
            Self::WriteTiles => "cpu: gzip + archive",
            Self::Ranks => "store: ranks in/out",
        }
    }

    /// Whether this is time inside the store.
    fn is_store(self) -> bool {
        !matches!(
            self,
            Self::Margin
                | Self::Mvt
                | Self::WriteTiles
                | Self::SourceParse
                | Self::Cutting
                | Self::WaitingToCut
        )
    }
}

const SLOTS: usize = Spent::ALL.len();

static ON: AtomicBool = AtomicBool::new(false);
#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
static NANOS: [AtomicU64; SLOTS] = [ZERO; SLOTS];
static CALLS: [AtomicU64; SLOTS] = [ZERO; SLOTS];
static BYTES: [AtomicU64; SLOTS] = [ZERO; SLOTS];

/// Start collecting (`--trace`). Until then `timed` is one relaxed load.
pub fn enable() {
    ON.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Run `work`, adding its time to `what`. `bytes` is what it moved, for a
/// throughput, or zero.
pub fn timed<T>(what: Spent, bytes: usize, work: impl FnOnce() -> T) -> T {
    if !enabled() {
        return work();
    }
    let started = Instant::now();
    let out = work();
    let slot = what.slot();
    NANOS[slot].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    CALLS[slot].fetch_add(1, Ordering::Relaxed);
    if bytes > 0 {
        BYTES[slot].fetch_add(bytes as u64, Ordering::Relaxed);
    }
    out
}

/// Add a duration measured another way, such as the gap between callbacks,
/// which is how time inside `lyv_source` is attributed.
pub fn add(what: Spent, took: std::time::Duration) {
    if enabled() {
        let slot = what.slot();
        NANOS[slot].fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
        CALLS[slot].fetch_add(1, Ordering::Relaxed);
    }
}

/// Add bytes moved, for a call whose size is known only after it returns.
pub fn moved(what: Spent, bytes: usize) {
    if enabled() {
        BYTES[what.slot()].fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// What was measured, as lines to print; empty if nothing was.
pub fn report() -> Vec<String> {
    if !enabled() {
        return Vec::new();
    }
    let seconds = |slot: usize| NANOS[slot].load(Ordering::Relaxed) as f64 / 1e9;
    let total: f64 = (0..SLOTS).map(seconds).sum();
    if total <= 0.0 {
        return Vec::new();
    }

    let mut lines = vec![
        "  timings — summed thread-time, so the parallel stages add up to more".to_string(),
        "  than they took. Shares within a stage are the point.".to_string(),
    ];
    let mut store = 0.0;
    for what in Spent::ALL {
        let slot = what.slot();
        let calls = CALLS[slot].load(Ordering::Relaxed);
        if calls == 0 {
            continue;
        }
        let took = seconds(slot);
        if what.is_store() {
            store += took;
        }
        let moved = BYTES[slot].load(Ordering::Relaxed);
        let rate = if moved > 0 && took > 0.0 {
            format!(
                "{:>9.0} MB {:>6.0} MB/s",
                moved as f64 / 1e6,
                moved as f64 / 1e6 / took
            )
        } else {
            String::new()
        };
        lines.push(
            format!(
                "  {:<15} {:<22} {:>11} calls {:>8.1}s {:>5.1}% {}",
                what.stage(),
                what.what(),
                thousands(calls),
                took,
                100.0 * took / total,
                rate
            )
            .trim_end()
            .to_string(),
        );
    }
    lines.push(format!(
        "  {:<15} {:<22} {:>26.1}s {:>5.1}%",
        "all",
        "in the store",
        store,
        100.0 * store / total
    ));
    lines
}

/// Shared with `footprint`, so the two punctuate numbers alike.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_collected_until_it_is_asked_for() {
        // Off by default: `timed` touches no counter.
        assert!(!enabled());
        assert_eq!(timed(Spent::GetTile, 99, || 7), 7);
        assert!(report().is_empty(), "a disabled report says nothing");
    }

    #[test]
    fn a_count_reads_with_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(3_941_426), "3,941,426");
    }

    #[test]
    fn every_kind_has_its_own_slot() {
        // A duplicated slot would silently add two measurements together.
        let mut seen: Vec<usize> = Spent::ALL.iter().map(|s| s.slot()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), SLOTS, "two kinds share a slot");
    }
}
