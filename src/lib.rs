//! Kerbside -- a roadside speed-enforcement camera.
//!
//! A Rust port of the Python proxy application. The module layout deliberately
//! mirrors the Python package one-for-one, so a reader can hold the two side by
//! side; where this port had to diverge, the divergence is commented at the
//! point it happens rather than collected in a document that will drift.
//!
//! The three divergences worth knowing before reading anything else:
//!
//! * **The settings rebind is explicit.** Python swaps a module global and the
//!   interpreter lock makes it atomic for free. Here it is an `ArcSwap`, and
//!   every per-frame consumer holds its `Arc` snapshot for the whole frame.
//!   See [`config`].
//! * **The inference overlap is built, not inherited.** In Python it falls out
//!   of ONNX Runtime releasing the GIL. There is no GIL to release here, so the
//!   detector owns a real worker thread and the handoff is a channel. See
//!   [`detect::detector`].
//! * **The evidence record is owned nested data, not a graph the runtime
//!   traces.** The allocation is still there, frame after frame, retained just
//!   as deeply -- what is gone is the collector that had to walk it. That
//!   difference is the headline measurement this port exists to produce, so the
//!   allocation must not be optimised away. See [`consumers`].

pub mod config;
pub mod consumers;
pub mod detect;
pub mod enforce;
pub mod geometry;
pub mod measure;
pub mod numpy_rng;
pub mod output;
pub mod perf;
pub mod pipeline;
pub mod source;
pub mod track;
pub mod tuning;
