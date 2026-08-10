//! Pins the allocation churn.
//!
//! The Python version of this file exists because its evidence record and its
//! evidence ring are the measured cause of that implementation's latency tail:
//! hundreds of GC-tracked containers per frame, retained hundreds of frames
//! deep, walked in full by every major collection.
//!
//! **This port has no tracing collector, and that is the finding -- not a
//! licence to delete the allocation.** If the record got cheaper here, the
//! comparison would be between a heavy Python and a light Rust rather than
//! between two implementations of the same program, and the headline number
//! would be meaningless. So the same data is built, at the same rate, and
//! retained to the same depth. What changed is who has to walk it: nobody.
//!
//! These tests therefore assert exactly what the Python's do -- that the record
//! is built every frame, that it is heavy, that it carries the audit trail, and
//! that retention scales with ring depth -- minus the one assertion that has no
//! meaning here (that the collector is enabled).

mod common;

use common::{clip_settings, run_clip_pipeline};
use kerbside::config::Value;

const FRAMES: i64 = 260;

/// The background model needs to see the empty road before it can report
/// foreground, so the first frames of any clip have almost nothing in them.
/// Measuring churn across them would average in a period the device is not yet
/// doing its job.
const WARMUP: usize = 130;

/// Separately owned allocations the evidence record must produce per frame,
/// floor. Well below the median so a quiet stretch of road does not fail the
/// suite.
///
/// The number is lower than the Python's 150 because the two count differently:
/// CPython tracks one tuple per contour point, where this holds a contour in a
/// single `Vec`. Same retained data, different unit -- see
/// `EvidenceRecord::retained_allocations`.
const MIN_ALLOCATIONS_PER_RECORD: usize = 20;

fn settings(extra: &[(&str, Value)]) -> kerbside::config::Settings {
    let mut overrides: Vec<(&str, Value)> = vec![
        ("telemetry.MEASURE_STAGES", Value::Bool(false)),
        ("enforcement.EVIDENCE_PRE_FRAMES", Value::Int(40)),
        ("telemetry.RING_FRAMES", Value::Int(60)),
    ];
    overrides.extend_from_slice(extra);
    clip_settings(FRAMES, &overrides)
}

#[test]
fn the_evidence_record_is_built_and_is_heavy() {
    let (outputs, _pipeline) = run_clip_pipeline(&settings(&[]), FRAMES);
    let records: Vec<_> = outputs.iter().filter_map(|o| o.record.as_ref()).collect();
    assert_eq!(
        records.len(),
        outputs.len(),
        "the evidence record must be built every frame"
    );

    let mut counts: Vec<usize> = records[WARMUP.min(records.len())..]
        .iter()
        .map(|r| r.retained_allocations())
        .collect();
    counts.sort_unstable();
    let median = counts[counts.len() / 2];
    assert!(
        median >= MIN_ALLOCATIONS_PER_RECORD,
        "per-frame record is down to {median} retained allocations (floor \
         {MIN_ALLOCATIONS_PER_RECORD}). This is not an optimisation \
         opportunity -- see consumers::build_evidence_record. Reducing it \
         removes the very thing this port is measuring against."
    );
}

/// The audit trail is the largest part, and it must stay.
///
/// A packet that states a speed without the samples it was derived from is not
/// evidence, it is an assertion.
#[test]
fn the_record_carries_the_observations_behind_each_reading() {
    let (outputs, _pipeline) = run_clip_pipeline(&settings(&[]), FRAMES);
    let with_history: Vec<_> = outputs
        .iter()
        .filter_map(|o| o.record.as_ref())
        .flat_map(|r| r.vehicles.iter())
        .filter(|v| v.observations.len() > 10)
        .collect();
    assert!(
        !with_history.is_empty(),
        "no vehicle record carried its observation history"
    );
    let sample = with_history.last().unwrap().observations[0];
    // frame id, time, across, along, coverage.
    assert!(sample.0 >= 0);
    assert!(sample.1.is_finite() && sample.2.is_finite() && sample.3.is_finite());
}

/// Retention is what promotes the garbage in the Python; here it is what keeps
/// the memory profile comparable.
///
/// Also checks the frames are held by reference rather than copied -- a copy
/// would cost megabytes per frame and would be a different bug.
#[test]
fn the_ring_retains_frames_by_reference() {
    let (outputs, pipeline) =
        run_clip_pipeline(&settings(&[("telemetry.RING_FRAMES", Value::Int(60))]), FRAMES);
    let ring = &pipeline.consumers.ring;
    assert_eq!(ring.len(), 60, "the ring must fill to its configured depth");

    let retained = ring.snapshot();
    assert!(
        std::sync::Arc::ptr_eq(
            &retained.last().unwrap().frame.full,
            &outputs.last().unwrap().frame.full
        ),
        "frames must not be copied into the ring"
    );
    assert!(ring.tracked_containers() > 60 * MIN_ALLOCATIONS_PER_RECORD);
}

/// Deeper ring, proportionally more retained data.
///
/// This is the knob that turns a short collection into a long one in the
/// Python, so the relationship has to hold here too -- it is how a benchmark
/// reproduces the deployed memory profile rather than a developer machine's.
#[test]
fn churn_scales_with_ring_depth() {
    // EVIDENCE_PRE_FRAMES has to come down too: the ring depth is derived to
    // cover the pre-trigger window, so asking for a shallow ring on its own
    // silently gets clamped back up. That is the derived-field mechanism
    // working, and it is easy to trip over.
    let shallow = settings(&[
        ("telemetry.RING_FRAMES", Value::Int(30)),
        ("enforcement.EVIDENCE_PRE_FRAMES", Value::Int(20)),
    ]);
    let deep = settings(&[
        ("telemetry.RING_FRAMES", Value::Int(120)),
        ("enforcement.EVIDENCE_PRE_FRAMES", Value::Int(20)),
    ]);
    let (_o1, shallow) = run_clip_pipeline(&shallow, FRAMES);
    let (_o2, deep) = run_clip_pipeline(&deep, FRAMES);

    assert_eq!(shallow.consumers.ring.len(), 30);
    assert_eq!(deep.consumers.ring.len(), 120);
    // Not the full 4x of the depth ratio: the deeper ring reaches back into the
    // warm-up, where records are thin. The relationship is what matters.
    assert!(
        deep.consumers.ring.tracked_containers() as f64
            > 2.2 * shallow.consumers.ring.tracked_containers() as f64,
        "deep ring retains {}, shallow retains {}",
        deep.consumers.ring.tracked_containers(),
        shallow.consumers.ring.tracked_containers()
    );
}

/// The Python asserts here that a full collection has a large graph to walk.
///
/// The graph is still large -- that assertion is kept -- but there is no
/// collection to walk it, which is the whole point of the exercise. What can be
/// pinned is that the retained structure did not quietly shrink.
#[test]
fn the_retained_graph_is_still_large() {
    let (_outputs, pipeline) = run_clip_pipeline(
        &settings(&[("telemetry.RING_FRAMES", Value::Int(120))]),
        FRAMES,
    );
    let retained = pipeline.consumers.ring.tracked_containers();
    assert!(
        retained > 2_000,
        "only {retained} allocations retained; the ring is too shallow or the \
         record too thin for the comparison against the Python to mean anything"
    );
}
