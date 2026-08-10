//! Derived constants that were arrived at by measurement, not by reasoning.
//!
//! Everything in this file is an empirical number. Each one was chosen by
//! running clips and looking at what happened, and none can be re-derived from
//! first principles -- which is exactly what makes a file like this the most
//! valuable thing in a binary to whoever is trying to reverse-engineer it. A
//! competitor who obtains it skips the measurement campaign that produced it,
//! and that campaign is usually the expensive part of a system like this.
//!
//! It is kept separate from `config` for two reasons. The mundane one: these
//! are not operator-tunable, so they do not belong in the settings object. The
//! relevant one: it gives an evaluation of build-hardening options a single
//! concrete target to ask "can I still read this?" about.
//!
//! Porting note
//! ------------
//! This module is target T1 of the reverse-engineering assessment, and it is
//! the one the port is supposed to move the needle on. In the Python these are
//! module globals with their names attached, recoverable in seconds from the
//! `.pyc`. Here they are `f64` constants: the *names* are gone from a release
//! build, but the values are not -- they end up as immediates or in `.rodata`,
//! and a float like 1.85 next to a comparison is not hard to spot. Recovering
//! which of them is `GATE_COAST_MARGIN_KPH` is the part that got expensive, and
//! that distinction -- names versus values -- is what the RE table should
//! record.
//!
//! See `docs/re_scoring.md` in the Python repository.

// --- foreground acceptance -------------------------------------------------
/// Fraction of the working frame that may be foreground before the frame is
/// declared unusable. Above this the scene has changed, not the traffic.
pub const FRAME_MAX_FOREGROUND_RATIO: f64 = 0.35;
/// Frames after a light change during which readings are suppressed while the
/// mixture model re-converges.
pub const BACKGROUND_SETTLE_FRAMES: i64 = 22;
/// Blob bounding boxes overlapping by more than this are one vehicle seen
/// twice, not two vehicles.
pub const BLOB_MERGE_IOU: f64 = 0.62;

// --- association -----------------------------------------------------------
/// Overlap below which a match is accepted only if nothing else competes for it.
pub const ASSOC_WEAK_IOU: f64 = 0.21;
/// Consecutive frames a vehicle may be occluded before its measurement restarts
/// rather than bridging the gap.
pub const ASSOC_OCCLUSION_LIMIT: i64 = 6;
/// Ratio of box areas beyond which two boxes cannot be the same vehicle.
pub const ASSOC_MAX_AREA_RATIO: f64 = 2.8;

// --- measurement -----------------------------------------------------------
/// Metres of the zone that must be covered before the far-end samples are
/// trusted; below it the foreshortening dominates the fit.
pub const FIT_MIN_FAR_COVERAGE_M: f64 = 9.5;
/// Weighting applied to near-camera samples, where a pixel is worth less road.
pub const FIT_NEAR_WEIGHT: f64 = 1.35;
/// Residual above which a fit is reported but flagged for manual review.
pub const FIT_REVIEW_RESIDUAL_M: f64 = 0.31;

// --- the gate --------------------------------------------------------------
/// Extra margin, in km/h, applied when a vehicle was coasting through part of
/// the zone rather than measured on every frame.
pub const GATE_COAST_MARGIN_KPH: f64 = 1.85;
/// Frames after a lane change during which no violation is published: the
/// vehicle's ground-contact point moves for a reason unrelated to its speed.
pub const GATE_LANE_CHANGE_BLACKOUT: i64 = 12;
/// Fraction of a vehicle's in-zone samples that must be uninterrupted.
pub const GATE_MIN_CONTIGUITY: f64 = 0.68;
