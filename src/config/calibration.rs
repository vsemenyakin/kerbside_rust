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
        // The survey marks used to live here as encrypted `Vec` fields, but a
        // Vec field is decoded once into the long-lived Settings struct -- where
        // a core dump of the running process reads it straight back (RE target
        // T3; confirmed exploited via a heap scan for the point vectors). They
        // are now the transient accessors `survey_image_points` /
        // `survey_world_points` below: decoded from ciphertext into a stack array
        // only while the homography is built, then dropped. Persistent memory
        // then holds the derived 3x3 matrix, not the labelled point pairs.

        // The stretch of road, in metres along Y, over which speed is measured.
        // Starting past the near mark and ending before the far one keeps the
        // fit away from both edges of the calibrated quad, where a small survey
        // error has the most leverage.
        ZONE_START_M: f64 = crate::encf!(6.0),
        ZONE_END_M: f64 = crate::encf!(34.0),

        // A vehicle's ground-contact point is taken this far up from the bottom
        // of its box, as a fraction of box height. Exactly at the bottom edge
        // picks up the shadow; higher up picks up the bonnet, which is not on
        // the road.
        CONTACT_POINT_RATIO: f64 = crate::encf!(0.06),
    }
}

/// The four image survey marks (full-resolution pixels), decoded transiently.
///
/// Built from `encf!` ciphertext into a stack array at the moment the homography
/// is constructed, and dropped immediately after -- so the labelled survey never
/// persists in the heap the way a `Vec` settings field did. See the note in the
/// struct above and `crate::crypt` for the limit (a RAM dump timed into the
/// build call would still catch it; the derived matrix remains).
pub fn survey_image_points() -> [(f64, f64); 4] {
    [
        (crate::encf!(352.0), crate::encf!(690.0)),
        (crate::encf!(928.0), crate::encf!(690.0)),
        (crate::encf!(742.0), crate::encf!(388.0)),
        (crate::encf!(538.0), crate::encf!(388.0)),
    ]
}

/// The same four marks in road metres, decoded transiently. See
/// [`survey_image_points`].
pub fn survey_world_points() -> [(f64, f64); 4] {
    [
        (crate::encf!(0.0), crate::encf!(0.0)),
        (crate::encf!(7.3), crate::encf!(0.0)),
        (crate::encf!(7.3), crate::encf!(40.0)),
        (crate::encf!(0.0), crate::encf!(40.0)),
    ]
}
