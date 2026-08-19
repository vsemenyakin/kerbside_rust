//! Decide which foreground blobs are vehicles.
//!
//! The background model says *where* something is; the likelihood map says
//! *what*. This module joins them: for each blob, look at the region of the
//! likelihood map its box covers, and ask what fraction of that region reads as
//! vehicle.
//!
//! ```text
//!     sc_sample   a **loop over blobs**, two small reductions each.
//!                 Interpreter-bound in the Python: the arrays are a few dozen
//!                 cells, far too small for the vectorised work to outweigh the
//!                 dispatch, which is exactly why the cost scales with traffic
//!                 density rather than with frame size. This is one of the
//!                 stages a compiled port is expected to actually remove.
//! ```
//!
//! Coverage rather than mean likelihood. A mean is dragged down by the road
//! visible around a vehicle inside its own bounding box -- badly at the far
//! end, where a box is mostly gaps -- and dragged up by a single strongly-firing
//! cell on a blob that is otherwise shadow. The fraction of cells that clear
//! the threshold asks the question that actually matters, and answers it the
//! same way at both ends of the zone.

use ndarray::Array2;

use crate::config::Settings;
use crate::perf;
use crate::pipeline::types::Blob;

/// Vehicle coverage for each blob, in the same order.
///
/// Returns 1.0 for every blob when there is no likelihood map yet. That is a
/// deliberate fail-open: the model runs on a duty cycle and the first frames of
/// a run have no map at all, and a fail-closed default would silently drop
/// every vehicle until the first inference landed -- during which the system
/// would look like it was working perfectly and be measuring nothing.
pub fn score_blobs(
    likelihood: Option<&Array2<f32>>,
    blobs: &[Blob],
    working_shape: (i32, i32),
    _settings: &Settings,
    pf: &mut perf::Frame,
) -> Vec<f64> {
    let likelihood = match likelihood {
        Some(map) if !blobs.is_empty() => map,
        _ => return vec![1.0; blobs.len()],
    };

    let (work_h, work_w) = working_shape;
    let (grid_h, grid_w) = (likelihood.shape()[0], likelihood.shape()[1]);

    // Working-frame pixels -> likelihood-map cells.
    let sx = grid_w as f64 / work_w as f64;
    let sy = grid_h as f64 / work_h as f64;

    pf.start(crate::perf::stage::SCORE);
    pf.start(crate::perf::stage::SC_SAMPLE);
    let mut scores = Vec::with_capacity(blobs.len());
    let threshold = crate::tuning::vehicle_threshold() as f32;
    for blob in blobs {
        let b = &blob.box_;
        let x0 = f64::max(0.0, (b.x * sx).floor()) as usize;
        let y0 = f64::max(0.0, (b.y * sy).floor()) as usize;
        let x1 = f64::min(grid_w as f64, ((b.x + b.w) * sx).ceil()) as usize;
        let y1 = f64::min(grid_h as f64, ((b.y + b.h) * sy).ceil()) as usize;
        if x1 <= x0 || y1 <= y0 {
            // Smaller than one cell of the map. Too far away for the model to
            // have an opinion, so do not manufacture one -- pass it through and
            // let the size and speed gates decide.
            scores.push(1.0);
            continue;
        }
        let mut over = 0usize;
        for y in y0..y1 {
            for x in x0..x1 {
                if likelihood[[y, x]] > threshold {
                    over += 1;
                }
            }
        }
        let cells = (y1 - y0) * (x1 - x0);
        scores.push(over as f64 / cells as f64);
    }
    pf.end(crate::perf::stage::SC_SAMPLE);
    pf.end(crate::perf::stage::SCORE);
    scores
}

/// Which blobs survive as vehicles.
pub fn accept(scores: &[f64], _settings: &Settings) -> Vec<bool> {
    let floor = crate::tuning::min_coverage();
    scores.iter().map(|score| *score >= floor).collect()
}
