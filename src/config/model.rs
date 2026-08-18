//! The vehicle detector: its graph, its cadence, and how its output is decoded.
//!
//! The detector does not run every frame. The background model already says
//! *where* something is; the detector says *what*, and confirms a blob is a
//! vehicle rather than a shadow, a bird or a bag. That is a much weaker demand
//! on latency, so it runs on a duty cycle and its result is carried forward
//! between runs.

crate::settings_group! {
    pub struct ModelSettings {
        ENABLED: bool = true,
        FILE: String = obfstr::obfstr!("vehicle.onnx").to_string(),

        // Input is [1, 3, INPUT_HEIGHT, INPUT_WIDTH], a direct resize of the
        // working frame. Deliberately not square: the frame is 16:9, and
        // letterboxing it into a square would spend nearly half the tensor --
        // and half the inference -- on grey padding.
        INPUT_WIDTH: i32 = 384,
        INPUT_HEIGHT: i32 = 216,
        CHANNELS: i32 = 3,
        // Output stride. 384x216 at stride 4 gives a 96x54 score and box map.
        //
        // Stride 4 rather than 8 is forced by the far end of the measurement
        // zone. Perspective shrinks a vehicle at 34 m to roughly 20x7 pixels at
        // this input size -- under one cell tall at stride 8, which no amount of
        // capacity recovers. Halving the stride quadruples the deepest layers'
        // work, and that cost buys the far half of the zone.
        STRIDE: i32 = 4,

        // Run every Nth frame. At 50 fps this is ~13 detections per second,
        // which is several per vehicle across the zone -- enough to classify it
        // and to correct a drifting association, and not enough to matter to the
        // budget.
        EVERY_N_FRAMES: i64 = 4,

        // Cell likelihood above which a cell is counted as vehicle.
        //
        // This is a *linear response*, not a probability: the head is a single
        // fitted linear layer with no squashing function, so it sits near 0 for
        // road and near 1 for vehicle but is bounded by neither.
        VEHICLE_THRESHOLD: f64 = 0.19,
        // Fraction of a blob's cells that must clear VEHICLE_THRESHOLD before
        // the blob is accepted as a vehicle.
        //
        // A coverage fraction rather than a mean: a mean is dragged down by the
        // road visible around a vehicle inside its own bounding box, and dragged
        // up by one strongly-firing cell on a blob that is mostly shadow. The
        // fraction asks the question that actually matters -- how much of this
        // blob looks like vehicle -- and answers it the same way at both ends of
        // the zone.
        MIN_COVERAGE: f64 = 0.34,

        // Single-threaded and sequential. Letting the runtime take every core
        // makes this one call faster and the frame slower: it preempts the
        // pipeline thread running the background model, which is the work this
        // call is supposed to be hiding behind.
        ORT_INTRA_THREADS: i32 = 1,
        ORT_INTER_THREADS: i32 = 1,
    }
}
