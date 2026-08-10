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
        // Minimum IoU for a blob to continue an existing track. Vehicles are
        // large and the frame rate is high, so consecutive boxes overlap
        // heavily; a low gate here is how two adjacent lanes get swapped.
        MIN_IOU: f64 = 0.34,
        // A track is real after this many consecutive frames.
        CONFIRM_FRAMES: i64 = 4,
        // Coast this long through an occlusion (a sign gantry, a passing truck)
        // before the track is closed.
        MAX_MISSES: i64 = 10,

        // World positions retained per vehicle. At 50 fps this is 1.6 seconds,
        // comfortably longer than a vehicle takes to cross the measurement zone.
        HISTORY_FRAMES: usize = 80,
        // Exponential smoothing on the box size only. Position is never smoothed
        // -- see the module docstring.
        SIZE_EMA_ALPHA: f64 = 0.3,

        // Metres a new observation may deviate from the track's own
        // extrapolation before the history is discarded and measurement
        // restarts.
        //
        // This guards against the one failure that produces a *wrong accusation*
        // rather than a missed one. When association hands a track the wrong
        // vehicle -- two cars overlapping in the image, one passing behind the
        // other -- the fitted line then spans two vehicles, and its slope is
        // somewhere between their speeds. The residual does not necessarily
        // catch it: if the swap happens early, most samples come from the new
        // vehicle and the fit through them is excellent.
        //
        // At 50 fps a vehicle at the limit moves about 0.3 m per frame, so 1.5 m
        // is five frames of real motion -- comfortably beyond anything a genuine
        // vehicle does between frames, and far below the gap to a different one.
        MAX_JUMP_M: f64 = 1.5,
    }
}
