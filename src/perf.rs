//! Per-frame instrumentation, in two tiers.
//!
//! The rule this module exists to enforce: **the hot path deposits numbers and
//! nothing else.** All formatting, aggregation and I/O happen on the
//! `perf-writer` thread. A single log call carrying a handful of floats costs
//! several microseconds -- a measurable fraction of the frame budget spent
//! describing the frame instead of processing it -- which is why timing a stage
//! by logging it is never acceptable here.
//!
//! * **Tier 1 -- always on** (`telemetry.MEASURE_FPS`). A running budget
//!   summary: frame count, over-budget count, a coarse histogram. A few hundred
//!   nanoseconds per frame.
//! * **Tier 2 -- gated** (`telemetry.MEASURE_STAGES`, exposed as [`is_on`]). A
//!   full per-frame row of stage timings, appended to a bounded queue and
//!   drained by the writer thread into CSV.
//!
//! Porting note
//! ------------
//! The two-tier split matters more in a compiled port, not less: the reason to
//! keep tier 2 off by default is the CSV row's *I/O*, not its arithmetic, and
//! that cost does not go away when the arithmetic gets faster.
//!
//! The Python warns against `from kerbside.perf import ON`, because a
//! from-import binds the value at import time and the gate stops working.
//! Rust has the same trap in a different costume -- a `static mut` copied into
//! a local, or a `bool` captured at construction -- so the flag is an
//! `AtomicBool` read through [`is_on`] and never handed out by value.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Ordered stage names. A row is one float per name, in this order. Keeping it
/// a flat array rather than a map is what makes the row cheap to build.
pub const STAGES: [&str; 21] = [
    "pre",
    "bg",
    "morph",
    "blobs",
    "bl_find",
    "bl_filter",
    "infer_join",
    "infer",
    "score",
    "sc_sample",
    "assoc",
    "as_score",
    "as_pick",
    "as_life",
    "speed",
    "sp_project",
    "sp_fit",
    "gate",
    "emit",
    "em_record",
    "total",
];

/// Index of a stage name, resolved once at each call site by the compiler when
/// the name is a literal. A miss is a programming error, so it panics rather
/// than silently writing into the wrong column.
#[inline]
fn index_of(stage: &str) -> usize {
    match STAGES.iter().position(|s| *s == stage) {
        Some(i) => i,
        None => panic!("unknown perf stage {stage:?}; add it to perf::STAGES"),
    }
}

/// The tier-2 gate.
static ON: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn is_on() -> bool {
    // Relaxed is right: this flag is set once at startup before any frame runs,
    // and a stray read either way for one frame would cost one missing row.
    // Paying for an acquire fence on every stage boundary would be
    // instrumentation that distorts what it measures.
    ON.load(Ordering::Relaxed)
}

/// Tier-1 state.
///
/// The Python uses plain ints and notes they are only ever incremented on the
/// pipeline thread. That is true here too, but "only one thread writes" is not
/// something Rust will take on trust, and the summary is read from the main
/// thread at shutdown. Relaxed atomics say exactly that and compile to the same
/// instruction an unsynchronised increment would.
pub struct Counters {
    pub frames: AtomicU64,
    pub over_budget: AtomicU64,
    pub dropped: AtomicU64,
    pub inferences: AtomicU64,
    pub violations: AtomicU64,
    /// Milliseconds, as bits of an f64. Kept atomic for the same reason.
    total_ms_bits: AtomicU64,
    max_ms_bits: AtomicU64,
    /// Coarse latency histogram, bucket i = [i, i+1) ms, last is overflow.
    buckets: [AtomicU64; 64],
}

impl Counters {
    /// Tier 1. Called unconditionally, once per frame. Keep it this cheap.
    pub fn note_frame(&self, elapsed_ms: f64, budget_ms: f64) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        let total = f64::from_bits(self.total_ms_bits.load(Ordering::Relaxed)) + elapsed_ms;
        self.total_ms_bits.store(total.to_bits(), Ordering::Relaxed);
        let max = f64::from_bits(self.max_ms_bits.load(Ordering::Relaxed));
        if elapsed_ms > max {
            self.max_ms_bits.store(elapsed_ms.to_bits(), Ordering::Relaxed);
        }
        if elapsed_ms > budget_ms {
            self.over_budget.fetch_add(1, Ordering::Relaxed);
        }
        let bucket = elapsed_ms as usize;
        self.buckets[bucket.min(63)].fetch_add(1, Ordering::Relaxed);
    }

    pub fn max_ms(&self) -> f64 {
        f64::from_bits(self.max_ms_bits.load(Ordering::Relaxed))
    }

    pub fn total_ms(&self) -> f64 {
        f64::from_bits(self.total_ms_bits.load(Ordering::Relaxed))
    }

    pub fn histogram(&self) -> Vec<u64> {
        self.buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect()
    }

    pub fn summary(&self) -> String {
        let frames = self.frames.load(Ordering::Relaxed);
        if frames == 0 {
            return obfstr::obfstr!("perf: no frames").to_string();
        }
        let mean = self.total_ms() / frames as f64;
        let over = self.over_budget.load(Ordering::Relaxed);
        let pct = 100.0 * over as f64 / frames as f64;
        format!(
            "perf: {frames} frames  mean {mean:.2} ms  max {:.2} ms  \
             over-budget {over} ({pct:.2}%)  dropped {}  inferences {}  violations {}",
            self.max_ms(),
            self.dropped.load(Ordering::Relaxed),
            self.inferences.load(Ordering::Relaxed),
            self.violations.load(Ordering::Relaxed),
        )
    }

    pub fn reset(&self) {
        self.frames.store(0, Ordering::Relaxed);
        self.over_budget.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.inferences.store(0, Ordering::Relaxed);
        self.violations.store(0, Ordering::Relaxed);
        self.total_ms_bits.store(0, Ordering::Relaxed);
        self.max_ms_bits.store(0, Ordering::Relaxed);
        for bucket in &self.buckets {
            bucket.store(0, Ordering::Relaxed);
        }
    }
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            frames: AtomicU64::new(0),
            over_budget: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            inferences: AtomicU64::new(0),
            violations: AtomicU64::new(0),
            total_ms_bits: AtomicU64::new(0),
            max_ms_bits: AtomicU64::new(0),
            // `[AtomicU64; 64]` has no `Default`, because `AtomicU64` is not
            // `Copy` and the array form cannot be built by repetition.
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

pub fn counters() -> &'static Counters {
    static COUNTERS: OnceLock<Counters> = OnceLock::new();
    COUNTERS.get_or_init(Counters::default)
}

/// One frame's tier-2 row, under construction.
///
/// A preallocated array of floats plus a slot per stage for its start
/// timestamp. `start`/`end` pairs may nest (`blobs` contains `bl_find`), which
/// is why the timestamp is per stage rather than a single field.
pub struct Frame {
    pub frame_id: i64,
    pub values: [f64; STAGES.len()],
    starts: [Option<Instant>; STAGES.len()],
}

impl Frame {
    pub fn new(frame_id: i64) -> Self {
        Self {
            frame_id,
            values: [0.0; STAGES.len()],
            starts: [None; STAGES.len()],
        }
    }

    #[inline]
    pub fn start(&mut self, stage: &str) {
        if is_on() {
            self.starts[index_of(stage)] = Some(Instant::now());
        }
    }

    #[inline]
    pub fn end(&mut self, stage: &str) {
        if is_on() {
            let i = index_of(stage);
            if let Some(began) = self.starts[i].take() {
                self.values[i] = began.elapsed().as_secs_f64() * 1000.0;
            }
        }
    }

    #[inline]
    pub fn set(&mut self, stage: &str, value: f64) {
        if is_on() {
            self.values[index_of(stage)] = value;
        }
    }
}

/// Drains finished rows off the hot path and writes them as CSV.
///
/// The producer never blocks on this thread beyond a very short mutex: it
/// pushes onto a bounded queue and returns. If the writer falls behind, rows
/// are dropped from the front -- losing measurements is strictly better than
/// perturbing what is measured.
struct Writer {
    rows: Mutex<VecDeque<(i64, [f64; STAGES.len()])>>,
    wake: Condvar,
    stop: Mutex<bool>,
    flush: Duration,
    handle: Mutex<Option<JoinHandle<()>>>,
}

const MAX_QUEUED_ROWS: usize = 20_000;

impl Writer {
    fn submit(&self, frame: &Frame) {
        // Hot path. One lock, one push, nothing else.
        let mut rows = match self.rows.lock() {
            Ok(rows) => rows,
            // A poisoned queue means the writer thread panicked. Losing perf
            // rows must never take the pipeline down with it.
            Err(poisoned) => poisoned.into_inner(),
        };
        if rows.len() == MAX_QUEUED_ROWS {
            rows.pop_front();
        }
        rows.push_back((frame.frame_id, frame.values));
    }

    fn drain_to(&self, out: &mut BufWriter<File>) {
        let chunk: Vec<(i64, [f64; STAGES.len()])> = {
            let mut rows = match self.rows.lock() {
                Ok(rows) => rows,
                Err(poisoned) => poisoned.into_inner(),
            };
            rows.drain(..).collect()
        };
        for (frame_id, values) in chunk {
            let mut line = String::with_capacity(8 + STAGES.len() * 8);
            line.push_str(&frame_id.to_string());
            for value in values {
                line.push(',');
                line.push_str(&format!("{value:.4}"));
            }
            line.push('\n');
            let _ = out.write_all(line.as_bytes());
        }
        let _ = out.flush();
    }
}

static WRITER: OnceLock<&'static Writer> = OnceLock::new();

/// Turn tier 2 on and start the writer thread. Called once, from `main`.
pub fn configure(enabled: bool, directory: &str, flush_ms: i64, run_name: &str) -> Result<(), String> {
    ON.store(enabled, Ordering::Relaxed);
    if !enabled {
        return Ok(());
    }
    fs::create_dir_all(directory).map_err(|e| format!("{}{directory:?}: {e}", obfstr::obfstr!("cannot create ")))?;
    let path = Path::new(directory).join(format!("{}{run_name}{}", obfstr::obfstr!("perf_"), obfstr::obfstr!(".csv")));

    // Leaked deliberately: the writer outlives every frame and is reachable
    // from the pipeline thread for the whole run. One allocation, once.
    let writer: &'static Writer = Box::leak(Box::new(Writer {
        rows: Mutex::new(VecDeque::with_capacity(1024)),
        wake: Condvar::new(),
        stop: Mutex::new(false),
        flush: Duration::from_millis(flush_ms.max(1) as u64),
        handle: Mutex::new(None),
    }));

    let mut file = BufWriter::new(
        File::create(&path).map_err(|e| format!("{}{}: {e}", obfstr::obfstr!("cannot open "), path.display()))?,
    );
    writeln!(file, "frame_id,{}", STAGES.join(","))
        .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot write the perf header: ")))?;
    file.flush().map_err(|e| format!("{}{}: {e}", obfstr::obfstr!("cannot flush "), path.display()))?;

    let handle = std::thread::Builder::new()
        .name("perf-writer".into())
        .spawn(move || {
            let mut out = file;
            loop {
                let stopped = {
                    let guard = writer.stop.lock().unwrap_or_else(|e| e.into_inner());
                    let (guard, _timeout) = writer
                        .wake
                        .wait_timeout(guard, writer.flush)
                        .unwrap_or_else(|e| e.into_inner());
                    *guard
                };
                writer.drain_to(&mut out);
                if stopped {
                    return;
                }
            }
        })
        .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot start the perf writer thread: ")))?;

    *writer.handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    let _ = WRITER.set(writer);
    Ok(())
}

#[inline]
pub fn submit(frame: &Frame) {
    if is_on() {
        if let Some(writer) = WRITER.get() {
            writer.submit(frame);
        }
    }
}

/// Stop the writer and return the tier-1 summary.
pub fn shutdown() -> String {
    if let Some(writer) = WRITER.get() {
        {
            let mut stop = writer.stop.lock().unwrap_or_else(|e| e.into_inner());
            *stop = true;
        }
        writer.wake.notify_all();
        let handle = writer
            .handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
    counters().summary()
}

/// Tests only. The counters are process-global, so a test must restore them.
pub fn reset() {
    counters().reset();
}

/// Nearest-rank percentiles. For the report tool, never the hot path.
pub fn percentiles(values: &[f64], points: &[f64]) -> Vec<f64> {
    if values.is_empty() {
        return vec![0.0; points.len()];
    }
    let mut ordered = values.to_vec();
    ordered.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    points
        .iter()
        .map(|p| {
            let rank = (p / 100.0 * ordered.len() as f64).round() as i64 - 1;
            let idx = rank.clamp(0, ordered.len() as i64 - 1) as usize;
            ordered[idx]
        })
        .collect()
}
