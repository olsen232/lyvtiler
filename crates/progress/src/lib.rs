//! Progress lines on stderr during a run.
//!
//! Its own crate because the slow stages live in other crates, and they report
//! from where the work is: reporting only per zoom level left runs looking hung
//! for seconds at a time. Depends on nothing.
//!
//! Two kinds of line: a finished stage gets a line that stays; progress within
//! a stage overwrites the line in place. Every stage shows "N of M" where it
//! can.
//!
//! Redrawn when something changes, never on a timer alone, and at most twice a
//! second (see `REDRAW`). Output is the same whether or not stderr is a
//! terminal (see [`Verbosity`]).

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The shortest gap between redraws. Twice a second: faster, a burst of small
/// tiles blurs the counter and a slow one freezes it, which reads as stutter.
/// In a log, every redraw is a line.
const REDRAW: Duration = Duration::from_millis(500);

/// [`REDRAW`] in nanoseconds, for comparing without a lock.
const REDRAW_NANOS: u64 = REDRAW.as_nanos() as u64;

/// The four parts of a run, by share of the time: reading 35%, ingest 10%,
/// climb 25%, encoding 30%, measured on floodplains at the default worker count
/// and rounded to 5% — the aim is a bar that moves at roughly the right speed.
/// Re-measure after a change that lands mostly in one phase; `--threads` tilts
/// it towards reading.
///
/// A phase reports progress within its own band, so a wrong estimate moves the
/// bar only within that band.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Reading the source, cutting and bucketing as it goes.
    Reading,
    /// Closing rings, filling coverage, drawing the ranking, storing maxzoom.
    Ingest,
    /// Merging, thinning and writing every level above maxzoom.
    Climb,
    /// Buffering and encoding each tile into the archive.
    Encoding,
}

impl Phase {
    /// Where this phase starts and ends, as fractions of the run.
    fn band(self) -> (f64, f64) {
        match self {
            Phase::Reading => (0.00, 0.35),
            Phase::Ingest => (0.35, 0.45),
            Phase::Climb => (0.45, 0.70),
            Phase::Encoding => (0.70, 1.00),
        }
    }
}

/// Reports what the program is doing.
///
/// Shared via an `Arc` with a ticker thread, so the clock on the line keeps
/// moving through a stage that counts nothing for a minute; otherwise it looks
/// hung (a single `count(*)` once froze the line for 52 seconds).
pub struct Progress {
    inner: std::sync::Arc<Shared>,
    /// Joined on drop, so nothing draws over the closing lines.
    ticker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.inner.running.store(false, Ordering::Relaxed);
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
    }
}

/// Where a memory figure comes from, if the binary has one: the counting
/// allocator, set by the binary when built with `memory-stats`. This crate
/// can't depend on it, or every build would pay for counting.
static MEMORY: std::sync::OnceLock<fn() -> usize> = std::sync::OnceLock::new();

/// Show a memory figure on every progress line. Only the first call counts.
pub fn show_memory(reader: fn() -> usize) {
    let _ = MEMORY.set(reader);
}

/// Bytes live right now, or `None` if nothing is counting. A settled stage line
/// shows the stage's peak instead, so peaks can't be subtracted to find what a
/// stage added.
pub fn live() -> Option<usize> {
    MEMORY.get().map(|read| read())
}

/// How much progress is written. Not decided by whether stderr is a terminal:
/// that made `lyv | tee log` silently record nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Verbosity {
    /// A line redrawn in place as the work goes, and everything `Quiet` says.
    #[default]
    Normal,
    /// One settled line per stage, plus messages and the closing accounting;
    /// nothing redrawn, so a log gets one line per stage.
    Quiet,
    /// Nothing at all.
    Silent,
}

struct Shared {
    verbosity: Verbosity,
    /// Show the parts of each stage and the accounting at the end.
    tracing: bool,
    /// Cleared to stop the ticker.
    running: std::sync::atomic::AtomicBool,
    /// The most memory seen this stage, in bytes. Sampled at each redraw, so a
    /// spike between samples is missed; the end-of-run figure is exact.
    stage_peak: AtomicU64,
    /// How many of this stage's unit are done.
    count: AtomicU64,
    /// How many are expected, when that is known. Zero means unknown.
    total: AtomicU64,
    /// When this `Progress` was made, as the zero for `drawn_at`.
    epoch: Instant,
    /// Nanoseconds since `epoch` at the last redraw.
    ///
    /// Outside the mutex, so [`Progress::item`] — called per feature from every
    /// core — can see it has nothing to do without locking. Two threads can
    /// occasionally both draw; harmless.
    drawn_at: AtomicU64,
    /// Which phase is running, as a `Phase` discriminant.
    phase: AtomicU64,
    /// How much of the phase is done, and how much it expects.
    phase_done: AtomicU64,
    phase_total: AtomicU64,
    /// The highest percentage shown, in hundredths. The bar stalls rather than
    /// going backwards (the probe can read the source twice).
    reached: AtomicU64,
    line: Mutex<Line>,
    ledger: Mutex<Ledger>,
}

/// What the stages add up to, against the clock.
///
/// Catches time spent outside any stage directly, and time inside a stage but
/// before its work by subtraction — a `count(*)` before the first tile once
/// made a stage look twice as slow as its work.
#[derive(Default)]
struct Ledger {
    /// Summed over every ended stage. Stages run back to back, so the rest is
    /// startup and teardown.
    inside: Duration,
}

struct Line {
    label: String,
    /// The named part of the stage, when tracing. Empty otherwise.
    region: String,
    /// Whether any region was named in this stage; if so, it gets a summary
    /// line at the end.
    divided: bool,
    /// Whether this line has already been printed in its final form.
    settled: bool,
    /// When the stage began. Differs from the line's start once regions divide
    /// it.
    stage_started: Instant,
    unit: String,
    detail: String,
    started: Instant,
    /// Width of the last line drawn, so the next can clear it.
    width: usize,
}

impl Progress {
    /// Reports to stderr (see [`Verbosity`]).
    pub fn to_stderr(verbosity: Verbosity, tracing: bool) -> Self {
        Self::new(verbosity, tracing)
    }
    /// Says nothing. For tests, and for `--silent`.
    pub fn silent() -> Self {
        Self::new(Verbosity::Silent, false)
    }
    /// Stop drawing, so the closing lines are not overwritten.
    pub fn done(&self) {
        // Closed first, so the last stage is recorded like the others.
        self.inner.close();
        self.inner.running.store(false, Ordering::Relaxed);
    }
    fn new(verbosity: Verbosity, tracing: bool) -> Self {
        // Only the redrawing mode has anything for a timer to do.
        let drawing = verbosity == Verbosity::Normal;
        let inner = std::sync::Arc::new(Shared {
            verbosity,
            tracing,
            running: std::sync::atomic::AtomicBool::new(drawing),
            stage_peak: AtomicU64::new(0),
            count: AtomicU64::new(0),
            total: AtomicU64::new(0),
            epoch: Instant::now(),
            drawn_at: AtomicU64::new(0),
            phase: AtomicU64::new(0),
            phase_done: AtomicU64::new(0),
            phase_total: AtomicU64::new(0),
            reached: AtomicU64::new(0),
            line: Mutex::new(Line {
                label: String::new(),
                region: String::new(),
                divided: false,
                settled: true,
                stage_started: Instant::now(),
                unit: String::new(),
                detail: String::new(),
                started: Instant::now(),
                width: 0,
            }),
            ledger: Mutex::new(Ledger::default()),
        });
        // Only spawned when redrawing, so tests and `--quiet` get no extra
        // thread.
        let ticker = drawing.then(|| {
            let inner = std::sync::Arc::clone(&inner);
            std::thread::spawn(move || {
                while inner.running.load(Ordering::Relaxed) {
                    // Half the interval, so a deadline is never missed by a
                    // whole one.
                    std::thread::sleep(REDRAW / 2);
                    inner.render(false, false);
                }
            })
        });
        Self { inner, ticker }
    }

    /// Begin a stage, ending whatever was running. `unit` names what it counts
    /// ("tiles"), or `None` to show no count.
    pub fn stage(&self, label: &str, unit: Option<&str>) {
        self.inner.stage(label, unit);
    }

    /// How many items this stage expects, when that is known up front.
    pub fn expect(&self, total: u64) {
        self.inner.expect(total);
    }

    /// One more of whatever this stage counts.
    pub fn item(&self) {
        self.inner.item();
    }

    /// Say what is being worked on now.
    pub fn detail(&self, detail: &str) {
        self.inner.detail(detail);
    }

    /// Name a part of the current stage. Invisible unless tracing.
    pub fn region(&self, name: &str) {
        self.inner.region(name);
    }

    /// Begin a phase, and say how many items it expects in total.
    pub fn phase(&self, phase: Phase, total: u64) {
        self.inner.phase(phase, total);
    }

    /// Revise the current phase: how much work is left from here.
    pub fn expect_more(&self, remaining: u64) {
        self.inner.expect_more(remaining);
    }

    /// A line that stays, without a counter.
    pub fn say(&self, message: &str) {
        self.inner.say(message);
    }

    /// How many items the current stage has counted.
    pub fn counted(&self) -> u64 {
        self.inner.count.load(Ordering::Relaxed)
    }

    /// What the stages came to, against the clock, in one line.
    pub fn accounting(&self) -> String {
        self.inner.accounting()
    }
}

impl Shared {
    /// Whether a line is redrawn in place as the work goes.
    fn drawing(&self) -> bool {
        self.verbosity == Verbosity::Normal
    }

    /// Whether anything is written at all. Settled lines, messages and the
    /// accounting are this, not drawing — the distinction `--quiet` rests on.
    fn speaking(&self) -> bool {
        self.verbosity != Verbosity::Silent
    }

    /// Begin a stage, ending whatever was running.
    ///
    /// A stage ends because the next begins, so stage times sum to the run with
    /// no gaps (a separate `finish` once hid 16% of a run between stages). The
    /// ending stage's line is printed before the new one starts.
    pub fn stage(&self, label: &str, unit: Option<&str>) {
        self.close();
        {
            let mut line = self.line.lock().expect("progress line");
            line.label = label.to_string();
            line.region.clear();
            line.divided = false;
            line.settled = false;
            line.unit = unit.unwrap_or_default().to_string();
            line.detail.clear();
            line.started = Instant::now();
            line.stage_started = line.started;
        }
        self.count.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
        self.stage_peak.store(0, Ordering::Relaxed);
        self.render(false, true);
    }

    /// Name a part of the current stage, when tracing. Regions don't nest;
    /// naming the next ends the last, and the stage gets a summary line after.
    pub fn region(&self, name: &str) {
        // `--trace --quiet` is as quiet as `--quiet`: regions subdivide the
        // redrawn line.
        if !self.tracing || !self.drawing() {
            return;
        }
        // The first region replaces the stage's own line, which is printed as a
        // summary at the end; later regions settle the one before.
        if self.line.lock().expect("progress line").divided {
            self.settle();
        }
        {
            let mut line = self.line.lock().expect("progress line");
            line.region = name.to_string();
            line.divided = true;
            line.settled = false;
            line.started = Instant::now();
        }
        self.render(false, true);
    }

    /// Print the current line in its final form, once.
    fn settle(&self) {
        let worth_printing = {
            let line = self.line.lock().expect("progress line");
            !line.settled && !line.label.is_empty()
        };
        if worth_printing {
            self.render(true, true);
        }
        self.line.lock().expect("progress line").settled = true;
    }

    /// End the current stage: settle its line, add its time, summarise its
    /// regions.
    fn close(&self) {
        self.settle();
        let mut line = self.line.lock().expect("progress line");
        if line.label.is_empty() {
            return;
        }
        let mut ledger = self.ledger.lock().expect("the ledger");
        ledger.inside += line.stage_started.elapsed();
        if line.divided && self.speaking() {
            let total = line.stage_started.elapsed().as_secs_f64();
            let _ = writeln!(std::io::stderr(), "  {:<10} {total:>28.1}s", line.label);
        }
        line.label.clear();
        line.region.clear();
        line.divided = false;
        line.width = 0;
    }

    /// How many items this stage expects, when that is known up front.
    pub fn expect(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    /// One more of whatever this stage counts.
    pub fn item(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.phase_done.fetch_add(1, Ordering::Relaxed);
        self.render(false, false);
    }

    /// Begin a phase, estimating its total items.
    ///
    /// Stages are named pieces of work; phases are four pieces of time, which
    /// a percentage needs. The estimate can be wrong: 20% out on the climb moves
    /// the bar 5% within its band.
    pub fn phase(&self, phase: Phase, total: u64) {
        self.phase.store(phase as u64, Ordering::Relaxed);
        self.phase_done.store(0, Ordering::Relaxed);
        self.phase_total.store(total, Ordering::Relaxed);
    }

    /// Revise the current phase: how much work is left. Revising up would move
    /// the bar backwards, so it stalls instead.
    pub fn expect_more(&self, remaining: u64) {
        let done = self.phase_done.load(Ordering::Relaxed);
        self.phase_total.store(done + remaining, Ordering::Relaxed);
    }

    /// How far through the run, in hundredths of a percent. Never falls.
    fn reached(&self) -> u64 {
        let phase = match self.phase.load(Ordering::Relaxed) {
            0 => Phase::Reading,
            1 => Phase::Ingest,
            2 => Phase::Climb,
            _ => Phase::Encoding,
        };
        let (from, to) = phase.band();
        let total = self.phase_total.load(Ordering::Relaxed);
        let within = if total == 0 {
            0.0
        } else {
            (self.phase_done.load(Ordering::Relaxed) as f64 / total as f64).clamp(0.0, 1.0)
        };
        let now = ((from + (to - from) * within) * 10_000.0) as u64;
        self.reached.fetch_max(now, Ordering::Relaxed).max(now)
    }

    /// Say what is being worked on now. Call it *before* the work: when a tile
    /// takes seconds, this says which. Throttled, but a slow tile has always
    /// passed the throttle.
    pub fn detail(&self, detail: &str) {
        if !self.drawing() {
            return;
        }
        {
            let mut line = self.line.lock().expect("progress line");
            line.detail = detail.to_string();
        }
        self.render(false, false);
    }

    /// What the stages came to, against the clock, in one line.
    pub fn accounting(&self) -> String {
        let inside = self.ledger.lock().expect("the ledger").inside.as_secs_f64();
        let whole = self.epoch.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
        format!(
            "  stages {inside:.1}s, startup and teardown {:.1}s of {whole:.1}s",
            (whole - inside).max(0.0),
        )
    }

    /// A line that stays, without a counter.
    pub fn say(&self, message: &str) {
        if !self.speaking() {
            return;
        }
        // Print the running line first, or the message lands above its stage.
        self.settle();
        let mut line = self.line.lock().expect("progress line");
        let width = std::mem::replace(&mut line.width, 0);
        // Blank the live line only if there is one; otherwise the carriage
        // returns are litter in a log.
        if self.drawing() {
            let _ = writeln!(std::io::stderr(), "\r{:width$}\r{message}", "");
        } else {
            let _ = writeln!(std::io::stderr(), "{message}");
        }
    }

    fn render(&self, keep: bool, force: bool) {
        if !self.speaking() {
            return;
        }
        // Checked before the lock: this runs per feature on every thread, and
        // almost never has anything to do.
        let now = self.epoch.elapsed().as_nanos() as u64;
        let due =
            force || now.saturating_sub(self.drawn_at.load(Ordering::Relaxed)) >= REDRAW_NANOS;
        if due {
            self.drawn_at.store(now, Ordering::Relaxed);
            // Sampled here, since `--quiet` draws nothing until a stage ends.
            if let Some(read) = MEMORY.get() {
                self.stage_peak.fetch_max(read() as u64, Ordering::Relaxed);
            }
        }
        if !due {
            return;
        }
        // `keep` is a settled line; otherwise it's an in-place redraw.
        if !keep && !self.drawing() {
            return;
        }

        let mut line = self.line.lock().expect("progress line");
        if line.label.is_empty() {
            return;
        }
        // Already printed in final form; don't redraw under it.
        if line.settled && !keep {
            return;
        }

        let count = self.count.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        let counted = if line.unit.is_empty() {
            // Nothing counted: only the clock.
            String::new()
        } else if total > 0 {
            format!("{} of {}", thousands(count), thousands(total))
        } else {
            thousands(count)
        };

        // `stage::region` while tracing, otherwise just the stage.
        let named = if line.region.is_empty() {
            line.label.clone()
        } else {
            format!("{}::{}", line.label, line.region)
        };
        // Right of the clock, so the other columns don't move. A settled line
        // shows the stage's peak; the redrawn line shows now.
        let memory = match MEMORY.get() {
            Some(read) => {
                let bytes = if keep {
                    self.stage_peak.load(Ordering::Relaxed).max(read() as u64)
                } else {
                    read() as u64
                };
                format!(" {:>9}", thousands(bytes / 1_000_000) + " MB")
            }
            None => String::new(),
        };
        let text = format!(
            "{:>3}% {:<10} {:>16} {:<8} {:>6.1}s{memory}{}{}",
            self.reached() / 100,
            named,
            counted,
            line.unit,
            line.started.elapsed().as_secs_f64(),
            if line.detail.is_empty() { "" } else { "  " },
            line.detail,
        );

        let mut stderr = std::io::stderr();
        // Carriage return and padding only overwrite a line on screen; in a
        // log they're litter.
        if self.drawing() {
            let padding = line.width.saturating_sub(text.chars().count());
            let _ = write!(stderr, "\r{text}{:padding$}", "");
        } else {
            let _ = write!(stderr, "{text}");
        }
        if keep {
            let _ = writeln!(stderr);
            line.width = 0;
            // The label stays for `close`, which clears it after booking the
            // time.
        } else {
            line.width = text.chars().count();
            let _ = stderr.flush();
        }
    }
}

/// `1234567` as `1,234,567`.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_grouped_for_reading() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(368_050), "368,050");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn a_silent_reporter_counts_without_writing() {
        let progress = Progress::silent();
        progress.stage("cutting", Some("features"));
        for _ in 0..1000 {
            progress.item();
        }
        progress.detail("ignored");
        progress.done();
        assert_eq!(progress.counted(), 1000);
        assert_eq!(
            progress.inner.line.lock().unwrap().width,
            0,
            "nothing should have been drawn"
        );
    }

    #[test]
    fn counting_is_cheap_enough_for_millions_of_items() {
        // A guard against `item` growing an expensive body, not a benchmark.
        let progress = Progress::silent();
        progress.stage("counting", Some("things"));
        let started = Instant::now();
        for _ in 0..10_000_000u64 {
            progress.item();
        }
        let taken = started.elapsed();
        assert_eq!(progress.counted(), 10_000_000);
        assert!(taken < Duration::from_secs(2), "ten million took {taken:?}");
    }

    #[test]
    fn a_stage_that_counts_nothing_still_ends_cleanly() {
        let progress = Progress::silent();
        progress.stage("idle", Some("tiles"));
        progress.done();
        assert_eq!(progress.counted(), 0);
    }
}
