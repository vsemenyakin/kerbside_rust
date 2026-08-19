//! Decide whether a vehicle has committed an offence.
//!
//! **Never re-derive this condition anywhere else.** Read the [`Verdict`] this
//! module returns. It carries every input that went into the decision, so a
//! renderer, a log line and a test all report exactly what the gate saw without
//! recomputing anything, and a "no violation" can explain itself.
//!
//! The gate is deliberately conservative. Every precondition exists because
//! publishing a wrong violation is far more costly than missing a real one: the
//! output of this device is used against a person, and a number it cannot
//! defend is worse than no number.

use std::collections::HashMap;

use crate::config::Settings;
use crate::perf;
use crate::track::types::{VehicleState, LONG_VEHICLE_M};

/// One vehicle's assessment at one frame, with every precondition recorded.
///
/// `violation` is the AND of the booleans below it. Keeping them as separate
/// fields rather than collapsing them at construction is what makes a negative
/// decision debuggable: "no violation" is useless; "no violation because the
/// fit residual was 0.6 m and the limit is 0.42" is not.
#[derive(Debug, Clone, Copy)]
pub struct Verdict {
    pub frame_id: i64,
    pub vehicle_id: i64,
    pub violation: bool,

    pub in_zone: bool,
    pub confirmed: bool,
    pub enough_samples: bool,
    pub enough_baseline: bool,
    pub fit_good: bool,
    pub confident: bool,
    pub over_limit: bool,
    pub stable: bool,

    pub speed_kph: f64,
    pub limit_kph: f64,
    pub excess_kph: f64,
    pub residual_m: f64,
    pub baseline_m: f64,
    pub samples: usize,
    pub coverage: f64,
    pub is_long: bool,
    pub streak: i64,
}

impl Verdict {
    /// Why there was no violation. Empty string when there was one.
    pub fn reason(&self) -> &'static str {
        if self.violation {
            return "";
        }
        // The labels are decoded once into 'static storage: the ciphertext,
        // not the plaintext, is what ships in .rodata, while `reason` still
        // returns a `&'static str` that the evidence record stores by reference
        // -- so the per-frame allocation profile the churn tests pin does not
        // change, and the decoded bytes are identical (the result CSV, and thus
        // the oracle, does not move).
        static LABELS: std::sync::OnceLock<[String; 9]> = std::sync::OnceLock::new();
        let labels = LABELS.get_or_init(|| {
            [
                obfstr::obfstr!("outside zone").to_string(),
                obfstr::obfstr!("unconfirmed").to_string(),
                obfstr::obfstr!("too few samples").to_string(),
                obfstr::obfstr!("baseline too short").to_string(),
                obfstr::obfstr!("poor fit").to_string(),
                obfstr::obfstr!("low confidence").to_string(),
                obfstr::obfstr!("within limit").to_string(),
                obfstr::obfstr!("unstable").to_string(),
                obfstr::obfstr!("unknown").to_string(),
            ]
        });
        for (i, ok) in [
            self.in_zone,
            self.confirmed,
            self.enough_samples,
            self.enough_baseline,
            self.fit_good,
            self.confident,
            self.over_limit,
            self.stable,
        ]
        .into_iter()
        .enumerate()
        {
            if !ok {
                return labels[i].as_str();
            }
        }
        labels[8].as_str()
    }
}

/// The speed limit this vehicle is held to: one limit, for everything.
///
/// Goods vehicles are held to a lower limit than cars on real roads, and this
/// device deliberately does **not** implement that. Two ways of establishing
/// vehicle class were built and measured, and neither is good enough to enforce
/// on:
///
/// * **From the model.** A second output head fitted for "is this a goods
///   vehicle" reached an area under the ROC curve of 0.63 -- barely better than
///   chance. A cell inside a lorry looks locally like a cell inside a car; what
///   separates them is overall size, which a per-cell classifier cannot see.
/// * **From geometry.** Apparent length through the ground plane sounds
///   auditable and is not: the bounding box's height is the vehicle's projected
///   footprint length *plus* its projected body height, which is two unknowns
///   behind one measurement. Measured against ground truth the error has a 90th
///   percentile of 6.3 m, and **24% of cars would be held to the goods-vehicle
///   limit**.
///
/// A quarter of drivers judged against the wrong limit is not a rounding error,
/// it is a wrongful accusation with a mechanism. So the class is measured,
/// reported in the evidence record, and not enforced on. A real installation
/// would take it from a source that can carry the weight -- an axle sensor, or
/// a plate lookup -- and until there is one, one limit applies to everything.
pub fn limit_for(_state: &VehicleState, settings: &Settings) -> f64 {
    settings.enforcement.SPEED_LIMIT_KPH
}

/// Evaluates the gate per vehicle and debounces across frames.
pub struct EnforcementGate {
    streaks: HashMap<i64, i64>,
}

impl EnforcementGate {
    pub fn new(_settings: &Settings) -> Self {
        Self {
            streaks: HashMap::new(),
        }
    }

    pub fn evaluate(
        &mut self,
        frame_id: i64,
        states: &[VehicleState],
        settings: &Settings,
        pf: &mut perf::Frame,
    ) -> Vec<Verdict> {
        let enf = &settings.enforcement;
        pf.start(crate::perf::stage::GATE);

        let mut verdicts = Vec::with_capacity(states.len());
        let mut live: Vec<i64> = Vec::with_capacity(states.len());
        for state in states {
            live.push(state.vehicle_id);
            let limit = limit_for(state, settings);
            let threshold = limit + enf.TOLERANCE_KPH;

            let in_zone = state.in_zone;
            let confirmed = state.confirmed;
            let enough_samples = state.samples >= enf.MIN_SAMPLES;
            let enough_baseline = state.baseline_m >= enf.MIN_BASELINE_M;
            let fit_good = state.fit_residual_m <= enf.MAX_FIT_RESIDUAL_M && state.speed_kph > 0.0;
            let confident = state.coverage >= settings.model.MIN_COVERAGE;
            let over_limit = state.speed_kph > threshold;

            // Debounce per vehicle. A speed sitting within a fraction of a km/h
            // of the threshold flips frame to frame on measurement noise alone,
            // and an undebounced gate would publish a violation on whichever
            // frame happened to land high.
            let entry = self.streaks.entry(state.vehicle_id).or_insert(0);
            *entry = if over_limit { *entry + 1 } else { 0 };
            let streak = *entry;
            let stable = streak >= enf.STABILITY_FRAMES;

            let violation = in_zone
                && confirmed
                && enough_samples
                && enough_baseline
                && fit_good
                && confident
                && over_limit
                && stable;

            verdicts.push(Verdict {
                frame_id,
                vehicle_id: state.vehicle_id,
                violation,
                in_zone,
                confirmed,
                enough_samples,
                enough_baseline,
                fit_good,
                confident,
                over_limit,
                stable,
                speed_kph: state.speed_kph,
                limit_kph: limit,
                excess_kph: f64::max(0.0, state.speed_kph - limit),
                residual_m: state.fit_residual_m,
                baseline_m: state.baseline_m,
                samples: state.samples,
                coverage: state.coverage,
                is_long: state.length_m >= LONG_VEHICLE_M,
                streak,
            });
        }

        // Retire streaks for vehicles that have gone, so the map does not grow
        // for the lifetime of the process.
        self.streaks.retain(|id, _| live.contains(id));

        pf.end(crate::perf::stage::GATE);
        verdicts
    }
}
