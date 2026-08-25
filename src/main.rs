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
#[cfg(feature = "overlay")]
use kerbside::measure::build_homography;
use kerbside::output::ResultWriter;
#[cfg(feature = "overlay")]
use kerbside::output::OverlayWriter;
use kerbside::perf;
use kerbside::pipeline::types::SharedMat;
use kerbside::pipeline::{live_settings, Pipeline, RawFrame, RunningPipeline};
use kerbside::source::RoadScene;

// Was `const USAGE: &str`; a const cannot hold an obfstr! value (it decodes at
// run time), so this is a function and the help text ships only as ciphertext.
//
// The whole help text is `introspection`-gated: a dist build has no `--help`
// and no unknown-argument hint, so this large descriptive string is not linked
// at all. The CLI x-ray the reverse-engineering reports built came straight from
// running `--help`.
#[cfg(feature = "introspection")]
fn usage() -> String {
    obfstr::obfstr!("\
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
").to_string()
}

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
    #[cfg_attr(not(feature = "introspection"), allow(dead_code))]
    gc_stats: bool,
    threaded: bool,
    dump_settings: bool,
    // Only read by the introspection-gated `--version` handler; in a dist build
    // the flag does not exist and the field is never read.
    #[cfg_attr(not(feature = "introspection"), allow(dead_code))]
    version: bool,
}

/// The unknown-argument error. In dev/release it names the argument and prints
/// the help; a dist build has neither the help nor the message, so it returns an
/// empty error -- the process still exits non-zero, saying nothing.
#[cfg(feature = "introspection")]
fn unknown_arg(a: &str) -> String {
    format!("{}{a}\n\n{}", obfstr::obfstr!("unknown argument: "), usage())
}
#[cfg(not(feature = "introspection"))]
fn unknown_arg(_a: &str) -> String {
    String::new()
}

fn parse_args() -> Result<Args, String> {
    // A dist build writes no CSV, so it needs no default path and `--out` is not
    // accepted -- the string, and the flag, are absent from the shipped binary.
    #[cfg(feature = "introspection")]
    let default_out = obfstr::obfstr!("telemetry/results.csv").to_string();
    #[cfg(not(feature = "introspection"))]
    let default_out = String::new();
    let mut args = Args {
        out: default_out,
        ..Default::default()
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{arg}{}", kerbside::obfstr_err!(" expects a value")));
        // if/else with obfstr comparisons rather than a `match` on literals:
        // a match pattern must be a plain literal, so its text would ship in
        // .rodata; comparing against an obfstr! value keeps the flag names out.
        let a = arg.as_str();
        let mut handled = true;
        // Core flags -- everything the oracle path needs. Always present.
        if a == obfstr::obfstr!("--replay") {
            args.replay = true;
        } else if a == obfstr::obfstr!("--frames") {
            args.frames = Some(value()?.parse().map_err(|e| format!("{}{e}", kerbside::obfstr_err!("--frames: ")))?);
        } else if a == obfstr::obfstr!("--seed") {
            args.seed = Some(value()?.parse().map_err(|e| format!("{}{e}", kerbside::obfstr_err!("--seed: ")))?);
        } else if a == obfstr::obfstr!("--limit") {
            args.limit = Some(value()?.parse().map_err(|e| format!("{}{e}", kerbside::obfstr_err!("--limit: ")))?);
        } else {
            handled = false;
        }
        // Diagnostic / introspection flags. A dist build drops this whole block,
        // so their names, the help text and the version text never enter the
        // shipped binary; none is needed to run the oracle. In dist an unknown
        // flag falls through to the (silent) unknown-argument path below.
        #[cfg(feature = "introspection")]
        if !handled {
            handled = true;
            if a == obfstr::obfstr!("--realtime") {
                args.realtime = true;
            } else if a == obfstr::obfstr!("--profile") {
                args.profile = Some(value()?);
            } else if a == obfstr::obfstr!("--out") {
                args.out = value()?;
            } else if a == obfstr::obfstr!("--overlay") {
                args.overlay = Some(value()?);
            } else if a == obfstr::obfstr!("--perf") {
                args.perf = true;
            } else if a == obfstr::obfstr!("--perf-dir") {
                args.perf_dir = Some(value()?);
            } else if a == obfstr::obfstr!("--gc-stats") {
                args.gc_stats = true;
            } else if a == obfstr::obfstr!("--threaded") {
                args.threaded = true;
            } else if a == obfstr::obfstr!("--dump-settings") {
                args.dump_settings = true;
            } else if a == obfstr::obfstr!("--version") || a == obfstr::obfstr!("-V") {
                args.version = true;
            } else if a == obfstr::obfstr!("-h") || a == obfstr::obfstr!("--help") {
                println!("{}", usage());
                std::process::exit(0);
            } else {
                handled = false;
            }
        }
        if !handled {
            return Err(unknown_arg(a));
        }
    }
    if args.replay && args.realtime {
        return Err(kerbside::obfstr_err!("--replay and --realtime are mutually exclusive").into());
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
        .map_err(|e| format!("{}{e}", kerbside::obfstr_err!("cannot pin the OpenCV thread pool: ")))
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
///
/// `introspection`-gated: `--version` is a diagnostic flag a dist build drops,
/// so none of this text (build profile, library banners, the resolved runtime
/// path) is linked into the shipped binary. The reports used `--version` to
/// fingerprint the build.
#[cfg(feature = "introspection")]
fn print_version() {
    println!("{}{} ({})", obfstr::obfstr!("kerbside "), env!("CARGO_PKG_VERSION"), build_profile());
    match opencv::core::get_version_string() {
        Ok(version) => println!("{}{version}{}", obfstr::obfstr!("opencv       "), obfstr::obfstr!(" (thread pool pinned by settings)")),
        Err(e) => println!("{}{e}", obfstr::obfstr!("opencv       unavailable: ")),
    }
    match kerbside::detect::probe_runtime() {
        Ok(()) => println!(
            "{}{}",
            obfstr::obfstr!("onnxruntime  "),
            kerbside::detect::resolved_runtime_path().unwrap_or(obfstr::obfstr!("loaded, path unknown"))
        ),
        Err(e) => println!("{}{e}", obfstr::obfstr!("onnxruntime  NOT LOADED\n")),
    }
}

#[cfg(feature = "introspection")]
fn build_profile() -> String {
    // A debug build is several times slower and must never be benchmarked; the
    // bench scripts refuse to record one, and this is how they can tell.
    if cfg!(debug_assertions) {
        obfstr::obfstr!("debug -- NOT valid for benchmarking").to_string()
    } else {
        obfstr::obfstr!("release").to_string()
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;

    // `--version` only exists in an introspection build; in dist `args.version`
    // is never set and this whole block is compiled out with `print_version`.
    #[cfg(feature = "introspection")]
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
            return Err(kerbside::obfstr_err!("--dump-settings is unavailable in this build: it was \
                        compiled without the introspection feature, which is what \
                        keeps the settings field names out of the binary")
                .into());
        }
    }

    let realtime = args.realtime;
    // Bound to an owned String: an obfstr! result borrows a stack temporary that
    // is dropped at the end of its `{ }` arm, so it cannot be passed inline.
    let run_name = if realtime {
        kerbside::obfstr_err!("realtime").to_string()
    } else {
        kerbside::obfstr_err!("replay").to_string()
    };
    perf::configure(
        settings.telemetry.MEASURE_STAGES,
        &settings.telemetry.PERF_DIR,
        settings.telemetry.PERF_FLUSH_MS,
        &run_name,
    )?;

    let scene = RoadScene::new(&settings)?;
    let total = i64::min(scene.frame_count(), settings.video.SCENE_FRAMES);

    let mut sinks: Vec<Box<dyn Consumer + Send>> = Vec::new();
    sinks.push(Box::new(ResultWriter::new(&args.out)?));
    #[cfg(feature = "overlay")]
    if let Some(path) = &args.overlay {
        sinks.push(Box::new(OverlayWriter::new(
            &settings,
            path,
            &build_homography(&settings)?,
        )?));
    }
    // A build without the `overlay` feature still parses `--overlay` (the flag is
    // not feature-gated), so refuse it loudly here. The dist build drops overlay
    // -- videoio/imgcodecs are not linked so the OpenCV video backend is absent --
    // and without this it would accept `--overlay` and silently write no file.
    #[cfg(not(feature = "overlay"))]
    if args.overlay.is_some() {
        return Err(kerbside::obfstr_err!("--overlay is unavailable in this build: it was compiled \
                    without the overlay feature (opencv videoio/imgcodecs). The dist build drops \
                    it so the video backend is not linked; use a release build to record an overlay")
            .into());
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

    // The perf writer thread is stopped here regardless; only the *printing* of
    // the diagnostic summary is introspection-gated. A dist build emits exactly
    // one line -- the sha256 fingerprint -- and no diagnostics at all: not the
    // labels, and not the orphaned numbers a bare string-cut would leave behind.
    let perf_summary = perf::shutdown();
    #[cfg(feature = "introspection")]
    {
        println!("{perf_summary}");
        println!("{}{wall:.2}{}{:.1}{}", obfstr::obfstr!("wall "), obfstr::obfstr!(" s  ("), total as f64 / wall, obfstr::obfstr!(" fps effective)"));
        println!("{}{path}{}{rows}{}{violations}", obfstr::obfstr!("results "), obfstr::obfstr!("  rows "), obfstr::obfstr!("  violations "));
    }
    println!("{}{digest}", obfstr::obfstr!("sha256 "));
    // Deliberately *not* called "tracked containers" like the Python's line.
    // The Python counts GC-tracked dicts, lists and tuples because those are
    // what its collector walks -- including one tuple per contour point. This
    // counts heap allocations, and a contour lives in a single `Vec`. The two
    // numbers measure the same retained data through different lenses, and
    // printing them under the same label would invite a comparison that means
    // nothing. What is comparable is the frame count and the fact that both
    // hold the frames by reference.
    #[cfg(feature = "introspection")]
    println!(
        "{}{ring_frames}{}{}{}",
        obfstr::obfstr!("ring retains "),
        obfstr::obfstr!(" frames, ~"),
        thousands(ring_containers as u64),
        obfstr::obfstr!(" retained allocations")
    );
    #[cfg(not(feature = "introspection"))]
    let _ = (&perf_summary, &path, rows, violations, wall, ring_frames, ring_containers);
    // `--gc-stats` exists only in an introspection build, so a dist build never
    // reaches `report_gc` and it is compiled out with its `println!`s.
    #[cfg(feature = "introspection")]
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
///
/// `introspection`-gated with `--gc-stats`, so a dist build carries neither the
/// call nor these strings.
#[cfg(feature = "introspection")]
fn report_gc() {
    println!("{}", kerbside::obfstr_err!("gc: no tracing collector in this build"));
    println!(
        "{}",
        kerbside::obfstr_err!("  the evidence record and the ring allocate exactly as the Python's do; \
         what is gone is the collection pass over them")
    );
    let counters = perf::counters();
    println!(
        "{}{:.3}{}{}{}",
        kerbside::obfstr_err!("  worst frame "),
        counters.max_ms(),
        kerbside::obfstr_err!(" ms over "),
        counters.frames.load(Ordering::Relaxed),
        kerbside::obfstr_err!(" frames -- compare against the Python's gen2 pause distribution")
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

/// Anti-tamper for the shipped `dist` build (feature `anti-tamper`; off in
/// dev/release/tests, which stay freely debuggable).
///
/// `PR_SET_DUMPABLE = 0` makes the process non-dumpable, so a non-root user can
/// no longer `ptrace`-attach or `gcore` it -- the cheap method every RE report
/// here relied on (core-dumping the live process and reading the settings
/// struct, the OpenCV objects and the decoded strings straight out of RAM). The
/// `TracerPid` check refuses to run under a debugger attached at start. A
/// **root** owner of the device defeats both: this raises the cost from a bare
/// `gcore` to "need root and defeat anti-ptrace", it is not a barrier.
#[cfg(feature = "anti-tamper")]
fn harden() {
    // Assemble the anti-emulation decode key from an environment probe, before
    // any constant is decoded. On real hardware this yields the true key; under
    // an emulator that stubs the probe (or never runs start-up) it comes out
    // wrong, so every `encf!`/`enci!` decodes to garbage. See `crypt::keying`.
    kerbside::crypt::init_keying();
    // PR_SET_DUMPABLE (4) = 0 (SUID_DUMP_DISABLE): drop dumpability so a non-root
    // ptrace/gcore of this process is denied by the kernel.
    extern "C" {
        fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
    }
    unsafe {
        prctl(4, 0, 0, 0, 0);
        // Read PR_SET_DUMPABLE back (PR_GET_DUMPABLE = 3). The observed bypass is
        // an LD_PRELOAD shim that no-ops `prctl(PR_SET_DUMPABLE)` so the process
        // stays dumpable for `gcore`/`/proc/PID/mem`. That shim leaves the real
        // dumpable flag at 1, so if the read-back is not 0 the set did not take --
        // refuse, quietly. A shim that also hooks PR_GET_DUMPABLE defeats this; it
        // is a cost step against exactly the hook seen, not a wall.
        if prctl(3, 0, 0, 0, 0) != 0 {
            std::process::exit(1);
        }
    }
    // Already traced at start? Refuse -- quietly, with no anti-debug banner to
    // steer around.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("TracerPid:") {
                if rest.trim() != "0" {
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(not(feature = "anti-tamper"))]
#[inline]
fn harden() {}

fn main() -> ExitCode {
    harden();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{}{message}", kerbside::obfstr_err!("kerbside: "));
            ExitCode::FAILURE
        }
    }
}
