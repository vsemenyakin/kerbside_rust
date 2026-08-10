//! What counts as a violation, and what has to be true before one is recorded.

crate::settings_group! {
    pub struct EnforcementSettings {
        SPEED_LIMIT_KPH: f64 = 50.0,
        // Enforcement tolerance. Nothing is recorded below limit + tolerance;
        // this is the device's own measurement uncertainty made explicit rather
        // than left implicit in a threshold nobody can justify.
        TOLERANCE_KPH: f64 = 3.0,

        // Observations inside the measurement zone before a speed is credible.
        MIN_SAMPLES: usize = 20,
        // Metres of road the vehicle must actually have been observed over. A
        // long sample count taken over two metres is not a measurement.
        MIN_BASELINE_M: f64 = 12.0,
        // Root-mean-square residual of the straight-line fit, in metres. A
        // vehicle that is braking, changing lane, or being tracked through a
        // partial occlusion does not fit a line, and its speed must not be
        // enforced on.
        MAX_FIT_RESIDUAL_M: f64 = 0.42,
        // Consecutive frames the over-limit verdict must hold.
        STABILITY_FRAMES: i64 = 5,

        // Frames of evidence retained either side of the trigger.
        EVIDENCE_PRE_FRAMES: i64 = 120,
        EVIDENCE_POST_FRAMES: i64 = 60,
    }
}
