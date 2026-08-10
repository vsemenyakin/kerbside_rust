//! Vehicle association, lifecycle and the published state snapshot.

pub mod associator;
pub mod types;

pub use associator::{Associator, Match};
pub use types::{
    Diagnostics, FrameVehicles, Observation, Vehicle, VehicleHistory, VehicleState, LONG_VEHICLE_M,
};
