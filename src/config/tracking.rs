//! Vehicle association and lifecycle.
//!
//! No motion filter. Association is by bounding-box overlap between consecutive
//! frames, and speed comes from a fit over the retained history rather than
//! from a recursive estimator.
//!
//! That is a deliberate choice, not a simplification for its own sake: a
//! constant-velocity filter would smooth the very quantity being measured, and
//! smoothing a speed estimate before enforcing on it means the number in the
//! evidence packet is partly the filter's opinion. A least-squares fit over raw
//! observations can state its own residual, and the gate can refuse when that
//! residual is too high.

crate::settings_group! {
    pub struct TrackingSettings {
        MAX_VEHICLES: usize = 12,
        // Association IoU gate, confirmation count and coast-before-close count
        // moved to crate::tuning (fixed, never operator-set).

        // World positions retained per vehicle. At 50 fps this is 1.6 seconds,
        // comfortably longer than a vehicle takes to cross the measurement zone.
        HISTORY_FRAMES: usize = 80,
        // Exponential smoothing on the box size only. Position is never smoothed
        // -- see the module docstring.
        SIZE_EMA_ALPHA: f64 = 0.3,

        // MAX_JUMP_M (track-swap guard) moved to crate::tuning.
    }
}
