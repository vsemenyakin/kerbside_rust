//! Turn a foreground mask into candidate vehicles.
//!
//! `findContours` is native and fast. Everything after it is a **loop with
//! OpenCV calls inside**, and it scales with how busy the scene is rather than
//! with frame size. That makes it the stage whose cost tracks traffic density,
//! and the first place a compiled port shows a benefit that grows with load.
//!
//! Do not vectorise the filter loop. It is one of the few genuinely
//! interpreter-bound stages in the Python, and it is here to be measured; the
//! comparison is only meaningful if both implementations are doing the same
//! work per blob.
//!
//! Porting note
//! ------------
//! The Python's `cv2.findContours` hands back numpy arrays that are cheap to
//! slice and pass around, and both the blob filter and the evidence record lean
//! on that. Here it hands back `Vector<Vector<Point>>`, an owned OpenCV
//! container whose elements cross the FFI boundary one at a time. Each contour
//! is therefore copied once, into a plain `Vec<(i32, i32)>`, at extraction --
//! which is the cheapest honest translation: the record needs owned points
//! anyway, and repeatedly indexing the OpenCV vector later would pay the
//! crossing per point per frame.

use opencv::core::{Mat, Point as CvPoint, Vector};

use crate::config::Settings;
use crate::geometry::Box;
use crate::perf;

use super::types::Blob;

/// Extracts and filters foreground regions.
pub struct BlobFinder;

impl BlobFinder {
    pub fn new(_settings: &Settings, _shape: (i32, i32)) -> Self {
        Self
    }

    pub fn find(
        &self,
        mask: &Mat,
        _settings: &Settings,
        pf: &mut perf::Frame,
    ) -> Result<Vec<Blob>, String> {
        pf.start(crate::perf::stage::BLOBS);
        let result = (|| -> Result<Vec<Blob>, String> {
            pf.start(crate::perf::stage::BL_FIND);
            let mut contours: Vector<Vector<CvPoint>> = Vector::new();
            opencv::imgproc::find_contours(
                mask,
                &mut contours,
                opencv::imgproc::RETR_EXTERNAL,
                opencv::imgproc::CHAIN_APPROX_SIMPLE,
                CvPoint::new(0, 0),
            )
            .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot extract contours: ")))?;
            pf.end(crate::perf::stage::BL_FIND);

            pf.start(crate::perf::stage::BL_FILTER);
            let mut found: Vec<Blob> = Vec::new();
            let min_area = crate::tuning::blob_min_area();
            let max_area = crate::tuning::blob_max_area();
            let min_aspect = crate::tuning::blob_min_aspect();
            let max_aspect = crate::tuning::blob_max_aspect();
            let min_fill = crate::tuning::blob_min_fill();
            for contour in contours.iter() {
                let area = opencv::imgproc::contour_area_def(&contour)
                    .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot measure a contour: ")))?;
                if area < min_area as f64 || area > max_area as f64 {
                    continue;
                }
                let rect = opencv::imgproc::bounding_rect(&contour)
                    .map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot bound a contour: ")))?;
                if rect.height <= 0 || rect.width <= 0 {
                    continue;
                }

                let aspect = rect.width as f64 / rect.height as f64;
                if aspect < min_aspect || aspect > max_aspect {
                    continue;
                }

                // Fill: contour area over bounding-box area. A vehicle is
                // roughly convex and fills most of its box. A shadow trailing
                // off diagonally, or two vehicles bridged by one, does not --
                // and a merged pair is the single worst input the speed stage
                // can receive, because its box straddles both and its bottom
                // edge belongs to neither.
                let fill = area / (rect.width as f64 * rect.height as f64);
                if fill < min_fill {
                    continue;
                }

                found.push(Blob {
                    box_: Box::new(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                    ),
                    area,
                    fill,
                    contour: contour.iter().map(|p| (p.x, p.y)).collect(),
                });
            }
            pf.end(crate::perf::stage::BL_FILTER);
            Ok(found)
        })();
        pf.end(crate::perf::stage::BLOBS);
        result
    }
}
