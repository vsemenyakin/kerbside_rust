//! Where a finished frame goes: the result CSV, and an optional overlay video.
//!
//! The CSV is the port's correctness oracle. Every field is a value some stage
//! computed, so two implementations agreeing on this file agree on the pipeline
//! -- not just on the final violation count, which is a handful of events per
//! clip and would hide almost every possible disagreement.
//!
//! On floating-point equality across platforms
//! -------------------------------------------
//! Bit-identical output across x86 and aarch64 is **not** guaranteed and is not
//! claimed. OpenCV and ONNX Runtime dispatch to different vectorised kernels per
//! architecture and differ in the low bits.
//!
//! Two levels of check, answering different questions:
//!
//! * **same machine** -- byte-identical. The determinism test runs the clip
//!   twice and compares. This catches any accidental dependence on the clock,
//!   on iteration order, or on uninitialised memory.
//! * **across ports** -- compared with tolerances by the Python's
//!   `tools/compare_runs.py`, which reports the largest deviation per column.
//!   That is the check this port has to pass.
//!
//! Formatting note
//! ---------------
//! Every number below is rendered with the same precision the Python uses, and
//! rows are terminated with CRLF because Python's `csv.writer` does that on
//! every platform. The running SHA-256 hashes the comma-joined row *without*
//! its terminator, exactly as the Python does, so the digests are directly
//! comparable even if a file gets its line endings mangled in transit.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use opencv::core::{Mat, Point as CvPoint, Scalar, Size, Vector};
use opencv::prelude::*;
use sha2::{Digest, Sha256};

use crate::config::Settings;
use crate::consumers::{Consumer, FrameOutput, SinkSummary};
use crate::enforce::Verdict;
use crate::geometry::Homography;
use crate::track::types::VehicleState;

/// Columns of the result CSV, in order. One row per frame, describing the
/// vehicle furthest through the measurement zone -- the one about to be judged.
pub const COLUMNS: [&str; 20] = [
    "frame_id",
    "n_blobs",
    "n_vehicles",
    "n_in_zone",
    "foreground_ratio",
    "inference_ran",
    "lead_id",
    "lead_x_m",
    "lead_y_m",
    "lead_speed_kph",
    "lead_residual_m",
    "lead_baseline_m",
    "lead_samples",
    "lead_length_m",
    "lead_coverage",
    "violation",
    "limit_kph",
    "excess_kph",
    "reason",
    "violations_total",
];

/// The vehicle furthest along the measurement zone, and its verdict.
///
/// One row per frame needs one subject. The furthest-along in-zone vehicle is
/// the one closest to being judged, so it is the one whose numbers matter --
/// and it is a stable choice frame to frame, which a "highest speed" or
/// "largest box" rule would not be.
fn lead(output: &FrameOutput) -> (Option<&VehicleState>, Option<&Verdict>) {
    let lead = output
        .vehicles
        .vehicles
        .iter()
        .filter(|v| v.in_zone)
        .max_by(|a, b| {
            a.along_m
                .partial_cmp(&b.along_m)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.vehicle_id.cmp(&b.vehicle_id))
        });
    let verdict = lead.and_then(|l| {
        output
            .verdicts
            .iter()
            .find(|w| w.vehicle_id == l.vehicle_id)
    });
    (lead, verdict)
}

/// One CSV row. Full precision -- rounding is the comparator's job.
pub fn row_for(output: &FrameOutput, running_violations: u64) -> Vec<String> {
    let (lead, verdict) = lead(output);
    let blank = String::new();
    vec![
        output.frame.frame_id.to_string(),
        output.result.blobs.len().to_string(),
        output.vehicles.vehicles.len().to_string(),
        output
            .vehicles
            .vehicles
            .iter()
            .filter(|v| v.in_zone)
            .count()
            .to_string(),
        format!("{:.6}", output.result.foreground_ratio),
        (output.result.inference_ran as i32).to_string(),
        lead.map(|l| l.vehicle_id).unwrap_or(-1).to_string(),
        lead.map(|l| format!("{:.4}", l.across_m)).unwrap_or(blank.clone()),
        lead.map(|l| format!("{:.4}", l.along_m)).unwrap_or(blank.clone()),
        lead.map(|l| format!("{:.4}", l.speed_kph)).unwrap_or(blank.clone()),
        lead.map(|l| format!("{:.5}", l.fit_residual_m)).unwrap_or(blank.clone()),
        lead.map(|l| format!("{:.4}", l.baseline_m)).unwrap_or(blank.clone()),
        lead.map(|l| l.samples.to_string()).unwrap_or(blank.clone()),
        lead.map(|l| format!("{:.4}", l.length_m)).unwrap_or(blank.clone()),
        lead.map(|l| format!("{:.4}", l.coverage)).unwrap_or(blank.clone()),
        (verdict.map(|w| w.violation).unwrap_or(false) as i32).to_string(),
        verdict.map(|w| format!("{:.2}", w.limit_kph)).unwrap_or(blank.clone()),
        verdict.map(|w| format!("{:.4}", w.excess_kph)).unwrap_or(blank.clone()),
        verdict.map(|w| w.reason().to_string()).unwrap_or(blank),
        running_violations.to_string(),
    ]
}

/// Streams result rows to a CSV and hashes them as it goes.
pub struct ResultWriter {
    path: PathBuf,
    file: Option<BufWriter<File>>,
    digest: Sha256,
    pub rows: u64,
    pub violations: u64,
}

impl ResultWriter {
    pub fn new(path: &str) -> Result<Self, String> {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
        }
        let mut file = BufWriter::new(
            File::create(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?,
        );
        // CRLF, because `csv.writer` uses it on every platform.
        write!(file, "{}\r\n", COLUMNS.join(","))
            .map_err(|e| format!("cannot write the CSV header: {e}"))?;
        Ok(Self {
            path,
            file: Some(file),
            digest: Sha256::new(),
            rows: 0,
            violations: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn close(&mut self) -> Result<String, String> {
        if let Some(mut file) = self.file.take() {
            file.flush()
                .map_err(|e| format!("cannot flush {}: {e}", self.path.display()))?;
        }
        let digest = std::mem::take(&mut self.digest);
        Ok(format!("{:x}", digest.finalize()))
    }
}

impl Consumer for ResultWriter {
    fn consume(&mut self, output: &Arc<FrameOutput>) -> Result<(), String> {
        self.violations += output.violation_count() as u64;
        let row = row_for(output, self.violations);
        let joined = row.join(",");
        if let Some(file) = self.file.as_mut() {
            write!(file, "{joined}\r\n").map_err(|e| format!("cannot write a result row: {e}"))?;
        }
        // Hash the canonical text form, not the float values: it is the file
        // that gets compared, so it is the file that should be hashed.
        self.digest.update(joined.as_bytes());
        self.rows += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<Option<SinkSummary>, String> {
        let digest = self.close()?;
        Ok(Some(SinkSummary {
            path: self.path.display().to_string(),
            rows: self.rows,
            violations: self.violations,
            digest,
        }))
    }
}

/// Renders a human-readable overlay video. Off by default.
///
/// Purely for looking at. It runs on the pipeline thread, so it is not part of
/// a benchmark run -- the `bench` profile forces it off.
pub struct OverlayWriter {
    writer: opencv::videoio::VideoWriter,
    scale: f64,
    zone: Vector<CvPoint>,
}

impl OverlayWriter {
    pub fn new(settings: &Settings, path: &str, homography: &Homography) -> Result<Self, String> {
        let vid = &settings.video;
        let cal = &settings.calibration;
        let scale = vid.DOWNSCALE;
        let zone: Vector<CvPoint> = homography
            .zone_polygon((0.2, 7.1), (cal.ZONE_START_M, cal.ZONE_END_M))
            .iter()
            .map(|p| CvPoint::new((p.x * scale) as i32, (p.y * scale) as i32))
            .collect();

        let fourcc = opencv::videoio::VideoWriter::fourcc('m', 'p', '4', 'v')
            .map_err(|e| format!("fourcc: {e}"))?;
        let writer = opencv::videoio::VideoWriter::new(
            path,
            fourcc,
            vid.FPS as f64,
            Size::new(vid.FRAME_WIDTH, vid.FRAME_HEIGHT),
            true,
        )
        .map_err(|e| format!("cannot open video writer for {path:?}: {e}"))?;
        if !writer
            .is_opened()
            .map_err(|e| format!("video writer: {e}"))?
        {
            return Err(format!("cannot open video writer for {path:?}"));
        }
        Ok(Self {
            writer,
            scale,
            zone,
        })
    }

    pub fn close(&mut self) -> Result<(), String> {
        self.writer
            .release()
            .map_err(|e| format!("cannot close the overlay: {e}"))
    }
}

impl Consumer for OverlayWriter {
    fn consume(&mut self, output: &Arc<FrameOutput>) -> Result<(), String> {
        let mut canvas = output
            .frame
            .full
            .get()
            .clone();

        // The measurement zone, in the operator's own coordinates.
        opencv::imgproc::polylines(
            &mut canvas,
            &self.zone,
            true,
            Scalar::new(0.0, 200.0, 255.0, 0.0),
            2,
            opencv::imgproc::LINE_8,
            0,
        )
        .map_err(|e| format!("cannot draw the zone: {e}"))?;

        for state in &output.vehicles.vehicles {
            let verdict = output
                .verdicts
                .iter()
                .find(|w| w.vehicle_id == state.vehicle_id);
            let colour = match verdict {
                Some(v) if v.violation => Scalar::new(0.0, 0.0, 255.0, 0.0),
                _ if state.in_zone && state.speed_kph > 0.0 => Scalar::new(0.0, 255.0, 0.0, 0.0),
                _ => Scalar::new(170.0, 170.0, 170.0, 0.0),
            };
            let x = (state.box_.x * self.scale) as i32;
            let y = (state.box_.y * self.scale) as i32;
            let w = (state.box_.w * self.scale) as i32;
            let h = (state.box_.h * self.scale) as i32;
            opencv::imgproc::rectangle(
                &mut canvas,
                opencv::core::Rect::new(x, y, w, h),
                colour,
                2,
                opencv::imgproc::LINE_8,
                0,
            )
            .map_err(|e| format!("cannot draw a vehicle box: {e}"))?;

            if state.speed_kph > 0.0 {
                // Speed only. The measured length is deliberately not shown: it
                // is not accurate enough to enforce on (see
                // `enforce::gate::limit_for`), and a class printed on an
                // evidence frame reads as a finding rather than an estimate. It
                // stays in the evidence record, where it carries its provenance
                // with it.
                opencv::imgproc::put_text(
                    &mut canvas,
                    &format!("{:.0} km/h", state.speed_kph),
                    CvPoint::new(x, i32::max(14, y - 6)),
                    opencv::imgproc::FONT_HERSHEY_SIMPLEX,
                    0.5,
                    colour,
                    1,
                    opencv::imgproc::LINE_AA,
                    false,
                )
                .map_err(|e| format!("cannot label a vehicle: {e}"))?;
            }
        }

        let worst = output.violations().max_by(|a, b| {
            a.excess_kph
                .partial_cmp(&b.excess_kph)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some(worst) = worst {
            opencv::imgproc::put_text(
                &mut canvas,
                &format!(
                    "VIOLATION  {:.0} in a {:.0}",
                    worst.speed_kph, worst.limit_kph
                ),
                CvPoint::new(24, 46),
                opencv::imgproc::FONT_HERSHEY_SIMPLEX,
                1.0,
                Scalar::new(0.0, 0.0, 255.0, 0.0),
                2,
                opencv::imgproc::LINE_AA,
                false,
            )
            .map_err(|e| format!("cannot draw the violation banner: {e}"))?;
        }
        opencv::imgproc::put_text(
            &mut canvas,
            &format!(
                "f{}  blobs {}  vehicles {}",
                output.frame.frame_id,
                output.result.blobs.len(),
                output.vehicles.vehicles.len()
            ),
            CvPoint::new(24, 80),
            opencv::imgproc::FONT_HERSHEY_SIMPLEX,
            0.6,
            Scalar::new(235.0, 235.0, 235.0, 0.0),
            1,
            opencv::imgproc::LINE_AA,
            false,
        )
        .map_err(|e| format!("cannot draw the frame caption: {e}"))?;

        self.writer
            .write(&canvas)
            .map_err(|e| format!("cannot write an overlay frame: {e}"))
    }

    fn finish(&mut self) -> Result<Option<SinkSummary>, String> {
        self.close()?;
        Ok(None)
    }
}

/// SHA-256 of a frame's bytes. Proves the source is reproducible.
pub fn frame_hash(frame: &Mat) -> Result<String, String> {
    let bytes = frame
        .data_bytes()
        .map_err(|e| format!("frame is not contiguous: {e}"))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}
