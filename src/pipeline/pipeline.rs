//! The pipeline thread: one frame in, one `FrameOutput` out.
//!
//! Everything in [`Pipeline::process_frame`] runs as a single synchronous call
//! stack on one thread, and that call stack is the frame budget. The only
//! concurrency is the classifier job, submitted before the background model and
//! joined after the blobs have been extracted.
//!
//! Two threads, one mailbox
//! ------------------------
//! The source thread posts frames into a **one-slot mailbox**. If the pipeline
//! has not taken the previous frame, the new one overwrites it and a counter
//! goes up. A queue would trade a visible dropped-frame count for an invisible
//! and unbounded latency debt -- and for a device that timestamps evidence, a
//! stale reading delivered on time is not merely useless, it is indefensible.
//!
//! `--replay` flips the mailbox into lockstep: the source blocks until the
//! pipeline has taken the previous frame. Every frame is then processed, in
//! order, and the run is reproducible. Realtime and replay cannot both be true,
//! and the difference between them is the difference between measuring the
//! answer and measuring the machine.
//!
//! Settings
//! --------
//! `(self.settings_provider)()` is called **exactly once per frame**, on the
//! line after the timestamp, and the result is passed down by reference.
//! Nothing deeper calls it -- and here nothing deeper *can*, because what it
//! receives is a `&Settings` borrowed from the pipeline's own `Arc` snapshot.
//!
//! Porting note
//! ------------
//! The Python's `Pipeline` is one object that the worker thread mutates while
//! the main thread reads `last_output` off it. That is safe there only because
//! of the interpreter lock. Here the pipeline is *moved into* the worker thread
//! and handed back when it stops, so there is no shared mutable object at all;
//! progress is published separately through an atomic. Threading this out was
//! the single most invasive change the port needed, and it is the one to budget
//! for on a real codebase.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ndarray::Array2;

use crate::config::{self, Settings};
use crate::consumers::{ConsumerChain, FrameOutput};
use crate::detect::{score_blobs, Detector};
use crate::enforce::EnforcementGate;
use crate::measure::SpeedMeasurer;
use crate::perf;
use crate::track::Associator;
use crate::tuning;

use super::background::{to_working, BackgroundModel};
use super::blobs::BlobFinder;
use super::types::{FrameResult, RawFrame, SharedMat};

/// One-slot handoff between the source and the pipeline.
pub struct Mailbox {
    slot: Mutex<Slot>,
    condition: Condvar,
    lockstep: bool,
    pub dropped: AtomicU64,
}

struct Slot {
    frame: Option<RawFrame>,
    closed: bool,
}

impl Mailbox {
    pub fn new(lockstep: bool) -> Self {
        Self {
            slot: Mutex::new(Slot {
                frame: None,
                closed: false,
            }),
            condition: Condvar::new(),
            lockstep,
            dropped: AtomicU64::new(0),
        }
    }

    pub fn post(&self, frame: RawFrame) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if self.lockstep {
            while slot.frame.is_some() && !slot.closed {
                slot = self
                    .condition
                    .wait(slot)
                    .unwrap_or_else(|e| e.into_inner());
            }
        } else if slot.frame.is_some() {
            // Drop to latest. Count it -- an uncounted drop is a lie.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        slot.frame = Some(frame);
        drop(slot);
        self.condition.notify_all();
    }

    pub fn take(&self, timeout: Duration) -> Option<RawFrame> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        while slot.frame.is_none() && !slot.closed {
            let (guard, timed_out) = self
                .condition
                .wait_timeout(slot, timeout)
                .unwrap_or_else(|e| e.into_inner());
            slot = guard;
            if timed_out.timed_out() && slot.frame.is_none() {
                return None;
            }
        }
        let frame = slot.frame.take();
        drop(slot);
        self.condition.notify_all();
        frame
    }

    pub fn close(&self) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.closed = true;
        drop(slot);
        self.condition.notify_all();
    }
}

/// How the pipeline gets its settings. `config::current` in production; a
/// fixed snapshot in tests, which is why it is a parameter at all.
pub type SettingsProvider = Box<dyn Fn() -> Arc<Settings> + Send>;

pub struct Pipeline {
    pub consumers: ConsumerChain,
    settings_provider: SettingsProvider,
    budget_ms: f64,
    work_w: i32,
    work_h: i32,
    background: BackgroundModel,
    blob_finder: BlobFinder,
    detector: Option<Detector>,
    associator: Associator,
    measurer: SpeedMeasurer,
    gate: EnforcementGate,
    warm_frames: i64,
    likelihood: Option<Array2<f32>>,
    last_frame_id: Arc<AtomicI64>,
    mailbox: Arc<Mailbox>,
}

impl Pipeline {
    pub fn new(
        settings: &Settings,
        consumers: ConsumerChain,
        settings_provider: SettingsProvider,
        lockstep: bool,
    ) -> Result<Self, String> {
        // Construction-time reads only. Everything per-frame comes from the
        // snapshot pulled in `process_frame`.
        let scale = 1.0 / settings.video.DOWNSCALE;
        let work_w = (settings.video.FRAME_WIDTH as f64 * scale) as i32;
        let work_h = (settings.video.FRAME_HEIGHT as f64 * scale) as i32;

        let background = BackgroundModel::new(settings, (work_h, work_w))?;
        let warm_frames = background.warm_frames(settings);

        Ok(Self {
            consumers,
            settings_provider,
            budget_ms: settings.telemetry.BUDGET_MS,
            work_w,
            work_h,
            background,
            blob_finder: BlobFinder::new(settings, (work_h, work_w)),
            detector: if settings.model.ENABLED {
                Some(Detector::new(settings)?)
            } else {
                None
            },
            associator: Associator::new(settings),
            measurer: SpeedMeasurer::new(settings)?,
            gate: EnforcementGate::new(settings),
            warm_frames,
            likelihood: None,
            last_frame_id: Arc::new(AtomicI64::new(-1)),
            mailbox: Arc::new(Mailbox::new(lockstep)),
        })
    }

    pub fn mailbox(&self) -> Arc<Mailbox> {
        Arc::clone(&self.mailbox)
    }

    pub fn progress(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.last_frame_id)
    }

    pub fn working_shape(&self) -> (i32, i32) {
        (self.work_h, self.work_w)
    }

    pub fn measurer(&self) -> &SpeedMeasurer {
        &self.measurer
    }

    /// Run a frame synchronously on the calling thread.
    ///
    /// For tests and for `--replay`, where a second thread adds nothing but a
    /// scheduling variable.
    pub fn process_one(&mut self, frame: RawFrame) -> Result<Arc<FrameOutput>, String> {
        self.process_frame(frame)
    }

    // -- the frame -------------------------------------------------------

    fn process_frame(&mut self, frame: RawFrame) -> Result<Arc<FrameOutput>, String> {
        let began = Instant::now();
        // ONE pull. Everything below receives this object by reference.
        let settings = (self.settings_provider)();
        let settings = settings.as_ref();
        let mut pf = perf::Frame::new(frame.frame_id);

        pf.start("pre");
        let working = SharedMat::new(to_working(frame.full.get(), self.work_w, self.work_h)?);
        pf.end("pre");

        // Submit the inference *before* the background model so it overlaps.
        // See `detect::detector` -- this ordering is the whole reason the model
        // is close to free on a frame where it runs.
        let pending = match self.detector.as_ref() {
            Some(detector) if detector.should_run(frame.frame_id, settings) => {
                Some(detector.submit(Arc::clone(&working))?)
            }
            _ => None,
        };

        let (mask, foreground_ratio) = self.background.apply(working.get(), &mut pf)?;
        let mut blobs = self.blob_finder.find(&mask, settings, &mut pf)?;

        // Whether the model ran, not whether it was *timed*. Deriving this from
        // a perf counter would make the result CSV depend on `--perf`, which is
        // the sort of thing that turns a benchmark into a different program.
        let inference_ran = pending.is_some();

        pf.start("infer_join");
        if let Some(pending) = pending {
            let inference = pending
                .recv()
                .map_err(|_| obfstr::obfstr!("the detector thread stopped before answering").to_string())??;
            self.likelihood = Some(inference.likelihood);
            pf.set("infer", inference.infer_ms);
            perf::counters().inferences.fetch_add(1, Ordering::Relaxed);
        }
        pf.end("infer_join");

        // A frame where most of the image is foreground is not a frame with a
        // lot of traffic in it -- it is a frame where something happened to the
        // camera or the light. Every blob in it is nonsense, so drop them all
        // rather than tracking the results.
        //
        // (The Python spells the threshold inline; it is the same number as
        // `tuning::frame_max_foreground_ratio()`, named here so the two cannot
        // drift apart.)
        if foreground_ratio > tuning::frame_max_foreground_ratio() || frame.frame_id < self.warm_frames
        {
            blobs.clear();
        }

        let coverage = score_blobs(
            self.likelihood.as_ref(),
            &blobs,
            (self.work_h, self.work_w),
            settings,
            &mut pf,
        );
        let matched = self
            .associator
            .update(frame.frame_id, &blobs, &coverage, settings, &mut pf);
        self.measurer.observe(
            frame.frame_id,
            frame.t_capture,
            self.associator.vehicles_mut(),
            &matched,
            &blobs,
            settings,
            &mut pf,
        );
        let states = self.associator.snapshot(frame.frame_id);
        let verdicts = self
            .gate
            .evaluate(frame.frame_id, &states.vehicles, settings, &mut pf);

        let result = FrameResult {
            frame_id: frame.frame_id,
            t_capture: frame.t_capture,
            blobs,
            detections: Vec::new(),
            inference_ran,
            foreground_ratio,
            working_shape: (self.work_h, self.work_w),
        };

        // The observation lists, borrowed rather than copied -- the record
        // rounds them into its own storage.
        let observations: Vec<(i64, &[crate::track::types::Observation])> = self
            .associator
            .vehicles()
            .iter()
            .map(|v| (v.vehicle_id, v.observations.as_slice()))
            .collect();

        pf.start("emit");
        let output = self.consumers.run(
            frame,
            result,
            states,
            verdicts,
            coverage,
            settings,
            &mut pf,
            &observations,
        )?;
        pf.end("emit");

        let elapsed_ms = began.elapsed().as_secs_f64() * 1000.0;
        pf.set("total", elapsed_ms);
        let counters = perf::counters();
        counters.note_frame(elapsed_ms, self.budget_ms);
        counters.dropped.store(
            self.mailbox.dropped.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        perf::submit(&pf);
        self.last_frame_id
            .store(output.frame.frame_id, Ordering::Release);
        Ok(output)
    }
}

/// A pipeline running on its own thread.
///
/// The pipeline is *moved* onto the worker and returned by [`Self::stop`], so
/// there is never a moment where two threads hold it. That is what replaces the
/// Python's "the worker mutates the object while the main thread reads a field
/// off it", and it is why `last_output` became an atomic frame counter rather
/// than a shared reference.
pub struct RunningPipeline {
    mailbox: Arc<Mailbox>,
    progress: Arc<AtomicI64>,
    handle: Option<JoinHandle<Result<Pipeline, String>>>,
}

impl RunningPipeline {
    pub fn start(pipeline: Pipeline) -> Result<Self, String> {
        let mailbox = pipeline.mailbox();
        let progress = pipeline.progress();
        let handle = std::thread::Builder::new()
            .name(obfstr::obfstr!("pipeline").into())
            .spawn(move || -> Result<Pipeline, String> {
                let mut pipeline = pipeline;
                let mailbox = pipeline.mailbox();
                loop {
                    match mailbox.take(Duration::from_millis(500)) {
                        Some(frame) => {
                            pipeline.process_frame(frame)?;
                        }
                        None => {
                            let closed = {
                                let slot =
                                    mailbox.slot.lock().unwrap_or_else(|e| e.into_inner());
                                slot.closed && slot.frame.is_none()
                            };
                            if closed {
                                return Ok(pipeline);
                            }
                        }
                    }
                }
            })
            .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot start the pipeline thread: ")))?;
        Ok(Self {
            mailbox,
            progress,
            handle: Some(handle),
        })
    }

    pub fn mailbox(&self) -> &Arc<Mailbox> {
        &self.mailbox
    }

    pub fn last_frame_id(&self) -> i64 {
        self.progress.load(Ordering::Acquire)
    }

    /// Close the mailbox, join the worker, and take the pipeline back.
    pub fn stop(mut self) -> Result<Pipeline, String> {
        self.mailbox.close();
        match self.handle.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| obfstr::obfstr!("the pipeline thread panicked").to_string())?,
            None => Err(obfstr::obfstr!("the pipeline thread was already joined").into()),
        }
    }
}

/// The default settings provider: the live, atomically rebindable object.
pub fn live_settings() -> SettingsProvider {
    Box::new(config::current)
}

/// A fixed snapshot, for tests that must not see a runtime change.
pub fn fixed_settings(settings: Arc<Settings>) -> SettingsProvider {
    Box::new(move || Arc::clone(&settings))
}
