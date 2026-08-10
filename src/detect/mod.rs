//! Vehicle classification: the model, its worker thread, and blob scoring.

pub mod detector;
pub mod score;

pub use detector::{preprocess, Detector, Inference};
pub use score::{accept, score_blobs};
