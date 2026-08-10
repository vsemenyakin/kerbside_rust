//! Shared test helpers.
//!
//! One process is shared by the tests in each binary, so anything global a test
//! mutates must be restored. Settings are the exception -- they cannot be
//! assigned through a shared reference, and `config::temporarily` restores
//! itself on drop.
//!
//! Every helper here builds its own `Settings` and hands the pipeline a *fixed*
//! snapshot rather than the live one. That is deliberate: a test that read the
//! process-wide settings would pass or fail depending on what another test in
//! the same binary happened to be doing.

#![allow(dead_code)]

use std::sync::Arc;

use std::sync::Mutex;

use kerbside::config::{self, Settings, Value};
use kerbside::consumers::{ConsumerChain, Consumer, FanOut, FrameOutput};
use kerbside::pipeline::types::SharedMat;
use kerbside::pipeline::{fixed_settings, Pipeline, RawFrame, RunningPipeline};
use kerbside::source::{GroundTruth, RoadScene};

/// Settings for a clip of `frames` frames, with the ring deep enough to retain
/// all of them so tests can read the outputs back out of it.
pub fn clip_settings(frames: i64, extra: &[(&str, Value)]) -> Settings {
    let mut overrides: Vec<(&str, Value)> = vec![
        ("video.SCENE_FRAMES", Value::Int(frames)),
        // The ring is the test's window onto the run. Sizing it to the clip is
        // the only change from production, and it is a change in depth rather
        // than in kind -- the retention path being exercised is the real one.
        ("telemetry.RING_FRAMES", Value::Int(frames.max(1))),
    ];
    overrides.extend_from_slice(extra);
    config::resolve_settings(None, &overrides, true).expect("test settings must resolve")
}

/// Pin OpenCV's thread pool.
///
/// Without this the suite inherits the host's core count, and the
/// timing-sensitive tests pass or fail depending on which machine ran them.
pub fn pin_runtime(settings: &Settings) {
    opencv::core::set_num_threads(settings.telemetry.OPENCV_THREADS)
        .expect("the OpenCV thread pool must be pinnable");
}

/// A sink that keeps every frame it is handed.
///
/// The tests need to see *all* the outputs, not just the ones the ring happens
/// to still be holding -- the ring depth is itself under test in `churn.rs`.
/// Keeping the `Arc` costs a pointer per frame, which is what the ring does
/// too.
struct Collector {
    frames: Arc<Mutex<Vec<Arc<FrameOutput>>>>,
}

impl Consumer for Collector {
    fn consume(&mut self, output: &Arc<FrameOutput>) -> Result<(), String> {
        self.frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(output));
        Ok(())
    }
}

/// Run `frames` frames through a full pipeline; return every frame it produced.
pub fn run_clip(settings: &Settings, frames: i64) -> Vec<Arc<FrameOutput>> {
    run_clip_inner(settings, frames, false).0
}

/// The same clip, driven through the mailbox and the worker thread.
pub fn run_clip_threaded(settings: &Settings, frames: i64) -> Vec<Arc<FrameOutput>> {
    run_clip_inner(settings, frames, true).0
}

/// The clip, plus the pipeline it ran on -- for tests that need to look at the
/// ring itself rather than at the frames it happens to be holding.
pub fn run_clip_pipeline(settings: &Settings, frames: i64) -> (Vec<Arc<FrameOutput>>, Pipeline) {
    run_clip_inner(settings, frames, false)
}

fn run_clip_inner(
    settings: &Settings,
    frames: i64,
    threaded: bool,
) -> (Vec<Arc<FrameOutput>>, Pipeline) {
    pin_runtime(settings);
    let scene = RoadScene::new(settings).expect("the scene must build");
    let snapshot = Arc::new(settings.clone());
    let collected: Arc<Mutex<Vec<Arc<FrameOutput>>>> = Arc::new(Mutex::new(Vec::new()));
    let pipeline = Pipeline::new(
        settings,
        ConsumerChain::new(
            settings,
            Some(FanOut::new(vec![Box::new(Collector {
                frames: Arc::clone(&collected),
            })])),
        ),
        fixed_settings(Arc::clone(&snapshot)),
        true,
    )
    .expect("the pipeline must build");

    let pipeline = if threaded {
        let running = RunningPipeline::start(pipeline).expect("the worker must start");
        for frame_id in 0..frames {
            running.mailbox().post(frame_for(&scene, settings, frame_id));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline && running.last_frame_id() < frames - 1 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        running.stop().expect("the worker must stop cleanly")
    } else {
        let mut pipeline = pipeline;
        for frame_id in 0..frames {
            pipeline
                .process_one(frame_for(&scene, settings, frame_id))
                .expect("every frame must process");
        }
        pipeline
    };

    let frames = collected.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (frames, pipeline)
}

pub fn frame_for(scene: &RoadScene, settings: &Settings, frame_id: i64) -> RawFrame {
    let (image, _truth) = scene.render(frame_id).expect("the frame must render");
    RawFrame::new(
        frame_id,
        SharedMat::new(image),
        frame_id as f64 / settings.video.FPS as f64,
    )
}

/// Frame id -> ground truth, from a fresh generator.
pub fn ground_truth(settings: &Settings, frames: i64) -> Vec<GroundTruth> {
    let scene = RoadScene::new(settings).expect("the scene must build");
    (0..frames)
        .map(|frame_id| scene.render(frame_id).expect("the frame must render").1)
        .collect()
}
