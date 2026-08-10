//! Vehicle state, and the per-frame snapshot handed to every other thread.
//!
//! Two kinds of object, and the distinction is the whole design:
//!
//! [`Vehicle`] is **mutable** and is owned by the associator. Nothing outside
//! `track::associator` may touch one -- and here that is enforced rather than
//! asked for, because the associator hands out `&VehicleState` and never
//! `&mut Vehicle`.
//!
//! [`VehicleState`], [`FrameVehicles`] and [`VehicleHistory`] are **immutable**
//! and are what everybody else sees. The associator publishes a fresh
//! [`VehicleHistory`] at the end of each frame; readers on other threads take
//! that `Arc` and hold it as long as they like. No locks, no copying at the
//! read side, and no possibility of observing a half-updated vehicle.
//!
//! The cost of that guarantee is real and is measured: rebuilding the snapshot
//! allocates a fresh object per vehicle every frame, forever. See
//! `Associator::snapshot` before deciding it is wasteful.

use crate::geometry::Box;

/// One sighting of a vehicle: when, where in the image, where on the road.
#[derive(Debug, Clone, Copy)]
pub struct Observation {
    pub frame_id: i64,
    pub t: f64,
    pub box_: Box,
    pub across_m: f64,
    pub along_m: f64,
    /// Model coverage for the blob this came from. Carried all the way to the
    /// evidence record, because a measurement's confidence has to travel with
    /// it -- a number in a violation packet with no provenance is unusable.
    pub coverage: f64,
}

/// Apparent length in metres at or above which a vehicle is *reported* as a
/// goods vehicle. Reported only -- see `enforce::gate::limit_for`.
pub const LONG_VEHICLE_M: f64 = 7.0;

/// One vehicle, as of one frame. Immutable, shareable, cheap to hold.
#[derive(Debug, Clone, Copy)]
pub struct VehicleState {
    pub vehicle_id: i64,
    pub box_: Box,
    pub across_m: f64,
    pub along_m: f64,
    pub speed_kph: f64,
    /// RMS residual of the straight-line fit, in metres. The honest measure of
    /// whether this speed means anything.
    pub fit_residual_m: f64,
    pub samples: usize,
    pub baseline_m: f64,
    pub confirmed: bool,
    pub hits: i64,
    pub misses: i64,
    pub age: i64,
    pub coverage: f64,
    pub in_zone: bool,
    /// Apparent length in metres, from the box through the homography. This is
    /// what decides the vehicle's class, not the model.
    pub length_m: f64,
}

impl VehicleState {
    pub fn is_long(&self) -> bool {
        self.length_m >= LONG_VEHICLE_M
    }
}

/// Per-frame counts, published alongside the states.
///
/// The Python builds this as a dict, which is one of the containers the churn
/// test counts. Kept as a struct here: the allocation that matters is the
/// per-frame `Vec` of ids, and a `HashMap` of four fixed keys would be slower
/// in both languages while measuring nothing extra.
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    pub n_vehicles: usize,
    pub n_confirmed: usize,
    pub n_in_zone: usize,
    pub ids: Vec<i64>,
}

/// Every vehicle at one frame.
#[derive(Debug, Clone, Default)]
pub struct FrameVehicles {
    pub frame_id: i64,
    pub vehicles: Vec<VehicleState>,
    pub diagnostics: Diagnostics,
}

impl FrameVehicles {
    pub fn by_id(&self, vehicle_id: i64) -> Option<&VehicleState> {
        self.vehicles.iter().find(|s| s.vehicle_id == vehicle_id)
    }
}

/// The last N frames of vehicle state, newest last.
#[derive(Debug, Clone, Default)]
pub struct VehicleHistory {
    pub frames: Vec<FrameVehicles>,
}

impl VehicleHistory {
    pub fn latest(&self) -> Option<&FrameVehicles> {
        self.frames.last()
    }
}

/// Mutable internal track. Owned exclusively by the associator.
pub struct Vehicle {
    pub vehicle_id: i64,
    pub box_: Box,
    pub hits: i64,
    pub misses: i64,
    pub age: i64,
    pub confirmed: bool,
    pub coverage: f64,
    pub observations: Vec<Observation>,
    pub speed_kph: f64,
    pub fit_residual_m: f64,
    pub baseline_m: f64,
    pub length_m: f64,
    pub in_zone: bool,
    /// True once a violation has been recorded, so one vehicle cannot be fined
    /// sixty times a second on its way through the zone.
    pub enforced: bool,
}

impl Vehicle {
    pub fn new(vehicle_id: i64, box_: Box, coverage: f64) -> Self {
        Self {
            vehicle_id,
            box_,
            hits: 1,
            misses: 0,
            age: 1,
            confirmed: false,
            coverage,
            observations: Vec::new(),
            speed_kph: 0.0,
            fit_residual_m: 0.0,
            baseline_m: 0.0,
            length_m: 0.0,
            in_zone: false,
            enforced: false,
        }
    }

    pub fn to_state(&self) -> VehicleState {
        let last = self.observations.last();
        VehicleState {
            vehicle_id: self.vehicle_id,
            box_: self.box_,
            across_m: last.map(|o| o.across_m).unwrap_or(0.0),
            along_m: last.map(|o| o.along_m).unwrap_or(0.0),
            speed_kph: self.speed_kph,
            fit_residual_m: self.fit_residual_m,
            samples: self.observations.len(),
            baseline_m: self.baseline_m,
            confirmed: self.confirmed,
            hits: self.hits,
            misses: self.misses,
            age: self.age,
            coverage: self.coverage,
            in_zone: self.in_zone,
            length_m: self.length_m,
        }
    }
}
