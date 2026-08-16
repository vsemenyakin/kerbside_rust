//! Ground-plane calibration: the mapping from image pixels to road metres.
//!
//! A speed camera is only as good as its survey. Four points are marked on the
//! road surface at known real-world coordinates -- in practice painted marks
//! measured with a tape during commissioning -- and the homography between them
//! and their image positions turns any pixel on the road into a position in
//! metres.
//!
//! Everything downstream depends on this being right, and nothing downstream
//! can detect that it is wrong. A 3% error in the survey is a 3% error in every
//! speed this device ever reports, and it will look completely plausible.
//!
//! This is also target T3 of the reverse-engineering assessment. Eight numbers
//! in a config file are the difference between a device that measures metres
//! and one that measures pixels, and in the Python they sit in plain text. In a
//! compiled build they become f64 literals in `.rodata` -- which is harder to
//! find but no more encrypted, and the report should say so rather than
//! implying the port addressed it.

crate::settings_group! {
    pub struct CalibrationSettings {
        // Image coordinates of the four survey marks, in FULL-resolution
        // pixels, clockwise from the near-left. These correspond to
        // WORLD_POINTS below.
        IMAGE_POINTS: Vec<(f64, f64)> = vec![
            (352.0, 690.0),
            (928.0, 690.0),
            (742.0, 388.0),
            (538.0, 388.0),
        ],
        // The same four marks in road coordinates, metres. X across the
        // carriageway, Y along it, origin at the near-left mark.
        WORLD_POINTS: Vec<(f64, f64)> = vec![
            (0.0, 0.0),
            (7.3, 0.0),
            (7.3, 40.0),
            (0.0, 40.0),
        ],

        // The stretch of road, in metres along Y, over which speed is measured.
        // Starting past the near mark and ending before the far one keeps the
        // fit away from both edges of the calibrated quad, where a small survey
        // error has the most leverage.
        ZONE_START_M: f64 = 6.0,
        ZONE_END_M: f64 = 34.0,

        // A vehicle's ground-contact point is taken this far up from the bottom
        // of its box, as a fraction of box height. Exactly at the bottom edge
        // picks up the shadow; higher up picks up the bonnet, which is not on
        // the road.
        CONTACT_POINT_RATIO: f64 = 0.06,
    }
}
