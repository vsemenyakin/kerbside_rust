//! Vehicle classification: the model, its worker thread, and blob scoring.

pub mod detector;
pub mod score;

pub use detector::{preprocess, probe_runtime, resolved_runtime_path, Detector, Inference};
pub use score::{accept, score_blobs};
