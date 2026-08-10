//! Does the device measure the right speed?
//!
//! This is the test the application exists to pass. The scene generator builds
//! every vehicle through the same homography the app calibrates with, so each
//! vehicle's true speed is known exactly, in metres per second, by
//! construction. That turns accuracy into an assertion instead of an
//! impression.
//!
//! It is also the test that keeps the rest of the suite honest. Determinism,
//! allocation churn and stage budgets can all be satisfied by an application
//! that produces confident nonsense; this one cannot. A port that reproduces
//! the reference CSV passes it automatically -- and a port that is subtly wrong
//! may reproduce most columns and still mismeasure, which is exactly why it is
//! run rather than assumed.
//!
//! **Where the error is measured matters.** These assertions look at the frames
//! on which a violation was actually published, not at every frame a vehicle
//! was tracked. That is not flattery -- it is the only measurement the device
//! makes any claim about. A vehicle half-way out of the far end of the zone has
//! a legitimately poor estimate, and the gate declines to enforce on it;
//! scoring those frames would be scoring readings the device explicitly
//! refused to issue.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::{clip_settings, ground_truth, run_clip};
use kerbside::consumers::FrameOutput;
use kerbside::enforce::Verdict;
use kerbside::geometry::Box;
use kerbside::track::VehicleState;

const FRAMES: i64 = 900;

struct Published {
    verdict: Verdict,
    state: VehicleState,
    true_speed_kph: f64,
}

struct Measured {
    settings: kerbside::config::Settings,
    outputs: Vec<Arc<FrameOutput>>,
    published: Vec<Published>,
}

/// The clip is expensive, so it is run once and shared -- the equivalent of
/// pytest's module-scoped fixture. Each assertion below then gets its own test
/// and its own name, which is what makes a failure say something.
fn measured() -> &'static Measured {
    static MEASURED: std::sync::OnceLock<Measured> = std::sync::OnceLock::new();
    MEASURED.get_or_init(build)
}

/// Run one clip and pair every published reading with its ground truth.
///
/// Pairing is done **per track over its whole life**, not per frame. A single
/// frame is a bad instrument: when a faster vehicle catches a slower one their
/// boxes overlap heavily, and a nearest-overlap rule can pick the wrong truth
/// on a margin of a few hundredths -- which then reads as the device wildly
/// misreporting a slow vehicle when in fact it was following the fast one
/// correctly. That happened, and it was the test that was wrong.
///
/// Accumulating overlap across every frame of a track resolves it: a track
/// follows one vehicle for the great majority of its life, so the total is
/// unambiguous even where individual frames are not.
fn build() -> Measured {
    let settings = clip_settings(FRAMES, &[]);
    let outputs = run_clip(&settings, FRAMES);
    let truth = ground_truth(&settings, FRAMES);
    let scale = 1.0 / settings.video.DOWNSCALE;

    let truth_box = |b: &Box| Box::new(b.x * scale, b.y * scale, b.w * scale, b.h * scale);

    // Pass one: total overlap between each track and each true vehicle.
    let mut affinity: HashMap<i64, HashMap<i64, f64>> = HashMap::new();
    for output in &outputs {
        let frame_truth = &truth[output.frame.frame_id as usize];
        for state in &output.vehicles.vehicles {
            let row = affinity.entry(state.vehicle_id).or_default();
            for vehicle in frame_truth.visible() {
                if let Some(b) = &vehicle.box_ {
                    let overlap = state.box_.iou(&truth_box(b));
                    if overlap > 0.1 {
                        *row.entry(vehicle.vehicle_id).or_insert(0.0) += overlap;
                    }
                }
            }
        }
    }
    let identity: HashMap<i64, i64> = affinity
        .iter()
        .filter_map(|(track, row)| {
            row.iter()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(true_id, _)| (*track, *true_id))
        })
        .collect();

    // Pass two: collect the published readings under that fixed identity.
    let mut published = Vec::new();
    for output in &outputs {
        let frame_truth = &truth[output.frame.frame_id as usize];
        for verdict in &output.verdicts {
            if !verdict.violation {
                continue;
            }
            let state = match output.vehicles.by_id(verdict.vehicle_id) {
                Some(state) => *state,
                None => continue,
            };
            let true_id = match identity.get(&verdict.vehicle_id) {
                Some(id) => *id,
                None => continue,
            };
            let actual = frame_truth
                .visible()
                .into_iter()
                .find(|v| v.vehicle_id == true_id);
            if let Some(actual) = actual {
                published.push(Published {
                    verdict: *verdict,
                    state,
                    true_speed_kph: actual.speed_kph,
                });
            }
        }
    }
    Measured {
        settings,
        outputs,
        published,
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values[values.len() / 2]
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((p / 100.0) * values.len() as f64).round() as usize;
    values[idx.min(values.len() - 1)]
}

fn errors() -> Vec<f64> {
    measured()
        .published
        .iter()
        .map(|p| p.verdict.speed_kph - p.true_speed_kph)
        .collect()
}

/// A clip that triggers nothing would pass every accuracy assertion below.
#[test]
fn violations_are_published_at_all() {
    let published = &measured().published;
    assert!(
        published.len() > 40,
        "only {} published readings could be matched to ground truth; the \
         accuracy assertions would be near-vacuous",
        published.len()
    );
}

/// The number that goes in the evidence packet is close to the truth.
#[test]
fn published_speeds_are_accurate() {
    let errors = errors();

    let median_error = median(&mut errors.clone());
    assert!(
        median_error.abs() < 1.5,
        "median speed error {median_error:+.2} km/h -- a systematic bias this \
         size means the survey or the contact point is wrong, not the noise"
    );

    let within_3 = errors.iter().filter(|e| e.abs() < 3.0).count() as f64 / errors.len() as f64;
    assert!(
        within_3 > 0.90,
        "only {:.0}% of published readings are within 3 km/h of truth",
        within_3 * 100.0
    );

    let mut absolute: Vec<f64> = errors.iter().map(|e| e.abs()).collect();
    let p90 = percentile(&mut absolute, 90.0);
    assert!(p90 < 4.0, "90th percentile absolute error {p90:.2} km/h");
}

/// The direction of the error that matters.
///
/// Over-reading a vehicle that was inside the limit is the failure with a
/// victim. The enforcement tolerance exists to absorb measurement error, so a
/// true speed below limit-plus-tolerance must never produce a violation.
#[test]
fn no_violation_is_published_below_the_limit() {
    let m = measured();
    let tolerance = m.settings.enforcement.TOLERANCE_KPH;
    let wrongly_accused: Vec<&Published> = m
        .published
        .iter()
        .filter(|p| p.true_speed_kph <= p.verdict.limit_kph + tolerance)
        .collect();
    assert!(
        wrongly_accused.is_empty(),
        "{} violations published against vehicles that were within the limit \
         plus tolerance; worst was a true {:.1} km/h",
        wrongly_accused.len(),
        wrongly_accused
            .iter()
            .map(|p| p.true_speed_kph)
            .fold(f64::INFINITY, f64::min)
    );
}

/// The residual is the device's own statement of confidence.
///
/// If it does not correlate with actual error it is decoration, and the gate's
/// reliance on it is unfounded. This asserts the weak but essential property:
/// readings the fit called good really are better than the rest.
#[test]
fn the_fit_residual_reflects_reality() {
    let m = measured();
    let cut = median(&mut m.published.iter().map(|p| p.state.fit_residual_m).collect::<Vec<_>>());

    let mut tight: Vec<f64> = Vec::new();
    let mut loose: Vec<f64> = Vec::new();
    for p in &m.published {
        let error = (p.verdict.speed_kph - p.true_speed_kph).abs();
        if p.state.fit_residual_m <= cut {
            tight.push(error);
        } else {
            loose.push(error);
        }
    }
    if tight.is_empty() || loose.is_empty() {
        return;
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    assert!(
        mean(&tight) <= mean(&loose) * 1.35,
        "readings with a tight fit ({:.2} km/h mean error) are no better than \
         loose ones ({:.2}) -- the residual is not measuring what the gate \
         believes it measures",
        mean(&tight),
        mean(&loose)
    );
}

/// Vehicle class is reported but never enforced on -- see gate::limit_for.
///
/// Measured length is not good enough to judge anyone by: its error has a 90th
/// percentile of over six metres, which would put roughly a quarter of cars
/// under the goods-vehicle limit. This pins the decision not to use it, because
/// the temptation to wire it back in is obvious and the harm is invisible.
#[test]
fn one_limit_applies_to_every_vehicle() {
    let m = measured();
    let mut limits: Vec<f64> = m.published.iter().map(|p| p.verdict.limit_kph).collect();
    limits.sort_by(|a, b| a.partial_cmp(b).unwrap());
    limits.dedup();
    assert_eq!(
        limits.len(),
        1,
        "more than one limit was applied: {limits:?}. Vehicle class is not \
         measured well enough to enforce on."
    );
    assert!((limits[0] - m.settings.enforcement.SPEED_LIMIT_KPH).abs() < 1e-9);
}

/// Reported, because the evidence record should carry what was observed.
#[test]
fn vehicle_length_is_still_reported() {
    assert!(measured().published.iter().any(|p| p.state.length_m > 0.0));
}

/// Nothing is enforced on outside the calibrated stretch of road.
#[test]
fn the_zone_is_where_measurement_happens() {
    let m = measured();
    let cal = &m.settings.calibration;
    for output in &m.outputs {
        for verdict in &output.verdicts {
            if !verdict.violation {
                continue;
            }
            let state = output
                .vehicles
                .by_id(verdict.vehicle_id)
                .expect("a verdict without a vehicle");
            assert!(state.in_zone);
            assert!(
                cal.ZONE_START_M - 1.0 <= state.along_m && state.along_m <= cal.ZONE_END_M + 1.0,
                "a violation was published at {:.2} m, outside the zone",
                state.along_m
            );
        }
    }
}
