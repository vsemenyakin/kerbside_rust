//! Camera geometry and capture cadence.

crate::settings_group! {
    /// Source geometry. Changing any of these invalidates the calibration.
    pub struct VideoSettings {
        // Colour, because vehicle appearance carries most of its information in
        // hue and a grayscale background model confuses a dark car with its own
        // shadow far more often.
        FRAME_WIDTH: i32 = 1280,
        FRAME_HEIGHT: i32 = 720,

        // 50 fps is not a luxury here. Speed is estimated by fitting a line to a
        // vehicle's world position over the measurement zone, so the standard
        // error of the fit falls with the square root of the sample count.
        // Halving the frame rate roughly doubles the speed uncertainty over the
        // same stretch of road, and the enforcement tolerance is what pays for
        // it.
        FPS: i32 = 50,

        // The pipeline works on a downscaled copy: 2.0 gives a 640x360 working
        // frame.
        //
        // This constant sets the native floor of the whole application. Every
        // heavy OpenCV call -- the resize, the background model, the morphology
        // -- costs in proportion to working-frame area, and together they are
        // the large majority of the frame budget. Halving the linear scale
        // quarters all of them at once.
        //
        // What it costs is measurement resolution at the far end of the zone,
        // where a vehicle is only a few pixels tall and its contact point is
        // quantised more coarsely. Measured against ground truth, 2.0 holds
        // speed accuracy within the enforcement tolerance; going further does
        // not.
        DOWNSCALE: f64 = 2.0,

        // Synthetic scene generation (source/road.rs, bin/make_clip.rs).
        SCENE_SEED: i64 = 7,
        SCENE_FRAMES: i64 = 1500,
    }
}
