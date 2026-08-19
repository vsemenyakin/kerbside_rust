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
//! This module is target T1 of the reverse-engineering assessment. In the
//! Python these are module globals with their names attached, recoverable in
//! seconds from the `.pyc`. Here the *names* are gone from a release build, and
//! each value is **encrypted at rest**: it is stored XORed against a key and
//! decoded at run time behind an optimisation barrier (see [`crate::crypt`]), so
//! a float scan of the shipped binary no longer finds it. That is why these are
//! accessor functions rather than `pub const` -- an encrypted constant has to
//! materialise at run time, and a `const` would be folded back to plaintext.
//! The decoded value is still exposed by a RAM dump; this addresses the medium,
//! not the live process.

use crate::{encf, enci};

// --- foreground acceptance -------------------------------------------------
/// Fraction of the working frame that may be foreground before the frame is
/// declared unusable. Above this the scene has changed, not the traffic.
pub fn frame_max_foreground_ratio() -> f64 { encf!(0.35) }
/// Frames after a light change during which readings are suppressed while the
/// mixture model re-converges.
pub fn background_settle_frames() -> i64 { enci!(22) }
/// Blob bounding boxes overlapping by more than this are one vehicle seen
/// twice, not two vehicles.
pub fn blob_merge_iou() -> f64 { encf!(0.62) }

// --- association -----------------------------------------------------------
/// Overlap below which a match is accepted only if nothing else competes for it.
pub fn assoc_weak_iou() -> f64 { encf!(0.21) }
/// Consecutive frames a vehicle may be occluded before its measurement restarts
/// rather than bridging the gap.
pub fn assoc_occlusion_limit() -> i64 { enci!(6) }
/// Ratio of box areas beyond which two boxes cannot be the same vehicle.
pub fn assoc_max_area_ratio() -> f64 { encf!(2.8) }

// --- measurement -----------------------------------------------------------
/// Metres of the zone that must be covered before the far-end samples are
/// trusted; below it the foreshortening dominates the fit.
pub fn fit_min_far_coverage_m() -> f64 { encf!(9.5) }
/// Weighting applied to near-camera samples, where a pixel is worth less road.
pub fn fit_near_weight() -> f64 { encf!(1.35) }
/// Residual above which a fit is reported but flagged for manual review.
pub fn fit_review_residual_m() -> f64 { encf!(0.31) }

// --- the gate --------------------------------------------------------------
/// Extra margin, in km/h, applied when a vehicle was coasting through part of
/// the zone rather than measured on every frame.
pub fn gate_coast_margin_kph() -> f64 { encf!(1.85) }
/// Frames after a lane change during which no violation is published: the
/// vehicle's ground-contact point moves for a reason unrelated to its speed.
pub fn gate_lane_change_blackout() -> i64 { enci!(12) }
/// Fraction of a vehicle's in-zone samples that must be uninterrupted.
pub fn gate_min_contiguity() -> f64 { encf!(0.68) }


// ===========================================================================
// Relocated from `config/*` (were plaintext defaults there).
//
// Fixed, non-operator-tunable numbers that happened to live in the settings
// groups as plain f64/i64 literals -- and were therefore recoverable by a float
// scan of the binary. None is overridden by any CLI flag, profile or test, so
// per the note at the top of this module they belong here: encrypted at rest
// and materialised only at the use-site, deep in the frame loop.
// ===========================================================================

// -- blob geometry filter (was config/background.rs) --
pub fn blob_min_area() -> i64 { enci!(135) }
pub fn blob_max_area() -> i64 { enci!(14_600) }
pub fn blob_min_aspect() -> f64 { encf!(0.35) }
pub fn blob_max_aspect() -> f64 { encf!(4.5) }
pub fn blob_min_fill() -> f64 { encf!(0.42) }

// -- detector scoring (was config/model.rs) --
pub fn infer_every_n_frames() -> i64 { enci!(4) }
pub fn vehicle_threshold() -> f64 { encf!(0.19) }
pub fn min_coverage() -> f64 { encf!(0.34) }

// -- association / lifecycle (was config/tracking.rs) --
pub fn assoc_min_iou() -> f64 { encf!(0.34) }
pub fn confirm_frames() -> i64 { enci!(4) }
pub fn max_misses() -> i64 { enci!(10) }
pub fn track_max_jump_m() -> f64 { encf!(1.5) }

// -- enforcement gate thresholds (was config/enforcement.rs) --
pub fn gate_min_samples() -> usize { enci!(20) as usize }
pub fn gate_min_baseline_m() -> f64 { encf!(12.0) }
pub fn gate_max_fit_residual_m() -> f64 { encf!(0.42) }
pub fn gate_stability_frames() -> i64 { enci!(5) }
