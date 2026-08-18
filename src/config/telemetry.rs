//! Instrumentation, evidence retention, and runtime pinning.

crate::settings_group! {
    pub struct TelemetrySettings {
        // Tier 1: always-on budget summary, a few hundred nanoseconds per frame.
        MEASURE_FPS: bool = true,
        // Tier 2: full per-frame stage timings, written as CSV by a separate
        // thread. The hot path only appends to a queue.
        MEASURE_STAGES: bool = false,
        PERF_DIR: String = obfstr::obfstr!("telemetry").to_string(),
        PERF_FLUSH_MS: i64 = 250,

        // The nested per-frame evidence record. A speed camera has to be able to
        // justify every reading it produces -- which vehicle, which
        // observations, which fit -- so this is a product requirement, not debug
        // output.
        EVIDENCE_RECORD: bool = true,
        // Frames retained by reference in the evidence ring.
        RING_FRAMES: i64 = 500,

        // Per-frame budget in ms; derived from video.FPS at resolve time.
        BUDGET_MS: f64 = 20.0,

        WRITE_OVERLAY: bool = false,

        // OpenCV's thread pool, pinned explicitly.
        //
        // Not a tuning knob -- a correctness requirement for any measurement.
        // Left alone, OpenCV sizes its pool from the host core count, so the
        // same code on the same input reports different stage costs on different
        // machines. Worse, it affects each stage differently: the resize
        // parallelises almost linearly, the background model much less, so even
        // the *ratios* between stages move. A comparison run against an unpinned
        // pool measures the two machines rather than the two implementations.
        //
        // Four matches the target board's core count, and leaves the picture
        // honest about contention with the detector thread.
        OPENCV_THREADS: i32 = 4,
    }
}
