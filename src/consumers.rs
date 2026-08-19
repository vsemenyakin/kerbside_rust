//! What happens to a frame after the pipeline has finished with it.
//!
//! The consumers run in a fixed order on the pipeline thread:
//!
//! ```text
//!     1. record   build the evidence record   (only if enabled)
//!     2. ring     retain the frame for export (holds `full` by reference)
//!     3. sink     emit the result row / overlay
//! ```
//!
//! `ring` must run after `record` so a retained frame carries its record.
//! `sink` runs last because it is the only one that may block.
//!
//! Two of these are deliberate allocation-pressure sites. Read the notes.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::config::Settings;
use crate::enforce::Verdict;
use crate::perf;
use crate::pipeline::types::{FrameResult, RawFrame};
use crate::track::types::{FrameVehicles, Observation};

/// Everything one frame produced. Immutable; safe to hand anywhere.
pub struct FrameOutput {
    pub frame: RawFrame,
    pub result: FrameResult,
    pub vehicles: FrameVehicles,
    pub verdicts: Vec<Verdict>,
    pub coverage: Vec<f64>,
    pub record: Option<EvidenceRecord>,
}

impl FrameOutput {
    pub fn violations(&self) -> impl Iterator<Item = &Verdict> {
        self.verdicts.iter().filter(|v| v.violation)
    }

    pub fn violation_count(&self) -> usize {
        self.verdicts.iter().filter(|v| v.violation).count()
    }
}

/// What a sink has to say for itself once the run is over.
///
/// The Python keeps the writer in `main` and closes it there, because the
/// consumer chain holds the same object by reference and both can reach it.
/// Ownership here is singular, so the sink reports upward instead of being
/// reached into -- which also means the digest cannot be read before the file
/// has actually been flushed.
pub struct SinkSummary {
    pub path: String,
    pub rows: u64,
    pub violations: u64,
    pub digest: String,
}

/// Anything that wants finished frames.
///
/// Takes the `Arc`, not a plain reference: the ring and the sinks hold the very
/// same frame, exactly as the Python's refcounted object graph does, and a sink
/// that wants to keep one -- a test collector, an evidence exporter -- must be
/// able to without copying two megabytes of pixels.
pub trait Consumer {
    fn consume(&mut self, output: &Arc<FrameOutput>) -> Result<(), String>;

    /// Flush, close, and report. Called once, after the last frame.
    fn finish(&mut self) -> Result<Option<SinkSummary>, String> {
        Ok(None)
    }
}

/// Combine several sinks into one. Order is preserved.
pub struct FanOut {
    sinks: Vec<Box<dyn Consumer + Send>>,
}

impl FanOut {
    pub fn new(sinks: Vec<Box<dyn Consumer + Send>>) -> Self {
        Self { sinks }
    }

    pub fn sinks_mut(&mut self) -> &mut Vec<Box<dyn Consumer + Send>> {
        &mut self.sinks
    }
}

impl Consumer for FanOut {
    fn consume(&mut self, output: &Arc<FrameOutput>) -> Result<(), String> {
        for sink in self.sinks.iter_mut() {
            sink.consume(output)?;
        }
        Ok(())
    }

    /// Closes every sink even if an earlier one failed, then reports the first
    /// summary offered. A half-closed overlay is a corrupt mp4, and losing it
    /// because the CSV flush failed would be a second bug on top of the first.
    fn finish(&mut self) -> Result<Option<SinkSummary>, String> {
        let mut summary = None;
        let mut first_error = None;
        for sink in self.sinks.iter_mut() {
            match sink.finish() {
                Ok(Some(s)) if summary.is_none() => summary = Some(s),
                Ok(_) => {}
                Err(e) if first_error.is_none() => first_error = Some(e),
                Err(_) => {}
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(summary),
        }
    }
}

// --------------------------------------------------------------------------
// 1. record
// --------------------------------------------------------------------------

/// One blob, as the evidence record describes it.
pub struct BlobRecord {
    pub box_: (f64, f64, f64, f64),
    pub area: f64,
    pub fill: f64,
    pub aspect: f64,
    pub coverage: Option<f64>,
    /// One entry per contour point. This is most of the container count, and
    /// the part that scales with traffic -- which is why the tail gets worse
    /// exactly when the road is busiest.
    pub contour: Vec<(i32, i32)>,
}

pub struct FitRecord {
    pub residual_m: f64,
    pub baseline_m: f64,
    pub samples: usize,
}

pub struct CountsRecord {
    pub hits: i64,
    pub misses: i64,
    pub age: i64,
}

pub struct FlagsRecord {
    pub confirmed: bool,
    pub in_zone: bool,
    pub is_long: bool,
}

pub struct VehicleRecord {
    pub id: i64,
    pub box_: (f64, f64, f64, f64),
    pub world: (f64, f64),
    pub speed_kph: f64,
    pub length_m: f64,
    pub fit: FitRecord,
    pub counts: CountsRecord,
    /// Every observation that fed the fit, not just its result.
    ///
    /// This is the audit trail, and in this domain it is the point: a reading
    /// used against a person has to be reconstructible, so the packet carries
    /// the samples rather than only the number derived from them. It is also,
    /// deliberately, a large and growing allocation -- see the note on
    /// [`build_evidence_record`].
    pub observations: Vec<(i64, f64, f64, f64, f64)>,
    pub flags: FlagsRecord,
    pub coverage: f64,
}

pub struct GatesRecord {
    pub in_zone: bool,
    pub confirmed: bool,
    pub enough_samples: bool,
    pub enough_baseline: bool,
    pub fit_good: bool,
    pub confident: bool,
    pub over_limit: bool,
    pub stable: bool,
}

pub struct VerdictRecord {
    pub vehicle_id: i64,
    pub violation: bool,
    pub speed_kph: f64,
    pub limit_kph: f64,
    pub excess_kph: f64,
    pub reason: &'static str,
    pub streak: i64,
    pub gates: GatesRecord,
}

pub struct DiagnosticsRecord {
    pub n_vehicles: usize,
    pub n_confirmed: usize,
    pub n_in_zone: usize,
    pub ids: Vec<i64>,
}

/// A complete, self-contained description of one frame, as plain data.
pub struct EvidenceRecord {
    pub frame_id: i64,
    pub t_capture: f64,
    pub skipped: i64,
    pub foreground_ratio: f64,
    pub inference_ran: bool,
    pub blobs: Vec<BlobRecord>,
    pub vehicles: Vec<VehicleRecord>,
    pub verdicts: Vec<VerdictRecord>,
    pub diagnostics: DiagnosticsRecord,
}

impl EvidenceRecord {
    /// Rough count of separately allocated containers currently retained.
    ///
    /// The Python counts GC-tracked dicts, lists and tuples, because those are
    /// what its collector has to walk. Nothing here is GC-tracked -- that is
    /// the whole point of the port -- so the analogous quantity is the number
    /// of heap allocations the record keeps alive. Same shape of data, same
    /// retention, and the comparison between the two numbers is exactly the
    /// measurement the exercise wants: the allocations did not go away, the
    /// tracing collector did.
    pub fn retained_allocations(&self) -> usize {
        // The record itself, its four vectors, and one allocation per element
        // plus one per nested vector.
        let mut total = 1 + 4;
        total += self.blobs.len() * 2; // the record and its contour vector
        total += self.vehicles.len() * 2; // the record and its observation vector
        total += self.verdicts.len();
        total += 1; // the diagnostics id vector
        total
    }
}

/// Build the per-frame evidence record.
///
/// -----------------------------------------------------------------------
/// THIS IS A DELIBERATE ALLOCATION SITE. DO NOT OPTIMISE IT.
/// -----------------------------------------------------------------------
/// This allocates several hundred separately owned containers every frame:
/// nested records, vectors, and one entry per contour point. Retained in a ring
/// hundreds of frames deep, that is millions of live allocations.
///
/// In the Python those are garbage-collector-tracked containers, and a full
/// collection has to walk every one of them. That walk is the application's
/// worst latency event by a wide margin, and it is invisible in a per-stage
/// profile: it does not happen inside a stage, it happens at whichever
/// allocation crosses the threshold, and it blames whatever code was unlucky
/// enough to be running.
///
/// **Here the walk does not happen at all** -- the memory is freed when the
/// ring evicts the frame, deterministically, on the thread that evicted it.
/// That is the single most important measurement this port produces, and it
/// only means something if the allocation itself is still here. A compact
/// binary encoding would be faster in both languages and would erase the
/// comparison.
///
/// It is also, in this domain, a genuine product requirement rather than debug
/// output: a device that produces evidence used against a person has to be able
/// to show every observation and every intermediate that led to a reading.
pub fn build_evidence_record(
    frame: &RawFrame,
    result: &FrameResult,
    vehicles: &FrameVehicles,
    verdicts: &[Verdict],
    coverage: &[f64],
    observations: &[(i64, &[Observation])],
) -> EvidenceRecord {
    EvidenceRecord {
        frame_id: frame.frame_id,
        t_capture: frame.t_capture,
        skipped: frame.skipped,
        foreground_ratio: result.foreground_ratio,
        inference_ran: result.inference_ran,
        blobs: result
            .blobs
            .iter()
            .enumerate()
            .map(|(i, b)| BlobRecord {
                box_: (b.box_.x, b.box_.y, b.box_.w, b.box_.h),
                area: b.area,
                fill: b.fill,
                aspect: b.aspect(),
                coverage: coverage.get(i).copied(),
                contour: b.contour.clone(),
            })
            .collect(),
        vehicles: vehicles
            .vehicles
            .iter()
            .map(|v| VehicleRecord {
                id: v.vehicle_id,
                box_: (v.box_.x, v.box_.y, v.box_.w, v.box_.h),
                world: (v.across_m, v.along_m),
                speed_kph: v.speed_kph,
                length_m: v.length_m,
                fit: FitRecord {
                    residual_m: v.fit_residual_m,
                    baseline_m: v.baseline_m,
                    samples: v.samples,
                },
                counts: CountsRecord {
                    hits: v.hits,
                    misses: v.misses,
                    age: v.age,
                },
                // Rounded here rather than at the producer, as the Python
                // does it: the observation itself keeps full precision for the
                // fit, and only the audit copy is trimmed.
                observations: observations
                    .iter()
                    .find(|(id, _)| *id == v.vehicle_id)
                    .map(|(_, obs)| {
                        obs.iter()
                            .map(|o| {
                                (
                                    o.frame_id,
                                    round_to(o.t, 4),
                                    round_to(o.across_m, 3),
                                    round_to(o.along_m, 3),
                                    round_to(o.coverage, 3),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                flags: FlagsRecord {
                    confirmed: v.confirmed,
                    in_zone: v.in_zone,
                    is_long: v.is_long(),
                },
                coverage: v.coverage,
            })
            .collect(),
        verdicts: verdicts
            .iter()
            .map(|w| VerdictRecord {
                vehicle_id: w.vehicle_id,
                violation: w.violation,
                speed_kph: w.speed_kph,
                limit_kph: w.limit_kph,
                excess_kph: w.excess_kph,
                reason: w.reason(),
                streak: w.streak,
                gates: GatesRecord {
                    in_zone: w.in_zone,
                    confirmed: w.confirmed,
                    enough_samples: w.enough_samples,
                    enough_baseline: w.enough_baseline,
                    fit_good: w.fit_good,
                    confident: w.confident,
                    over_limit: w.over_limit,
                    stable: w.stable,
                },
            })
            .collect(),
        diagnostics: DiagnosticsRecord {
            n_vehicles: vehicles.diagnostics.n_vehicles,
            n_confirmed: vehicles.diagnostics.n_confirmed,
            n_in_zone: vehicles.diagnostics.n_in_zone,
            ids: vehicles.diagnostics.ids.clone(),
        },
    }
}

/// Round to `digits` decimal places the way the Python's `round()` does.
///
/// Ties to even, not away from zero. The values this touches never reach the
/// result CSV, so the choice is not load-bearing for the oracle -- it is here
/// so the two evidence records read the same when someone compares them by eye.
pub fn round_to(value: f64, digits: i32) -> f64 {
    let scale = 10f64.powi(digits);
    (value * scale).round_ties_even() / scale
}

// --------------------------------------------------------------------------
// 2. ring
// --------------------------------------------------------------------------

/// Retains recent frames so a violation can be exported with its context.
///
/// -----------------------------------------------------------------------
/// THIS IS A DELIBERATE MEMORY-PRESSURE SITE. DO NOT OPTIMISE IT.
/// -----------------------------------------------------------------------
/// Frames are held **by reference** -- an `Arc` per frame, which is exactly
/// what the Python's reference counting gives it. Copying a two-megabyte colour
/// frame per consumer would cost more than several pipeline stages combined, so
/// the ring keeps the original and the whole chain shares it -- which is
/// correct, and which also means the ring pins `RING_FRAMES` full-resolution
/// buffers plus every evidence record attached to them.
///
/// That retention is what turns the per-frame churn above into long collections
/// *in the Python*: short-lived garbage is cheap, but garbage that survives long
/// enough to be promoted is not. Shrinking the ring makes the Python's tail look
/// dramatically better and makes the proxy dishonest, so this port keeps the
/// same depth and the same retention even though nothing here traces it.
///
/// The domain requires it independently: a violation packet has to show the
/// approach as well as the moment, which means frames from before the trigger
/// -- and you cannot retain those retrospectively.
pub struct EvidenceRing {
    frames: VecDeque<Arc<FrameOutput>>,
    capacity: usize,
}

impl EvidenceRing {
    pub fn new(settings: &Settings) -> Self {
        let capacity = settings.telemetry.RING_FRAMES.max(0) as usize;
        Self {
            frames: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
        }
    }

    pub fn push(&mut self, output: Arc<FrameOutput>) {
        if self.capacity == 0 {
            return;
        }
        if self.frames.len() == self.capacity {
            self.frames.pop_front();
        }
        self.frames.push_back(output);
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn snapshot(&self) -> Vec<Arc<FrameOutput>> {
        self.frames.iter().cloned().collect()
    }

    /// Rough count of retained containers. See
    /// [`EvidenceRecord::retained_allocations`] for what this counts and why it
    /// is the right thing to compare against the Python's figure.
    pub fn tracked_containers(&self) -> usize {
        self.frames
            .iter()
            .filter_map(|f| f.record.as_ref())
            .map(|r| r.retained_allocations())
            .sum()
    }
}

// --------------------------------------------------------------------------
// assembly
// --------------------------------------------------------------------------

/// Builds and runs the ordered consumer chain.
pub struct ConsumerChain {
    pub ring: EvidenceRing,
    record_enabled: bool,
    sink: Option<FanOut>,
}

impl ConsumerChain {
    pub fn new(settings: &Settings, sink: Option<FanOut>) -> Self {
        Self {
            ring: EvidenceRing::new(settings),
            record_enabled: settings.telemetry.EVIDENCE_RECORD,
            sink,
        }
    }

    pub fn sink_mut(&mut self) -> Option<&mut FanOut> {
        self.sink.as_mut()
    }

    pub fn into_sink(self) -> Option<FanOut> {
        self.sink
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &mut self,
        frame: RawFrame,
        result: FrameResult,
        vehicles: FrameVehicles,
        verdicts: Vec<Verdict>,
        coverage: Vec<f64>,
        _settings: &Settings,
        pf: &mut perf::Frame,
        observations: &[(i64, &[Observation])],
    ) -> Result<Arc<FrameOutput>, String> {
        pf.start(crate::perf::stage::EM_RECORD);
        let record = if self.record_enabled {
            Some(build_evidence_record(
                &frame,
                &result,
                &vehicles,
                &verdicts,
                &coverage,
                observations,
            ))
        } else {
            None
        };
        pf.end(crate::perf::stage::EM_RECORD);

        let output = Arc::new(FrameOutput {
            frame,
            result,
            vehicles,
            verdicts,
            coverage,
            record,
        });

        // Ring before sink, so a retained frame is retained whether or not the
        // sink succeeds -- and so the ordering matches the Python's.
        self.ring.push(Arc::clone(&output));
        if let Some(sink) = self.sink.as_mut() {
            sink.consume(&output)?;
        }
        let violations = output.violation_count();
        if violations > 0 {
            perf::counters()
                .violations
                .fetch_add(violations as u64, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(output)
    }
}
