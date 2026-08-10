//! The pipeline: its thread, its mailbox, and its stages.

pub mod background;
pub mod blobs;
// `pipeline::pipeline` mirrors the Python's `kerbside/pipeline/pipeline.py`.
// The layouts are kept one-for-one on purpose so the two can be read side by
// side, and that is worth more than avoiding a repeated path segment.
#[allow(clippy::module_inception)]
pub mod pipeline;
pub mod types;

pub use background::BackgroundModel;
pub use blobs::BlobFinder;
pub use pipeline::{
    fixed_settings, live_settings, Mailbox, Pipeline, RunningPipeline, SettingsProvider,
};
pub use types::{contact_of, Blob, Detection, FrameResult, RawFrame, SharedMat};
