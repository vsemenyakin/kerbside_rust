//! Turn a sequence of image positions into a speed in km/h.
//!
//! This is what the whole application exists to do, and it is worth being
//! precise about the chain, because every link is a place a plausible wrong
//! answer can be manufactured:
//!
//! ```text
//!     box  ->  contact point  ->  world position  ->  line fit  ->  km/h
//!      |           |                    |                 |
//!      |           |                    |                 +- sp_fit
//!      |           |                    +- sp_project (homography)
//!      |           +- bottom of the box, lifted slightly off the shadow
//!      +- from the background model
//! ```
//!
//! Why a least-squares fit rather than a filter, or rather than differencing
//! the endpoints:
//!
//! * **Endpoint differencing** uses two observations and throws away the eighty
//!   in between. Its error is the error of those two, and if either is the
//!   frame where the vehicle passed behind the pole, the answer is wrong with
//!   no way to tell.
//! * **A recursive filter** would smooth the estimate, which sounds desirable
//!   and is not: the published number would then be partly the filter's prior
//!   about how fast the vehicle ought to be going. For a measurement that gets
//!   enforced on, that is not acceptable.
//! * **A straight-line fit** uses every observation, weights them equally, and
//!   -- the decisive property -- **reports its own residual**. A vehicle that
//!   brakes, changes lane, or is tracked through a partial occlusion does not
//!   fit a line, and the residual says so. The gate can then decline to enforce
//!   rather than publishing a confident wrong number.
//!
//! The residual is the honesty of this module. Do not remove it, and do not let
//! a caller ignore it.

use crate::config::Settings;
use crate::geometry::{Box, Homography};
use crate::perf;
use crate::pipeline::types::Blob;
use crate::track::associator::Match;
use crate::track::types::{Observation, Vehicle};

/// The ground plane, built once at startup from the survey.
pub fn build_homography(settings: &Settings) -> Result<Homography, String> {
    let cal = &settings.calibration;
    Homography::from_survey(
        &cal.IMAGE_POINTS,
        &cal.WORLD_POINTS,
        settings.video.DOWNSCALE,
        (cal.ZONE_START_M, cal.ZONE_END_M),
    )
}

/// Projects observations onto the road and fits a speed to each vehicle.
pub struct SpeedMeasurer {
    homography: Homography,
    zone: (f64, f64),
    contact_ratio: f64,
    history: usize,
}

impl SpeedMeasurer {
    pub fn new(settings: &Settings) -> Result<Self, String> {
        // Construction-time reads: the survey and the zone define the
        // coordinate space, and cannot change without recalibrating.
        let cal = &settings.calibration;
        Ok(Self {
            homography: build_homography(settings)?,
            zone: (cal.ZONE_START_M, cal.ZONE_END_M),
            contact_ratio: cal.CONTACT_POINT_RATIO,
            history: settings.tracking.HISTORY_FRAMES,
        })
    }

    pub fn homography(&self) -> &Homography {
        &self.homography
    }

    /// Record one world observation per matched vehicle, then refit.
    ///
    /// The argument list is long because it mirrors the Python's, and every
    /// item on it is state the stage must not fetch for itself: the settings
    /// snapshot in particular has to be the one the frame started with.
    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        &self,
        frame_id: i64,
        t: f64,
        vehicles: &mut [Vehicle],
        matched: &[Match],
        blobs: &[Blob],
        settings: &Settings,
        pf: &mut perf::Frame,
    ) {
        pf.start("speed");

        pf.start("sp_project");
        for m in matched {
            let blob = match blobs.get(m.blob_index) {
                Some(blob) => blob,
                None => continue,
            };
            let index = match vehicles.iter().position(|v| v.vehicle_id == m.vehicle_id) {
                Some(index) => index,
                None => continue,
            };

            let contact = blob.box_.contact_point(self.contact_ratio);
            let (across, along) = self.homography.to_world(contact);
            if !across.is_finite() || !along.is_finite() {
                // On or above the horizon line. Not a position.
                continue;
            }
            let length = self.apparent_length(blob);
            let vehicle = &mut vehicles[index];
            if Self::is_a_jump(vehicle, t, along, settings) {
                // The track has almost certainly changed vehicle. Throw the
                // history away rather than fitting a line through two of them:
                // a measurement of nothing is recoverable, a confident
                // measurement of the wrong thing is not.
                vehicle.observations.clear();
                vehicle.speed_kph = 0.0;
                vehicle.fit_residual_m = 0.0;
                vehicle.baseline_m = 0.0;
            }
            vehicle.observations.push(Observation {
                frame_id,
                t,
                box_: blob.box_,
                across_m: across,
                along_m: along,
                coverage: m.coverage,
            });
            if vehicle.observations.len() > self.history {
                vehicle.observations.remove(0);
            }
            vehicle.in_zone = self.zone.0 <= along && along <= self.zone.1;
            vehicle.length_m = length;
        }
        pf.end("sp_project");

        pf.start("sp_fit");
        for m in matched {
            if let Some(index) = vehicles.iter().position(|v| v.vehicle_id == m.vehicle_id) {
                Self::fit(&mut vehicles[index], settings);
            }
        }
        pf.end("sp_fit");

        pf.end("speed");
    }

    /// Has this track just been handed a different vehicle?
    ///
    /// Extrapolates from the last two observations and compares. Two points
    /// rather than the full fit, deliberately: the full fit is dragged toward a
    /// swap that has already happened, so it would forgive the very event this
    /// is looking for.
    fn is_a_jump(vehicle: &Vehicle, t: f64, along: f64, settings: &Settings) -> bool {
        let n = vehicle.observations.len();
        if n < 3 {
            return false;
        }
        let previous = &vehicle.observations[n - 2];
        let last = &vehicle.observations[n - 1];
        let dt = last.t - previous.t;
        if dt <= 1e-9 {
            return false;
        }
        let rate = (last.along_m - previous.along_m) / dt;
        let predicted = last.along_m + rate * (t - last.t);
        (along - predicted).abs() > settings.tracking.MAX_JUMP_M
    }

    /// Vehicle length in metres, from its box through the ground plane.
    ///
    /// Measured along the road from the box's bottom-left to bottom-right
    /// corner is wrong -- that is the *width*. The length is the extent along
    /// the direction of travel, which in this view is mostly vertical in the
    /// image, so it comes from the near and far edges of the footprint.
    ///
    /// This is a geometric fact rather than a model output, which is why
    /// vehicle class is taken from here: it is auditable, and it degrades
    /// gracefully with distance instead of failing silently.
    fn apparent_length(&self, blob: &Blob) -> f64 {
        let b = &blob.box_;
        let near = self.homography.to_world(b.contact_point(self.contact_ratio));
        let far = self
            .homography
            .to_world(Box::new(b.x, b.y, b.w, b.h * 0.35).contact_point(0.0));
        if !near.1.is_finite() || !far.1.is_finite() {
            return 0.0;
        }
        (far.1 - near.1).abs()
    }

    /// Least-squares line through (time, distance along the road).
    ///
    /// Only observations inside the measurement zone are used. The zone starts
    /// past the near survey mark and ends before the far one, which keeps the
    /// fit away from both edges of the calibrated quad where a small survey
    /// error has the most leverage.
    fn fit(vehicle: &mut Vehicle, settings: &Settings) {
        let cal = &settings.calibration;
        let inside: Vec<&Observation> = vehicle
            .observations
            .iter()
            .filter(|o| cal.ZONE_START_M <= o.along_m && o.along_m <= cal.ZONE_END_M)
            .collect();
        if inside.len() < 3 {
            vehicle.speed_kph = 0.0;
            vehicle.fit_residual_m = 0.0;
            vehicle.baseline_m = 0.0;
            return;
        }

        let n = inside.len() as f64;
        // Centre before fitting. The times are seconds since the run started
        // and can be large; solving the normal equations on uncentred values
        // loses precision in the intercept for no reason.
        //
        // The sums are accumulated in source order, the same order numpy's
        // pairwise reduction visits for arrays this small, so the last bits
        // agree with the reference.
        let t_mean = inside.iter().map(|o| o.t).sum::<f64>() / n;
        let a_mean = inside.iter().map(|o| o.along_m).sum::<f64>() / n;
        let denominator: f64 = inside.iter().map(|o| (o.t - t_mean) * (o.t - t_mean)).sum();
        if denominator <= 1e-12 {
            vehicle.speed_kph = 0.0;
            vehicle.fit_residual_m = 0.0;
            vehicle.baseline_m = 0.0;
            return;
        }

        let numerator: f64 = inside
            .iter()
            .map(|o| (o.t - t_mean) * (o.along_m - a_mean))
            .sum();
        let slope = numerator / denominator;

        let sum_sq: f64 = inside
            .iter()
            .map(|o| {
                let residual = o.along_m - (a_mean + slope * (o.t - t_mean));
                residual * residual
            })
            .sum();

        let mut lowest = f64::INFINITY;
        let mut highest = f64::NEG_INFINITY;
        for o in &inside {
            lowest = lowest.min(o.along_m);
            highest = highest.max(o.along_m);
        }

        vehicle.speed_kph = slope.abs() * 3.6;
        vehicle.fit_residual_m = (sum_sq / n).sqrt();
        vehicle.baseline_m = highest - lowest;
    }
}
