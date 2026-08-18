//! Entry point.
//!
//! ```text
//!     kerbside --replay              deterministic, every frame
//!     kerbside --realtime --perf     paced, drops frames, measured
//! ```
//!
//! Replay is the default because the deterministic mode is the one that answers
//! a question. Realtime exists to be benchmarked, and its output is not
//! reproducible by construction -- which frames get dropped depends on the
//! machine.
//!
//! Porting note: `--gc-stats`
//! --------------------------
//! The Python's `--gc-stats` installs a callback on CPython's collector and
//! reports how long each collection took, because those pauses are its worst
//! latency events. **This build has no tracing collector, so there is nothing
//! to instrument and the flag reports exactly that.** It is kept, rather than
//! removed, because "the pauses are gone" is the headline result of the port
//! and a missing flag would look like an oversight rather than a finding.

use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use kerbside::config::{self, Settings};
use kerbside::consumers::{ConsumerChain, Consumer, FanOut};
use kerbside::measure::build_homography;
use kerbside::output::{OverlayWriter, ResultWriter};
use kerbside::perf;
use kerbside::pipeline::types::SharedMat;
use kerbside::pipeline::{live_settings, Pipeline, RawFrame, RunningPipeline};
use kerbside::source::RoadScene;

const USAGE: &str = "\
kerbside -- a roadside speed-enforcement camera

    --replay            process every frame, in order, as fast as possible (default)
    --realtime          pace the source at the configured rate and drop frames when behind
    --profile NAME      named settings profile (bench, replay, test)
    --frames N          override clip length
    --seed N            override scene seed
    --limit KPH         speed limit, km/h
    --out PATH          result CSV path (default telemetry/results.csv)
    --overlay PATH      also write an overlay mp4
    --perf              enable per-frame stage timing (tier 2)
    --perf-dir DIR      directory for the per-frame perf CSV
    --gc-stats          report collector pauses (see the note in main.rs)
    --threaded          run replay through the pipeline thread rather than inline
    --dump-settings     print settings and exit
    --version           print build and native-library versions, and exit
";

#[derive(Default)]
struct Args {
    replay: bool,
    realtime: bool,
    profile: Option<String>,
    frames: Option<i64>,
    seed: Option<i64>,
    limit: Option<f64>,
    out: String,
    overlay: Option<String>,
    perf: bool,
    perf_dir: Option<String>,
    gc_stats: bool,
    threaded: bool,
    dump_settings: bool,
    version: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        out: obfstr::obfstr!("telemetry/results.csv").to_string(),
        ..Default::default()
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{arg}{}", obfstr::obfstr!(" expects a value")));
        match arg.as_str() {
            "--replay" => args.replay = true,
            "--realtime" => args.realtime = true,
            "--profile" => args.profile = Some(value()?),
            "--frames" => args.frames = Some(value()?.parse().map_err(|e| format!("{}{e}", obfstr::obfstr!("--frames: ")))?),
            "--seed" => args.seed = Some(value()?.parse().map_err(|e| format!("{}{e}", obfstr::obfstr!("--seed: ")))?),
            "--limit" => args.limit = Some(value()?.parse().map_err(|e| format!("{}{e}", obfstr::obfstr!("--limit: ")))?),
            "--out" => args.out = value()?,
            "--overlay" => args.overlay = Some(value()?),
            "--perf" => args.perf = true,
            "--perf-dir" => args.perf_dir = Some(value()?),
            "--gc-stats" => args.gc_stats = true,
            "--threaded" => args.threaded = true,
            "--dump-settings" => args.dump_settings = true,
            "--version" | "-V" => args.version = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("{}{other}\n\n{USAGE}", obfstr::obfstr!("unknown argument: "))),
        }
    }
    if args.replay && args.realtime {
        return Err("--replay and --realtime are mutually exclusive".into());
    }
    Ok(args)
}

/// Fix everything that would otherwise be sized from the host machine.
///
/// Must run before any OpenCV call that matters. See `OPENCV_THREADS` in
/// `config/telemetry.rs` -- an unpinned thread pool makes two runs on two
/// machines incomparable, which defeats the purpose of this program.
fn pin_runtime(settings: &Settings) -> Result<(), String> {
    opencv::core::set_num_threads(settings.telemetry.OPENCV_THREADS)
        .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot pin the OpenCV thread pool: ")))
}

/// Timestamps come from the frame index, not the clock.
///
/// A wall-clock timestamp would make the run irreproducible for no benefit --
/// and would put the measured speed at the mercy of scheduling jitter, which is
/// not a property anyone wants in a device that issues fines.
fn frame_for(scene: &RoadScene, settings: &Settings, frame_id: i64) -> Result<RawFrame, String> {
    let (image, _truth) = scene.render(frame_id)?;
    Ok(RawFrame::new(
        frame_id,
        SharedMat::new(image),
        frame_id as f64 / settings.video.FPS as f64,
    ))
}

/// What this binary is, and what it will actually call into.
///
/// The Python's benchmark script prints the interpreter and library versions
/// before it records anything, because a stage table is meaningless without
/// them. This is the equivalent, and it reports the *resolved* ONNX Runtime
/// path rather than a version string: the crate exposes no version accessor,
/// and which file was loaded is the thing that actually decides the numbers.
fn print_version() {
    println!("kerbside {} ({})", env!("CARGO_PKG_VERSION"), build_profile());
    match opencv::core::get_version_string() {
        Ok(version) => println!("opencv       {version} (thread pool pinned by settings)"),
        Err(e) => println!("opencv       unavailable: {e}"),
    }
    match kerbside::detect::probe_runtime() {
        Ok(()) => println!(
            "onnxruntime  {}",
            kerbside::detect::resolved_runtime_path().unwrap_or(obfstr::obfstr!("loaded, path unknown"))
        ),
        Err(e) => println!("onnxruntime  NOT LOADED
{e}"),
    }
}

fn build_profile() -> &'static str {
    // A debug build is several times slower and must never be benchmarked; the
    // bench scripts refuse to record one, and this is how they can tell.
    if cfg!(debug_assertions) {
        "debug -- NOT valid for benchmarking"
    } else {
        "release"
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;

    if args.version {
        print_version();
        return Ok(());
    }

    // Typed overrides: each closure assigns a field through its compile-time
    // offset, naming no string. This is what lets a `dist` build with no name
    // table still honour the CLI flags. See config/mod.rs.
    let mut overrides: Vec<config::Override> = Vec::new();
    if let Some(frames) = args.frames {
        overrides.push(Box::new(move |s| s.video.SCENE_FRAMES = frames));
    }
    if let Some(seed) = args.seed {
        overrides.push(Box::new(move |s| s.video.SCENE_SEED = seed));
    }
    if let Some(limit) = args.limit {
        overrides.push(Box::new(move |s| s.enforcement.SPEED_LIMIT_KPH = limit));
    }
    if args.perf {
        overrides.push(Box::new(|s| s.telemetry.MEASURE_STAGES = true));
    }
    if let Some(dir) = &args.perf_dir {
        let dir = dir.clone();
        overrides.push(Box::new(move |s| s.telemetry.PERF_DIR = dir.clone()));
    }
    if args.overlay.is_some() {
        overrides.push(Box::new(|s| s.telemetry.WRITE_OVERLAY = true));
    }

    let settings = config::initialize(config::resolve(args.profile.as_deref(), overrides, false)?);
    pin_runtime(&settings)?;

    if args.dump_settings {
        // The dump prints every setting *by name*, so it exists only where the
        // name table does. A `dist` build removed that table on purpose.
        #[cfg(feature = "introspection")]
        {
            println!("{}", config::format_dump(&settings));
            return Ok(());
        }
        #[cfg(not(feature = "introspection"))]
        {
            return Err("--dump-settings is unavailable in this build: it was \
                        compiled without the introspection feature, which is what \
                        keeps the settings field names out of the binary"
                .into());
        }
    }

    let realtime = args.realtime;
    perf::configure(
        settings.telemetry.MEASURE_STAGES,
        &settings.telemetry.PERF_DIR,
        settings.telemetry.PERF_FLUSH_MS,
        if realtime { "realtime" } else { "replay" },
    )?;

    let scene = RoadScene::new(&settings)?;
    let total = i64::min(scene.frame_count(), settings.video.SCENE_FRAMES);

    let mut sinks: Vec<Box<dyn Consumer + Send>> = Vec::new();
    sinks.push(Box::new(ResultWriter::new(&args.out)?));
    if let Some(path) = &args.overlay {
        sinks.push(Box::new(OverlayWriter::new(
            &settings,
            path,
            &build_homography(&settings)?,
        )?));
    }

    let consumers = ConsumerChain::new(&settings, Some(FanOut::new(sinks)));
    let pipeline = Pipeline::new(
        &settings,
        consumers,
        // The live pull, not a captured snapshot: a runtime `apply()` must be
        // visible to the next frame, and handing over `settings` here would
        // defeat the whole volatility mechanism.
        live_settings(),
        !realtime,
    )?;

    let began = Instant::now();
    let mut pipeline = if realtime {
        run_realtime(pipeline, &scene, &settings, total)?
    } else if args.threaded {
        run_replay_threaded(pipeline, &scene, &settings, total)?
    } else {
        let mut pipeline = pipeline;
        for frame_id in 0..total {
            pipeline.process_one(frame_for(&scene, &settings, frame_id)?)?;
        }
        pipeline
    };
    let wall = began.elapsed().as_secs_f64();

    let ring_frames = pipeline.consumers.ring.len();
    let ring_containers = pipeline.consumers.ring.tracked_containers();

    // Close the sinks in order, then report. Taking the chain apart here is the
    // equivalent of the Python's `finally` block: the digest is only complete
    // once the writer has been closed.
    let summary = match pipeline.consumers.sink_mut() {
        Some(sink) => sink.finish()?,
        None => None,
    };
    let (path, rows, violations, digest) = match summary {
        Some(s) => (s.path, s.rows, s.violations, s.digest),
        None => (args.out.clone(), 0, 0, String::new()),
    };

    println!("{}", perf::shutdown());
    println!("wall {wall:.2} s  ({:.1} fps effective)", total as f64 / wall);
    println!("results {path}  rows {rows}  violations {violations}");
    println!("sha256 {digest}");
    // Deliberately *not* called "tracked containers" like the Python's line.
    // The Python counts GC-tracked dicts, lists and tuples because those are
    // what its collector walks -- including one tuple per contour point. This
    // counts heap allocations, and a contour lives in a single `Vec`. The two
    // numbers measure the same retained data through different lenses, and
    // printing them under the same label would invite a comparison that means
    // nothing. What is comparable is the frame count and the fact that both
    // hold the frames by reference.
    println!(
        "ring retains {ring_frames} frames, ~{} retained allocations",
        thousands(ring_containers as u64)
    );
    if args.gc_stats {
        report_gc();
    }
    Ok(())
}

/// Same frames, same order, but through the mailbox and worker thread.
///
/// Proves the threaded path produces identical results to the inline one -- if
/// it does not, the pipeline depends on scheduling and every benchmark taken
/// with it is comparing schedules rather than implementations.
fn run_replay_threaded(
    pipeline: Pipeline,
    scene: &RoadScene,
    settings: &Settings,
    total: i64,
) -> Result<Pipeline, String> {
    let running = RunningPipeline::start(pipeline)?;
    for frame_id in 0..total {
        running.mailbox().post(frame_for(scene, settings, frame_id)?);
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if running.last_frame_id() >= total - 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    running.stop()
}

/// Pace the source at the configured rate; the mailbox drops what it must.
fn run_realtime(
    pipeline: Pipeline,
    scene: &RoadScene,
    settings: &Settings,
    total: i64,
) -> Result<Pipeline, String> {
    let running = RunningPipeline::start(pipeline)?;
    let interval = Duration::from_secs_f64(1.0 / settings.video.FPS as f64);
    let started = Instant::now();
    for frame_id in 0..total {
        let target = started + interval.mul_f64(frame_id as f64);
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        }
        running.mailbox().post(frame_for(scene, settings, frame_id)?);
    }
    std::thread::sleep(Duration::from_millis(250));
    running.stop()
}

/// The Python reports collector pause counts and lengths here.
///
/// There is no tracing collector in this build. Memory from the evidence record
/// and the ring is released when the ring evicts a frame -- on the pipeline
/// thread, at a point the program chooses, in bounded time. That is the whole
/// finding, so it is stated rather than silently omitted.
fn report_gc() {
    println!("gc: no tracing collector in this build");
    println!(
        "  the evidence record and the ring allocate exactly as the Python's do; \
         what is gone is the collection pass over them"
    );
    let counters = perf::counters();
    println!(
        "  worst frame {:.3} ms over {} frames -- compare against the Python's \
         gen2 pause distribution",
        counters.max_ms(),
        counters.frames.load(Ordering::Relaxed)
    );
}

/// Thousands separators, the way Python's `{:,}` renders them.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("kerbside: {message}");
            ExitCode::FAILURE
        }
    }
}
