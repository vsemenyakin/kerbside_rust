//! Types produced by the pipeline.
//!
//! These live with their producer rather than in a shared module -- only
//! `Point`, `Box` and `Homography` earned a place in `geometry`, because
//! several modules use them and none owns them.
//!
//! Everything here is immutable once built. That is not decoration: these
//! objects are handed between threads without a lock, and immutability is the
//! entire safety argument. A reader that receives one holds a value that cannot
//! change underneath it, so no reader needs to copy and no writer needs to wait.

use std::sync::Arc;

use opencv::core::Mat;

use crate::geometry::{Box, Point};

/// A frame buffer shared, read-only, across the source, pipeline and detector
/// threads.
///
/// ## Why this wrapper exists
///
/// In Python a frame is a numpy array passed by reference, and the interpreter
/// lock means "several threads hold this array" needs no further thought. Here
/// it does. The `opencv` crate marks `Mat` as `Send` -- it may be moved between
/// threads -- but not `Sync`, because a `Mat` is perfectly capable of being
/// mutated through a shared handle, and the crate cannot know that this
/// application never does.
///
/// This application never does, and that is the invariant the wrapper encodes:
/// a `SharedMat` is built once, wrapped in an `Arc`, and from that moment only
/// ever read. The only accessor hands out `&Mat`, and OpenCV's read paths --
/// `resize`, `cvtColor`, the background model's input, the overlay's `copy` --
/// take their input by const reference.
///
/// ## Safety
///
/// `Sync` is asserted, not derived, and it holds because:
///
/// * [`SharedMat::get`] is the only way in, and it yields `&Mat`. There is no
///   `&mut` accessor and no interior mutability.
/// * The `Mat` is fully constructed before the `Arc` is created, so no thread
///   can observe a partially built buffer.
/// * OpenCV's own reference count is atomic, so the eventual single `Drop` --
///   when the last `Arc` goes -- is safe regardless of which thread runs it.
///
/// Adding a method that exposes `&mut Mat`, or that calls an OpenCV function
/// taking this buffer as an *output* array, breaks the argument above and makes
/// the `unsafe impl` unsound. Copy the `Mat` instead.
///
/// This is one of the places the porting note in the Python's `pipeline.py`
/// predicted: "immutable objects shared across three threads with no lock, safe
/// *because* they are never mutated. In a language with real ownership this is
/// either much easier or much more annoying." It is a fifteen-line wrapper and
/// a paragraph of justification -- which is the honest answer to record.
pub struct SharedMat(Mat);

unsafe impl Send for SharedMat {}
unsafe impl Sync for SharedMat {}

impl SharedMat {
    pub fn new(mat: Mat) -> Arc<Self> {
        Arc::new(Self(mat))
    }

    #[inline]
    pub fn get(&self) -> &Mat {
        &self.0
    }
}

/// One frame off the source, before any processing.
///
/// `full` is the full-resolution colour image. It is passed by reference the
/// whole way down and into the evidence ring -- copying two and a half
/// megabytes per consumer would cost more than every stage that reads it.
#[derive(Clone)]
pub struct RawFrame {
    pub frame_id: i64,
    pub full: Arc<SharedMat>,
    pub t_capture: f64,
    /// Source frames skipped since the last delivered one. Non-zero only in
    /// realtime mode, where the mailbox drops to latest.
    pub skipped: i64,
}

impl RawFrame {
    pub fn new(frame_id: i64, full: Arc<SharedMat>, t_capture: f64) -> Self {
        Self {
            frame_id,
            full,
            t_capture,
            skipped: 0,
        }
    }
}

/// A foreground region, in working-frame coordinates.
#[derive(Clone)]
pub struct Blob {
    pub box_: Box,
    pub area: f64,
    pub fill: f64,
    /// The contour as extracted, one entry per point.
    ///
    /// Held as owned points rather than as OpenCV's own vector because the
    /// evidence record walks it per frame and the tracker never does; carrying
    /// the OpenCV container would mean a foreign-call per point at exactly the
    /// place the Python is already slowest.
    pub contour: Vec<(i32, i32)>,
}

impl Blob {
    pub fn aspect(&self) -> f64 {
        if self.box_.h != 0.0 {
            self.box_.w / self.box_.h
        } else {
            0.0
        }
    }
}

/// One box from the model, in working-frame coordinates.
#[derive(Clone, Copy)]
pub struct Detection {
    pub box_: Box,
    pub score: f64,
    /// Which class the model picked. 0 = car, 1 = long vehicle.
    pub label: i32,
}

/// Everything the pipeline produced for one frame.
#[derive(Clone)]
pub struct FrameResult {
    pub frame_id: i64,
    pub t_capture: f64,
    pub blobs: Vec<Blob>,
    pub detections: Vec<Detection>,
    /// True when the model ran this frame; false when its result was carried
    /// forward from an earlier frame. Consumers must not treat a
    /// carried-forward detection as fresh evidence.
    pub inference_ran: bool,
    pub foreground_ratio: f64,
    /// (height, width), matching the Python's row-major convention.
    pub working_shape: (i32, i32),
}

/// The single place a vehicle's ground-contact point is derived.
///
/// Deliberately one function rather than an inline expression at each call
/// site: the ratio is the difference between measuring the vehicle and
/// measuring its shadow, and two call sites disagreeing about it would produce
/// two different speeds for the same vehicle with nothing to say which was
/// right.
#[inline]
pub fn contact_of(box_: &Box, ratio: f64) -> Point {
    box_.contact_point(ratio)
}
