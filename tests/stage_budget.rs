//! Guards the property that makes this port worth measuring.
//!
//! The one number a port evaluation turns on is **how much of the frame is
//! already inside a native library**. If that ratio drifts, every conclusion
//! drawn from the comparison stops transferring to the system it stands in for.
//!
//! What is and is not asserted
//! ---------------------------
//! Absolute stage timings are *not* asserted. They depend on the host, and a
//! test that fails on a slower laptop teaches people to ignore the suite.
//!
//! What is asserted is structural: which stages exist, that the expensive ones
//! are the expected ones, that the inference really does hide behind the
//! background model, and that the native share has not *fallen*.
//!
//! The band that moved
//! -------------------
//! The Python pins native at 72-78%. This asserts a **floor** of 72% and no
//! ceiling, because raising it is the entire point: the interpreter-bound
//! stages are what a compiled build removes, so the native share going up is
//! the result, not a regression. Pinning a ceiling here would fail the suite
//! precisely when the port succeeded.
//!
//! The measurement goes through the real tier-2 instrumentation and its CSV,
//! rather than reaching into the perf module, because that path is what a
//! benchmark run uses and it is worth exercising.

mod common;

use std::collections::HashMap;

use common::{clip_settings, run_clip};
use kerbside::config::Value;
use kerbside::perf;

const FRAMES: i64 = 400;

/// Stages whose time is essentially one OpenCV or ONNX Runtime call. A rewrite
/// cannot make these faster -- the same native code runs either way.
/// `infer_join` is the residual wait for the worker thread, which is also time
/// the pipeline thread spends inside native code it cannot avoid.
const NATIVE_STAGES: [&str; 5] = ["pre", "bg", "morph", "bl_find", "infer_join"];

/// Stages that were interpreter-bound in the Python. These are what the
/// compiled build removes.
const HOST_STAGES: [&str; 9] = [
    "bl_filter",
    "sc_sample",
    "as_score",
    "as_pick",
    "as_life",
    "sp_project",
    "sp_fit",
    "gate",
    "em_record",
];

/// Run a clip with tier-2 instrumentation on and return per-stage means.
///
/// Computed once and shared: the clip is expensive and `perf::configure` starts
/// a writer thread that may only be started once per process. Each property
/// then gets its own test, so a failure names one thing.
fn timings() -> &'static HashMap<String, f64> {
    static TIMINGS: std::sync::OnceLock<HashMap<String, f64>> = std::sync::OnceLock::new();
    TIMINGS.get_or_init(measure)
}

fn measure() -> HashMap<String, f64> {
    let dir = std::env::temp_dir().join(format!("kerbside-perf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temp directory for the perf CSV");

    let settings = clip_settings(
        FRAMES,
        &[
            ("telemetry.MEASURE_STAGES", Value::Bool(true)),
            (
                "telemetry.PERF_DIR",
                Value::Str(dir.to_string_lossy().to_string()),
            ),
            ("telemetry.PERF_FLUSH_MS", Value::Int(10)),
        ],
    );
    perf::configure(
        true,
        &settings.telemetry.PERF_DIR,
        settings.telemetry.PERF_FLUSH_MS,
        "test",
    )
    .expect("the perf writer must start");

    let outputs = run_clip(&settings, FRAMES);
    assert_eq!(outputs.len(), FRAMES as usize);
    perf::shutdown();

    let path = dir.join("perf_test.csv");
    let text = std::fs::read_to_string(&path).expect("the perf CSV must exist");
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().expect("a header row").split(',').collect();

    let mut sums: HashMap<String, f64> = HashMap::new();
    let mut rows = 0usize;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() != header.len() {
            continue;
        }
        for (name, value) in header.iter().zip(fields.iter()).skip(1) {
            *sums.entry((*name).to_string()).or_insert(0.0) +=
                value.parse::<f64>().unwrap_or(0.0);
        }
        rows += 1;
    }
    assert!(rows > 0, "the perf writer produced no rows");

    let _ = std::fs::remove_dir_all(&dir);
    sums.into_iter()
        .map(|(name, total)| (name, total / rows as f64))
        .collect()
}

/// Every declared stage reports a time.
#[test]
fn every_declared_stage_reports_a_time() {
    let timings = timings();
    for stage in perf::STAGES {
        assert!(
            timings.contains_key(stage),
            "stage {stage:?} is declared but never written to the perf CSV"
        );
    }
    assert!(timings["total"] > 0.0, "the frame total was never recorded");
}

/// One MOG2 apply is the single dominant call, in either language.
///
/// If it stops being so, either the working resolution changed or something
/// else grew enough to matter, and both invalidate the port comparison.
#[test]
fn the_expensive_stages_are_the_expected_ones() {
    let timings = timings();
    let bg = timings["bg"];
    for stage in ["pre", "morph", "blobs", "score", "assoc", "speed", "gate"] {
        assert!(
            bg >= timings[stage],
            "{stage} ({:.3} ms) now costs more than the background model ({bg:.3} ms)",
            timings[stage]
        );
    }
}

/// Inference hides behind the background model.
///
/// The residual wait must be under the inference itself; if it is not, the
/// overlap is not happening and the frame is paying for both. This is the
/// assertion that catches the port having silently serialised the detector --
/// which, without a GIL to make the overlap accidental, is the most likely way
/// to get it wrong.
#[test]
fn inference_overlaps_the_background_model() {
    let timings = timings();
    let infer = timings["infer"];
    let join = timings["infer_join"];
    if infer <= 0.0 {
        panic!("the model never ran, so the overlap was never exercised");
    }
    assert!(
        join < infer,
        "the residual join wait ({join:.3} ms) is not less than the inference \
         itself ({infer:.3} ms) -- the overlap is not happening"
    );
}

#[test]
fn the_frame_fits_the_budget() {
    let timings = timings();
    let total = timings["total"];
    let budget = 1000.0 / 50.0;
    assert!(
        total < budget,
        "mean frame {total:.2} ms against a {budget:.0} ms budget"
    );
}

/// The native share has not fallen.
///
/// A floor, with no ceiling -- see the module note. Raising it is the point of
/// the port, so a ceiling would fail precisely when the work succeeded.
#[test]
fn the_native_share_has_not_fallen() {
    let timings = timings();
    let native: f64 = NATIVE_STAGES.iter().map(|s| timings[*s]).sum();
    let host: f64 = HOST_STAGES.iter().map(|s| timings[*s]).sum();
    let accounted = native + host;
    assert!(accounted > 0.0);
    let native_share = native / accounted;
    assert!(
        native_share > 0.72,
        "native share is {:.1}% of accounted time (floor 72%). Either the \
         working resolution changed or a native call got skipped; both break \
         the comparison this port exists to support.",
        native_share * 100.0
    );
}
