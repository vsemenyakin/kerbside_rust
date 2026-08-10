//! Follow each vehicle from frame to frame.
//!
//! Stage breakdown, all three measured separately:
//!
//! ```text
//!     as_score   every (blob, vehicle) pair scored by box overlap -- a nested
//!                loop, O(N*M)
//!     as_pick    greedy assignment over the sorted scores
//!     as_life    confirm, coast, retire, spawn
//! ```
//!
//! Not one of these calls into a heavyweight native routine. This is the part
//! of the pipeline that is genuinely interpreter-bound in the Python, and its
//! cost scales with traffic rather than with frame size -- which makes it the
//! clearest place for a compiled port to show a win.
//!
//! **There is no motion filter here, and that is deliberate.** A
//! constant-velocity Kalman filter would smooth the very quantity being
//! measured, so the number in the evidence packet would be partly the filter's
//! opinion about how fast the vehicle ought to be going. Positions are recorded
//! raw and the speed comes from a least-squares fit over them, which can state
//! its own residual -- and the enforcement gate can refuse when that residual
//! is too high. A filter cannot offer that: it always has an answer, and it
//! never says how much of the answer is its own prior.

use std::collections::VecDeque;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Settings;
use crate::perf;
use crate::pipeline::types::Blob;

use super::types::{Diagnostics, FrameVehicles, Vehicle, VehicleHistory};

/// One blob matched to one existing track.
///
/// The Python hands back the `Vehicle` object itself. Here it hands back the
/// id, because a `&mut Vehicle` cannot outlive the borrow of the list it lives
/// in, and the list is edited (retired, extended) before the measurer runs.
/// Resolving by id is an O(MAX_VEHICLES) scan over at most twelve entries --
/// cheaper than the allocation the Python does to build the same list.
#[derive(Debug, Clone, Copy)]
pub struct Match {
    pub vehicle_id: i64,
    pub blob_index: usize,
    pub coverage: f64,
}

pub struct Associator {
    frames: VecDeque<FrameVehicles>,
    depth: usize,
    vehicles: Vec<Vehicle>,
    next_id: i64,
    /// Published snapshot. Replaced once per frame, never mutated in place.
    ///
    /// Other threads load this and hold the `Arc` for as long as they like, and
    /// that is safe *because* the pointed-to value is never edited. The Python
    /// gets the same guarantee from rebinding one attribute under the
    /// interpreter lock; this spells it out.
    pub history: ArcSwap<VehicleHistory>,
}

impl Associator {
    pub fn new(_settings: &Settings) -> Self {
        // Construction-time read: the history depth sizes the ring below.
        let depth = 8;
        Self {
            frames: VecDeque::with_capacity(depth),
            depth,
            vehicles: Vec::new(),
            next_id: 1,
            history: ArcSwap::from_pointee(VehicleHistory::default()),
        }
    }

    /// Live vehicles. Pipeline thread only -- these objects are mutable.
    pub fn vehicles(&self) -> &[Vehicle] {
        &self.vehicles
    }

    pub fn vehicles_mut(&mut self) -> &mut [Vehicle] {
        &mut self.vehicles
    }

    /// Match blobs to vehicles; return the matched triples for measurement.
    pub fn update(
        &mut self,
        _frame_id: i64,
        blobs: &[Blob],
        coverage: &[f64],
        settings: &Settings,
        pf: &mut perf::Frame,
    ) -> Vec<Match> {
        let trk = &settings.tracking;

        pf.start("assoc");

        pf.start("as_score");
        let mut scored: Vec<(f64, usize, usize)> = Vec::new();
        for (bi, blob) in blobs.iter().enumerate() {
            for (vi, vehicle) in self.vehicles.iter().enumerate() {
                let overlap = blob.box_.iou(&vehicle.box_);
                if overlap >= trk.MIN_IOU {
                    // Negated so a plain ascending sort puts the best first.
                    scored.push((-overlap, bi, vi));
                }
            }
        }
        pf.end("as_score");

        pf.start("as_pick");
        // The Python sorts tuples, so ties on overlap fall back to blob index
        // and then vehicle index. Reproducing that ordering exactly is what
        // keeps two implementations assigning the same blob to the same track
        // when two overlaps are equal -- which happens more often than it
        // sounds, because equal-area boxes give bit-identical IoUs.
        scored.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
                .then(a.2.cmp(&b.2))
        });

        let mut used_blobs: Vec<bool> = vec![false; blobs.len()];
        let mut used_vehicles: Vec<bool> = vec![false; self.vehicles.len()];
        let mut matched: Vec<Match> = Vec::new();
        for (_score, bi, vi) in &scored {
            if used_blobs[*bi] || used_vehicles[*vi] {
                continue;
            }
            used_blobs[*bi] = true;
            used_vehicles[*vi] = true;
            let coverage_value = coverage.get(*bi).copied().unwrap_or(1.0);
            let vehicle = &mut self.vehicles[*vi];
            vehicle.box_ = blobs[*bi].box_;
            vehicle.coverage = coverage_value;
            vehicle.hits += 1;
            vehicle.misses = 0;
            if !vehicle.confirmed && vehicle.hits >= trk.CONFIRM_FRAMES {
                vehicle.confirmed = true;
            }
            matched.push(Match {
                vehicle_id: vehicle.vehicle_id,
                blob_index: *bi,
                coverage: coverage_value,
            });
        }
        pf.end("as_pick");

        pf.start("as_life");
        for (vi, vehicle) in self.vehicles.iter_mut().enumerate() {
            vehicle.age += 1;
            if !used_vehicles[vi] {
                vehicle.misses += 1;
            }
        }
        // Coasting carries a vehicle through the roadside pole and through a
        // frame where it merged with the vehicle beside it. Retiring
        // immediately would split one vehicle into two measurements, each over
        // half the baseline and both therefore rejected.
        let max_misses = trk.MAX_MISSES;
        self.vehicles.retain(|v| v.misses <= max_misses);

        for (bi, blob) in blobs.iter().enumerate() {
            if used_blobs[bi] {
                continue;
            }
            if self.vehicles.len() >= trk.MAX_VEHICLES {
                break;
            }
            self.vehicles.push(Vehicle::new(
                self.next_id,
                blob.box_,
                coverage.get(bi).copied().unwrap_or(1.0),
            ));
            self.next_id += 1;
        }
        pf.end("as_life");

        pf.end("assoc");
        matched
    }

    /// Publish this frame's state and rebuild the history snapshot.
    ///
    /// ---------------------------------------------------------------------
    /// THIS IS A DELIBERATE ALLOCATION SITE. DO NOT OPTIMISE IT.
    /// ---------------------------------------------------------------------
    /// Every frame this builds one immutable `VehicleState` per live vehicle,
    /// one `FrameVehicles`, a diagnostics record with its own vector of ids,
    /// and a fresh `VehicleHistory` over the whole ring. It could obviously be
    /// cheaper -- an `Arc` per frame reused across the ring, say.
    ///
    /// It is written this way because that is what buys lock-free reads from
    /// three threads, and because the resulting allocation rate is a direct
    /// contributor to the Python's garbage-collection pauses -- which are its
    /// worst latency events by a wide margin. A port that quietly removed the
    /// churn would measure a machine with no tail and conclude, wrongly, that
    /// the compiled build fixed something it merely never had.
    ///
    /// The honest comparison is: same allocations, same retention, no tracing
    /// collector. Removing the allocations as well would answer a different
    /// question.
    pub fn snapshot(&mut self, frame_id: i64) -> FrameVehicles {
        let states: Vec<_> = self.vehicles.iter().map(|v| v.to_state()).collect();
        let diagnostics = Diagnostics {
            n_vehicles: states.len(),
            n_confirmed: states.iter().filter(|s| s.confirmed).count(),
            n_in_zone: states.iter().filter(|s| s.in_zone).count(),
            ids: states.iter().map(|s| s.vehicle_id).collect(),
        };
        let frame = FrameVehicles {
            frame_id,
            vehicles: states,
            diagnostics,
        };
        if self.frames.len() == self.depth {
            self.frames.pop_front();
        }
        self.frames.push_back(frame.clone());
        self.history.store(Arc::new(VehicleHistory {
            frames: self.frames.iter().cloned().collect(),
        }));
        frame
    }
}
