//! The vehicle classifier, on its own thread.
//!
//! This module is where the concurrency story lives, and it is the part of the
//! application the porting brief singles out as most instructive.
//!
//! The shape is the same as the Python's: the pipeline thread submits a job
//! **before** it runs the background model, and joins the result **after** the
//! foreground mask has been cleaned and its blobs extracted, so the per-frame
//! cost is `max(background, inference)` rather than the sum.
//!
//! What changed
//! ------------
//! In Python that overlap is free and very nearly accidental. `session.run`
//! releases the GIL for its whole duration, so the interpreter lets the
//! pipeline thread run the background model meanwhile, and nobody had to design
//! anything.
//!
//! **There is no GIL here, so there is nothing to release and no accident.**
//! The overlap has to be built: a real worker thread, an explicit handoff of
//! the frame, and an explicit join. That is what the channel pair below is.
//!
//! Two consequences worth recording in a port report:
//!
//! * The handoff needs its own safety argument. In Python "the interpreter
//!   serialises us anyway" was the argument; here it is
//!   [`crate::pipeline::types::SharedMat`] -- the frame is immutable after
//!   publication, so sharing it read-only across the two threads is sound.
//! * The join is where `last_infer_ms` becomes safe to read. The Python
//!   documents that carefully and relies on the programmer honouring it; here
//!   the timing travels back *through* the channel with the result, so there is
//!   no field that can be read at the wrong moment.
//!
//! The residual wait is measured separately as `infer_join`. A large
//! `infer_join` does not mean inference is slow -- it means inference is slower
//! than the background model it is hiding behind, which is a scheduling result
//! rather than a model result, and the two get confused constantly.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use ndarray::{Array2, Array4};
use opencv::core::{Mat, Size};
use opencv::prelude::*;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;

use crate::config::Settings;
use crate::pipeline::types::SharedMat;

/// One finished inference: the likelihood map, and how long the call took.
pub struct Inference {
    pub likelihood: Array2<f32>,
    pub infer_ms: f64,
}

type Job = (Arc<SharedMat>, Sender<Result<Inference, String>>);

/// Working frame -> model input tensor.
///
/// **The single definition of this mapping.** A port must reproduce it exactly.
/// A different interpolation or channel order shifts every box by a fraction of
/// a cell, which survives into the measured speed and is very hard to trace
/// back from there.
///
/// The arithmetic is done in `f32` because the Python's is: `astype(np.float32)`
/// happens before the division, so the divide is a single-precision one. Doing
/// it in `f64` and narrowing at the end would give a different last bit on some
/// pixels.
pub fn preprocess(working: &Mat, height: i32, width: i32) -> Result<Array4<f32>, String> {
    let mut resized = Mat::default();
    opencv::imgproc::resize(
        working,
        &mut resized,
        Size::new(width, height),
        0.0,
        0.0,
        opencv::imgproc::INTER_AREA,
    )
    .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot resize for the model: ")))?;

    let mut rgb = Mat::default();
    opencv::imgproc::cvt_color_def(&resized, &mut rgb, opencv::imgproc::COLOR_BGR2RGB)
        .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot convert to RGB: ")))?;

    let bytes = rgb
        .data_bytes()
        .map_err(|e| format!("{}{e}", obfstr::obfstr!("the model input is not contiguous: ")))?;
    let (h, w) = (height as usize, width as usize);
    let mut tensor = Array4::<f32>::zeros((1, 3, h, w));
    for y in 0..h {
        for x in 0..w {
            let base = (y * w + x) * 3;
            for c in 0..3 {
                tensor[[0, c, y, x]] = bytes[base + c] as f32 / 255.0;
            }
        }
    }
    Ok(tensor)
}

/// Where the model file lives.
///
/// The Python resolves it next to its own source file, which a compiled binary
/// has no equivalent of. The search order below covers the three ways this ends
/// up deployed: an explicit override, next to the binary (what `cargo build`
/// and an installed image both give), and the repository layout during
/// development.
fn locate_model(file: &str) -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var(obfstr::obfstr!("KERBSIDE_MODEL_DIR")) {
        candidates.push(PathBuf::from(dir).join(file));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(file));
            candidates.push(dir.join("model").join(file));
        }
    }
    candidates.push(PathBuf::from("model").join(file));
    candidates.push(PathBuf::from(file));

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    let looked = candidates
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n  ");
    Err(format!(
        "{file}{}{looked}{}{file}{}",
        obfstr::obfstr!(" was not found. Looked in:\n  "),
        obfstr::obfstr!("\nSet KERBSIDE_MODEL_DIR, or build it in the Python repository \
                         with:\n    venv/bin/python tools/export_model.py\nand copy \
                         kerbside/detect/"),
        obfstr::obfstr!(" into this project's model/ directory."),
    ))
}

/// The platform's ONNX Runtime shared library.
const RUNTIME_LIBRARY: &str = if cfg!(target_os = "windows") {
    "onnxruntime.dll"
} else if cfg!(target_os = "macos") {
    "libonnxruntime.dylib"
} else {
    "libonnxruntime.so"
};

/// Find and open the ONNX Runtime shared library.
///
/// The runtime is loaded rather than linked (see BUILD.md), so something has to
/// say where it is. Leaving that entirely to `ORT_DYLIB_PATH` produces the worst
/// failure this program can have: a binary that builds, ships, and then dies at
/// startup inside the loader with a panic about a poisoned mutex, several frames
/// away from the actual problem.
///
/// So the search mirrors [`locate_model`]: explicit override, then next to the
/// executable, then whatever the platform's own loader can find. Anything not
/// found is reported as an ordinary error naming every path that was tried.
fn init_runtime() -> Result<(), String> {
    static READY: OnceLock<Result<(), String>> = OnceLock::new();
    READY.get_or_init(try_init_runtime).clone()
}

/// Which ONNX Runtime library was actually loaded, once one has been.
///
/// A benchmark record has to name it. Two runs that differ only in which
/// `libonnxruntime` they found are not comparable, and the file path is the
/// only thing that says which one it was -- the crate exposes no version
/// accessor.
static RESOLVED_RUNTIME: OnceLock<String> = OnceLock::new();

/// The ONNX Runtime library this process is using, if the detector has started.
pub fn resolved_runtime_path() -> Option<&'static str> {
    RESOLVED_RUNTIME.get().map(String::as_str)
}

/// Load the runtime, for callers that only want to report on it.
pub fn probe_runtime() -> Result<(), String> {
    init_runtime()
}

fn try_init_runtime() -> Result<(), String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(path) = std::env::var("ORT_DYLIB_PATH") {
        if !path.is_empty() {
            candidates.push(PathBuf::from(path));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(RUNTIME_LIBRARY));
            candidates.push(dir.join("lib").join(RUNTIME_LIBRARY));
        }
    }

    if let Some(path) = candidates.iter().find(|path| path.is_file()) {
        return ort::init_from(path)
            .map_err(|e| {
                format!(
                    "{} could not be loaded as ONNX Runtime: {e}\nIt must be a \
                     1.26-compatible build for this architecture.",
                    path.display()
                )
            })
            .map(|environment| {
                environment.commit();
                let _ = RESOLVED_RUNTIME.set(path.display().to_string());
            });
    }

    // Nothing on disk where we looked. Hand the bare library name to the
    // platform loader before giving up: an `apt`-installed or ldconfig-
    // registered runtime is a perfectly good deployment, and it lives on a
    // search path this process has no business reimplementing.
    if ort::init_from(RUNTIME_LIBRARY)
        .map(|environment| {
            environment.commit();
        })
        .is_ok()
    {
        let _ = RESOLVED_RUNTIME.set(format!("{RUNTIME_LIBRARY}{}", obfstr::obfstr!(" (system library path)")));
        return Ok(());
    }

    // The remedy depends on the platform, so do not print the other one's.
    let remedy = if cfg!(target_os = "windows") {
        obfstr::obfstr!("Rebuild with the environment script, which copies it next to the binary:\n\
         \x20   . .\\scripts\\env-windows.ps1\n\x20   cargo build --release\n\
         Or set ORT_DYLIB_PATH to an ONNX Runtime 1.26 build.").to_string()
    } else {
        obfstr::obfstr!("Fetch the runtime for this architecture and point at it, for example:\n\
         \x20   curl -L -o ort.tgz https://github.com/microsoft/onnxruntime/releases/download/v1.26.0/onnxruntime-linux-aarch64-1.26.0.tgz\n\
         \x20   tar xf ort.tgz\n\
         \x20   export ORT_DYLIB_PATH=\"$PWD/onnxruntime-linux-aarch64-1.26.0/lib/libonnxruntime.so\"\n\
         Or copy that file next to this binary.").to_string()
    };
        let looked = candidates
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n  ");
    Err(format!(
        "{RUNTIME_LIBRARY}{}{looked}{}{remedy}{}",
        obfstr::obfstr!(" was not found. Looked in:\n  "),
        obfstr::obfstr!("\n  and on the system library search path.\n"),
        obfstr::obfstr!("\nSee BUILD.md."),
    ))
}

/// Owns the ONNX session and the worker thread.
pub struct Detector {
    jobs: Option<Sender<Job>>,
    worker: Option<JoinHandle<()>>,
}

impl Detector {
    pub fn new(settings: &Settings) -> Result<Self, String> {
        // Before anything touches `ort`: loading the runtime lazily inside the
        // session builder turns a missing library into a panic in a foreign
        // crate, and this is the one place that can still report it as an error.
        init_runtime()?;

        let mdl = &settings.model;
        let path = locate_model(&mdl.FILE)?;

        // Single-threaded and sequential, deliberately. Letting the runtime take
        // every core would make this one call faster and the frame slower: it
        // would preempt the pipeline thread running the background model, which
        // is the very work this call is supposed to be hiding behind.
        let session = Session::builder()
            .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot configure ONNX Runtime: ")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot set the optimisation level: ")))?
            .with_intra_threads(mdl.ORT_INTRA_THREADS as usize)
            .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot set intra-op threads: ")))?
            .with_inter_threads(mdl.ORT_INTER_THREADS as usize)
            .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot set inter-op threads: ")))?
            .with_parallel_execution(false)
            .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot select sequential execution: ")))?
            .commit_from_file(&path)
            .map_err(|e| format!("{}{}: {e}", obfstr::obfstr!("cannot load "), path.display()))?;

        let (tx, rx) = mpsc::channel::<Job>();
        let height = mdl.INPUT_HEIGHT;
        let width = mdl.INPUT_WIDTH;

        // The worker owns the session outright. That is stricter than the
        // Python, where any thread could in principle call `session.run`, and it
        // is free: only one thread ever needed it.
        let worker = std::thread::Builder::new()
            .name(obfstr::obfstr!("detector").into())
            .spawn(move || {
                let mut session = session;
                while let Ok((frame, reply)) = rx.recv() {
                    let outcome = run_once(&mut session, frame.get(), height, width);
                    // A closed reply channel means the pipeline gave up on this
                    // frame. Dropping the result is correct; failing is not.
                    let _ = reply.send(outcome);
                }
            })
            .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot start the detector thread: ")))?;

        // The input geometry is captured by the worker closure above, not kept
        // here: only the worker ever needs it, and a second copy on this struct
        // would be a second thing to keep in step with the settings.
        Ok(Self {
            jobs: Some(tx),
            worker: Some(worker),
        })
    }

    /// Whether to infer on this frame.
    ///
    /// The background model already says *where* something is; the model says
    /// *what*, and confirms a blob is a vehicle rather than a shadow or a bag.
    /// That is a far weaker demand on latency, so it runs on a duty cycle and
    /// its likelihood map is carried forward in between -- the scene changes
    /// slowly compared with the frame rate, and a map four frames old still
    /// describes the road correctly.
    pub fn should_run(&self, frame_id: i64, settings: &Settings) -> bool {
        if !settings.model.ENABLED {
            return false;
        }
        frame_id % i64::max(1, crate::tuning::infer_every_n_frames()) == 0
    }

    /// Queue one inference. Returns immediately.
    ///
    /// The working frame is shared by reference and is never mutated after
    /// publication, so the worker touches no state the pipeline thread might
    /// change underneath it.
    pub fn submit(&self, working: Arc<SharedMat>) -> Result<Receiver<Result<Inference, String>>, String> {
        let (tx, rx) = mpsc::channel();
        self.jobs
            .as_ref()
            .ok_or_else(|| obfstr::obfstr!("the detector has been shut down").to_string())?
            .send((working, tx))
            .map_err(|_| obfstr::obfstr!("the detector thread has stopped").to_string())?;
        Ok(rx)
    }

    pub fn close(&mut self) {
        // Dropping the sender is what ends the worker's `recv` loop.
        self.jobs = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Detector {
    fn drop(&mut self) {
        self.close();
    }
}

fn run_once(
    session: &mut Session,
    working: &Mat,
    height: i32,
    width: i32,
) -> Result<Inference, String> {
    let tensor = preprocess(working, height, width)?;
    let began = Instant::now();
    let input = Tensor::from_array(tensor).map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot build the input: ")))?;
    let outputs = session
        .run(ort::inputs!["input" => input])
        .map_err(|e| format!("{}{e}", obfstr::obfstr!("inference failed: ")))?;
    let infer_ms = began.elapsed().as_secs_f64() * 1000.0;

    let view = outputs["likelihood"]
        .try_extract_array::<f32>()
        .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot read the likelihood map: ")))?;
    let shape = view.shape().to_vec();
    if shape.len() != 4 {
        return Err(format!("{}{shape:?}", obfstr::obfstr!("expected a 4-D likelihood map, got shape ")));
    }
    // `likelihood[0, 0]` -- one image, one channel.
    let grid = view
        .into_dimensionality::<ndarray::Ix4>()
        .map_err(|e| format!("{}{e}", obfstr::obfstr!("unexpected likelihood shape: ")))?;
    let map = grid.slice(ndarray::s![0, 0, .., ..]).to_owned();
    Ok(Inference {
        likelihood: map,
        infer_ms,
    })
}
