//! Learn what the empty road looks like, and report what is not it.
//!
//! A fixed camera makes "what does this pixel normally look like" a well-posed
//! question, and OpenCV's MOG2 answers it per pixel with a small mixture of
//! Gaussians. Anything that does not fit its pixel's mixture is foreground.
//!
//! This is the dominant native cost of the frame, and it is the whole reason
//! this port exists in the form it does: `apply()` is a single call into
//! OpenCV's C++, and it costs exactly the same from here as it does from
//! Python. Nothing in this file is faster because it is written in Rust. The
//! only levers are the working resolution and the model's parameters -- both
//! available in Python today.
//!
//! Two behaviours to know before changing anything:
//!
//! * **Anything that stops moving is absorbed.** A vehicle stopped in traffic
//!   fades into the background over roughly `HISTORY` frames and then
//!   disappears from the foreground mask entirely. That is correct for this
//!   application -- a stationary vehicle has no speed to measure -- but it is
//!   surprising the first time a track evaporates in a queue.
//! * **Shadows are detected, not removed.** MOG2 marks them with a distinct
//!   value rather than dropping them, because the decision belongs to the
//!   caller. Here they are dropped: a shadow attached to a vehicle drags the
//!   bottom of its bounding box forward along the road, and the bottom of the
//!   box *is* the measurement. Shadow leakage does not blur the speed, it
//!   biases it.
//!
//! Porting note
//! ------------
//! The model is **stateful**: it carries a learned mixture per pixel, so it is
//! not a pure function of the frame but a function of every frame before it.
//! Reproducing the reference CSV requires feeding frames in the same order from
//! the same start, and any port that parallelises across frames will silently
//! diverge. That is why the pipeline below is a single thread and the mailbox
//! is one slot deep rather than a work queue.

use opencv::core::{Mat, Point as CvPoint, Scalar, Size};
use opencv::prelude::*;
use opencv::video::BackgroundSubtractorMOG2;

use crate::config::Settings;
use crate::perf;

/// Owns the mixture model and the morphological clean-up.
pub struct BackgroundModel {
    subtractor: opencv::core::Ptr<BackgroundSubtractorMOG2>,
    learning_rate: f64,
    shadow_value: i32,
    open_kernel: Mat,
    close_kernel: Mat,
    pixels: f64,
}

impl BackgroundModel {
    pub fn new(settings: &Settings, shape: (i32, i32)) -> Result<Self, String> {
        let bg = &settings.background;
        let (height, width) = shape;

        // Construction-time reads. These define the model's memory and the
        // coordinate space blob areas are measured in; neither can change
        // without rebuilding the stage, which is why neither may be VOLATILE.
        let subtractor = opencv::video::create_background_subtractor_mog2(
            bg.HISTORY,
            bg.VAR_THRESHOLD,
            bg.DETECT_SHADOWS,
        )
        .map_err(|e| format!("cannot create the background subtractor: {e}"))?;

        let open_kernel = opencv::imgproc::get_structuring_element(
            opencv::imgproc::MORPH_ELLIPSE,
            Size::new(bg.OPEN_KERNEL, bg.OPEN_KERNEL),
            CvPoint::new(-1, -1),
        )
        .map_err(|e| format!("cannot build the opening kernel: {e}"))?;
        let close_kernel = opencv::imgproc::get_structuring_element(
            opencv::imgproc::MORPH_ELLIPSE,
            Size::new(bg.CLOSE_KERNEL, bg.CLOSE_KERNEL),
            CvPoint::new(-1, -1),
        )
        .map_err(|e| format!("cannot build the closing kernel: {e}"))?;

        Ok(Self {
            subtractor,
            learning_rate: bg.LEARNING_RATE,
            shadow_value: bg.SHADOW_VALUE,
            open_kernel,
            close_kernel,
            pixels: (height as f64) * (width as f64),
        })
    }

    /// Frames before the model is worth believing.
    ///
    /// Until the mixtures have seen enough of the empty road, most of the frame
    /// reads as foreground. Reporting speeds during that period would not be
    /// wrong so much as meaningless, so the pipeline suppresses enforcement
    /// until it has passed.
    pub fn warm_frames(&self, settings: &Settings) -> i64 {
        i64::max(50, (settings.background.HISTORY / 6) as i64)
    }

    /// Return the cleaned foreground mask and the fraction of it that is set.
    ///
    /// The ratio is not decoration. A sudden jump -- headlights sweeping the
    /// scene, the camera being knocked, an exposure step -- puts most of the
    /// frame into the foreground, and every blob found in that frame is
    /// nonsense. Callers use it to refuse the frame outright rather than trying
    /// to track the resulting garbage.
    pub fn apply(&mut self, working: &Mat, pf: &mut perf::Frame) -> Result<(Mat, f64), String> {
        let mut mask = Mat::default();
        pf.start("bg");
        // Disambiguated explicitly: `Ptr<BackgroundSubtractorMOG2>` inherits an
        // `apply` from both the base and the MOG2 trait. The Python calls the
        // MOG2 one, which is the one that honours `learningRate`.
        opencv::prelude::BackgroundSubtractorMOG2Trait::apply(
            &mut self.subtractor,
            working,
            &mut mask,
            self.learning_rate,
        )
        .map_err(|e| format!("the background model failed: {e}"))?;
        pf.end("bg");

        pf.start("morph");
        // Shadows first: MOG2 marks them with a value of its own, so they are
        // removed by thresholding rather than by a morphological guess.
        if self.shadow_value != 0 {
            let mut thresholded = Mat::default();
            opencv::imgproc::threshold(
                &mask,
                &mut thresholded,
                self.shadow_value as f64,
                255.0,
                opencv::imgproc::THRESH_BINARY,
            )
            .map_err(|e| format!("cannot threshold the shadow value: {e}"))?;
            mask = thresholded;
        }
        // Open then close, in that order. Opening first removes speckle so the
        // close does not weld it into the vehicle; closing first would seal the
        // speckle in and then the open cannot reach it.
        //
        // A fresh output buffer per call, as the Python gets from numpy. Reusing
        // scratch buffers here would be an optimisation the reference does not
        // have, and the point of this exercise is to compare like with like.
        let mut opened = Mat::default();
        opencv::imgproc::morphology_ex_def(
            &mask,
            &mut opened,
            opencv::imgproc::MORPH_OPEN,
            &self.open_kernel,
        )
        .map_err(|e| format!("cannot open the foreground mask: {e}"))?;
        let mut closed = Mat::default();
        opencv::imgproc::morphology_ex_def(
            &opened,
            &mut closed,
            opencv::imgproc::MORPH_CLOSE,
            &self.close_kernel,
        )
        .map_err(|e| format!("cannot close the foreground mask: {e}"))?;
        mask = closed;
        pf.end("morph");

        let set = opencv::core::count_non_zero(&mask)
            .map_err(|e| format!("cannot measure the foreground: {e}"))?;
        Ok((mask, set as f64 / self.pixels))
    }

    /// The learned background, for the overlay. Never on the hot path.
    pub fn background_image(&self) -> Option<Mat> {
        let mut image = Mat::default();
        match self.subtractor.get_background_image(&mut image) {
            Ok(()) => Some(image),
            Err(_) => None,
        }
    }
}

/// Full-resolution frame -> working frame.
///
/// INTER_AREA, always. The background model is comparing a pixel against its
/// own learned distribution, and a resampling filter that rings -- as the
/// linear and cubic ones do on a hard edge -- injects variance the model then
/// has to absorb, which widens every mixture and costs foreground sensitivity
/// at exactly the distances where vehicles are smallest.
pub fn to_working(full: &Mat, width: i32, height: i32) -> Result<Mat, String> {
    let mut working = Mat::default();
    opencv::imgproc::resize(
        full,
        &mut working,
        Size::new(width, height),
        0.0,
        0.0,
        opencv::imgproc::INTER_AREA,
    )
    .map_err(|e| format!("cannot build the working frame: {e}"))?;
    Ok(working)
}

/// An empty single-channel mask, for the frames that are refused outright.
pub fn empty_mask(height: i32, width: i32) -> Result<Mat, String> {
    Mat::new_rows_cols_with_default(height, width, opencv::core::CV_8UC1, Scalar::all(0.0))
        .map_err(|e| format!("cannot allocate an empty mask: {e}"))
}
