//! What counts as a violation, and what has to be true before one is recorded.

crate::settings_group! {
    pub struct EnforcementSettings {
        SPEED_LIMIT_KPH: f64 = 50.0,
        // Enforcement tolerance. Nothing is recorded below limit + tolerance;
        // this is the device's own measurement uncertainty made explicit rather
        // than left implicit in a threshold nobody can justify.
        TOLERANCE_KPH: f64 = 3.0,

        // Gate thresholds (min samples, min baseline, max fit residual,
        // stability frames) moved to crate::tuning (fixed, never operator-set).

        // Frames of evidence retained either side of the trigger.
        EVIDENCE_PRE_FRAMES: i64 = 120,
        EVIDENCE_POST_FRAMES: i64 = 60,
    }
}
