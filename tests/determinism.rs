//! The port's correctness oracle.
//!
//! Two implementations agree if they produce the same result CSV. That only
//! means anything if each of them produces the *same file twice* on the same
//! machine first, so these tests pin the property the comparison rests on:
//! nothing in the frame path reads the clock, depends on iteration order, or
//! depends on which thread ran it.

mod common;

use common::{clip_settings, run_clip, run_clip_threaded};
use kerbside::config::Value;
use kerbside::output::{frame_hash, row_for};
use kerbside::source::RoadScene;

const FRAMES: i64 = 240;

/// The rows a run would have written, without going near a file.
fn rows(outputs: &[std::sync::Arc<kerbside::consumers::FrameOutput>]) -> Vec<String> {
    let mut running = 0u64;
    outputs
        .iter()
        .map(|output| {
            running += output.violation_count() as u64;
            row_for(output, running).join(",")
        })
        .collect()
}

#[test]
fn the_scene_is_reproducible_from_its_seed() {
    let settings = clip_settings(8, &[]);
    let first = RoadScene::new(&settings).expect("scene");
    let second = RoadScene::new(&settings).expect("scene");
    for frame_id in 0..8 {
        let a = frame_hash(&first.render(frame_id).unwrap().0).unwrap();
        let b = frame_hash(&second.render(frame_id).unwrap().0).unwrap();
        assert_eq!(a, b, "frame {frame_id} differs between two generators");
    }
}

#[test]
fn the_scene_depends_on_the_seed() {
    let a = clip_settings(4, &[("video.SCENE_SEED", Value::Int(7))]);
    let b = clip_settings(4, &[("video.SCENE_SEED", Value::Int(8))]);
    let first = RoadScene::new(&a).expect("scene");
    let second = RoadScene::new(&b).expect("scene");
    let differ = (0..4).any(|frame_id| {
        frame_hash(&first.render(frame_id).unwrap().0).unwrap()
            != frame_hash(&second.render(frame_id).unwrap().0).unwrap()
    });
    assert!(differ, "two different seeds produced the same clip");
}

#[test]
fn replay_is_reproducible() {
    let settings = clip_settings(FRAMES, &[]);
    let first = rows(&run_clip(&settings, FRAMES));
    let second = rows(&run_clip(&settings, FRAMES));
    assert_eq!(first.len(), FRAMES as usize);
    assert_eq!(
        first, second,
        "two identical replay runs produced different output"
    );
}

/// Proves the threaded path produces identical results to the inline one.
///
/// If it does not, the pipeline depends on scheduling and every benchmark taken
/// with it is comparing schedules rather than implementations. This is a
/// sharper test here than in the Python: there is no interpreter lock
/// serialising the detector thread against the pipeline thread, so a genuine
/// race would show up rather than being hidden.
#[test]
fn threaded_and_inline_agree() {
    let settings = clip_settings(FRAMES, &[]);
    let inline = rows(&run_clip(&settings, FRAMES));
    let threaded = rows(&run_clip_threaded(&settings, FRAMES));
    assert_eq!(inline.len(), threaded.len(), "the threaded run lost frames");
    assert_eq!(
        inline, threaded,
        "the threaded path disagrees with the inline one"
    );
}

/// Timestamps come from the frame index, never from the clock.
#[test]
fn no_wall_clock_in_the_frame_path() {
    let settings = clip_settings(20, &[]);
    let outputs = run_clip(&settings, 20);
    for output in &outputs {
        let expected = output.frame.frame_id as f64 / settings.video.FPS as f64;
        assert_eq!(
            output.frame.t_capture.to_bits(),
            expected.to_bits(),
            "frame {} carries a timestamp that is not derived from its index",
            output.frame.frame_id
        );
    }
}

/// A clip that finds nothing would satisfy every assertion above.
#[test]
fn the_clip_actually_exercises_the_system() {
    let settings = clip_settings(FRAMES, &[]);
    let outputs = run_clip(&settings, FRAMES);
    let blobs: usize = outputs.iter().map(|o| o.result.blobs.len()).sum();
    let tracked: usize = outputs.iter().map(|o| o.vehicles.vehicles.len()).sum();
    assert!(blobs > 100, "only {blobs} blobs over {FRAMES} frames");
    assert!(tracked > 100, "only {tracked} tracked vehicle-frames");
    assert!(
        outputs.iter().any(|o| o.result.inference_ran),
        "the model never ran"
    );
}
