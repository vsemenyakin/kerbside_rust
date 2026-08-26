//! The deterministic road scene, the ground truth it was built from, and the
//! frame-source abstraction that lets a build analyse a recorded clip instead of
//! generating the scene itself.

mod clip;
// The generator ships only in introspection builds; dist analyses a clip.
#[cfg(feature = "introspection")]
mod road;

pub use clip::ClipSource;
#[cfg(feature = "introspection")]
pub use clip::write_clip;
#[cfg(feature = "introspection")]
pub use road::{GroundTruth, RoadScene, VehicleTruth};

/// A source of frames for the pipeline: either the in-code scene generator
/// ([`RoadScene`]) or a recorded [`ClipSource`] read back from disk.
///
/// The pipeline consumes only the pixels of each frame (the ground truth the
/// generator also produces is used by tests, never by the run path), so the two
/// sources are interchangeable and a clip that reproduces the generator's frames
/// byte-for-byte reproduces its output.
pub trait FrameSource {
    fn frame_count(&self) -> i64;
    fn frame(&self, id: i64) -> Result<opencv::core::Mat, String>;
}
