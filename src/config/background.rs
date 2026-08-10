//! Background model and foreground clean-up.
//!
//! A fixed camera lets the scene be learned rather than differenced. That is
//! the central simplification of this application: with the camera bolted to a
//! pole, "what does this pixel normally look like" is a well-posed question,
//! and the answer separates vehicles from road far better than comparing
//! consecutive frames ever could.
//!
//! The cost is that the model has to be *maintained* -- it adapts to light, and
//! anything that stops moving is absorbed into it.

crate::settings_group! {
    pub struct BackgroundSettings {
        // Frames of history in the mixture model. At 50 fps this is ~12 seconds:
        // long enough that a queue of slow traffic does not get absorbed into
        // the background, short enough to follow cloud shadow crossing the road.
        HISTORY: i32 = 600,
        // Mahalanobis threshold for "this pixel does not match its background".
        VAR_THRESHOLD: f64 = 28.0,
        // Shadow detection on. A vehicle's shadow is the single biggest source
        // of over-large blobs, and an over-large blob moves its own
        // ground-contact point forward, which reads directly as extra speed.
        DETECT_SHADOWS: bool = true,
        // Shadows come back as this value in the mask; everything else is 255.
        SHADOW_VALUE: i32 = 127,
        LEARNING_RATE: f64 = -1.0,  // -1 lets OpenCV pick from HISTORY

        // Morphological clean-up: open to drop speckle, close to seal a vehicle
        // split by its own windscreen reflection.
        OPEN_KERNEL: i32 = 5,
        CLOSE_KERNEL: i32 = 9,

        // Blob acceptance, in working-frame pixels.
        MIN_AREA: i64 = 135,
        MAX_AREA: i64 = 14_600,
        // Vehicles are wider than tall in this view, but a motorcycle is not, so
        // the band is generous. Beyond it the blob is a shadow smear or two
        // vehicles merged.
        MIN_ASPECT: f64 = 0.35,
        MAX_ASPECT: f64 = 4.5,
        // Contour area over bounding-box area. A vehicle is roughly convex; a
        // merged pair or a shadow trail is not.
        MIN_FILL: f64 = 0.42,
    }
}
