//! A deterministic synthetic road scene.
//!
//! Why generated rather than recorded footage: the port's correctness oracle is
//! a comparison of per-frame output, and that only works if the *input* is
//! identical on every machine. A committed video file is not -- decoders differ
//! across builds and platforms -- and it has to be distributed and licensed. A
//! seeded generator is a few hundred lines that produce the same pixels
//! everywhere.
//!
//! The second reason matters more. **The scene is built through the same
//! homography the application calibrates with**, so every vehicle's true speed
//! is known exactly, in metres per second, by construction. That turns speed
//! accuracy into something a test can assert instead of something a human
//! squints at.
//!
//! Porting note
//! ------------
//! This is the module a port gets wrong first, and the failure is not obvious:
//! a scene that is plausible but not identical produces a run that is
//! internally consistent, passes every structural test, and disagrees with the
//! reference CSV on every frame for reasons that look like a tracking bug.
//!
//! Two things carry the weight. [`crate::numpy_rng`] reproduces numpy's stream
//! exactly, and every float-to-int conversion below truncates toward zero
//! because that is what a numpy `astype(np.int32)` and a Python `int()` both
//! do. `as i32` in Rust truncates the same way; it saturates where numpy would
//! wrap, which only matters for coordinates far outside the frame, and the
//! frame-hash check is what confirms it never happens here.
//!
//! Verify with `make_clip --frames 20 --hash` against the Python's
//! `tools/make_clip.py --frames 20 --hash` before debugging anything
//! downstream.

use opencv::core::{Mat, Point as CvPoint, Scalar, Vector, CV_8UC1, CV_8UC3};
use opencv::prelude::*;

use crate::config::Settings;
use crate::geometry::{Box, Homography};
use crate::numpy_rng::NumpyRng;

/// Lane centres in metres across the carriageway, and travel direction.
/// Positive travels away from the camera, negative towards it.
const LANES: [(f64, i32); 2] = [(1.9, -1), (5.4, 1)];

/// Vehicles are culled outside this window, in metres along the road.
///
/// The near limit is not cosmetic. The homography's denominator reaches zero at
/// roughly -22 m -- the plane through the camera -- and projection blows up as
/// it approaches: a vehicle at -20 m produced a bounding box twenty-four
/// thousand pixels tall before this limit existed. -6 m is already below the
/// bottom of the frame, so nothing visible is lost by stopping well short.
const ALONG_WINDOW: (f64, f64) = (-6.0, 60.0);

/// Where the generator actually put one vehicle, and how fast.
#[derive(Debug, Clone)]
pub struct VehicleTruth {
    pub vehicle_id: i64,
    pub across_m: f64,
    pub along_m: f64,
    pub speed_kph: f64,
    pub length_m: f64,
    pub box_: Option<Box>,
    pub visible: bool,
}

/// Every vehicle present at one frame. Used by tests, never by the app.
#[derive(Debug, Clone)]
pub struct GroundTruth {
    pub frame_id: i64,
    pub vehicles: Vec<VehicleTruth>,
}

impl GroundTruth {
    pub fn visible(&self) -> Vec<&VehicleTruth> {
        self.vehicles.iter().filter(|v| v.visible).collect()
    }
}

/// One vehicle's schedule, in world coordinates.
struct SceneVehicle {
    vehicle_id: i64,
    across: f64,
    direction: i32,
    speed_ms: f64,
    length: f64,
    width: f64,
    height: f64,
    shade: (i64, i64, i64),
    enter_frame: i64,
    start_along: f64,
}

impl SceneVehicle {
    /// Distance along the road at this frame. Exact -- constant velocity.
    fn along_at(&self, frame_id: i64, fps: i32) -> f64 {
        self.start_along
            + self.direction as f64
                * self.speed_ms
                * ((frame_id - self.enter_frame) as f64 / fps as f64)
    }

    fn speed_kph(&self) -> f64 {
        self.speed_ms * 3.6
    }
}

/// Generates frames of a fixed camera watching a two-lane road.
pub struct RoadScene {
    width: i32,
    height: i32,
    frames: i64,
    fps: i32,
    homography: Homography,
    background: Mat,
    vehicles: Vec<SceneVehicle>,
    occluder: Vector<CvPoint>,
    /// A bank of noise tiles, cycled per frame. Regenerating sensor noise at
    /// full resolution every frame would cost more than the pipeline does.
    noise: Vec<Mat>,
}

impl RoadScene {
    pub fn new(settings: &Settings) -> Result<Self, String> {
        Self::with_seed(settings, None)
    }

    pub fn with_seed(settings: &Settings, seed: Option<i64>) -> Result<Self, String> {
        let vid = &settings.video;
        let cal = &settings.calibration;
        let seed = seed.unwrap_or(vid.SCENE_SEED);

        // The generator works in FULL-resolution image pixels, so it builds the
        // homography with a downscale of 1. The application builds the same
        // survey at working resolution. Both describe the same road.
        let homography = Homography::from_survey(
            &cal.IMAGE_POINTS,
            &cal.WORLD_POINTS,
            1.0,
            (cal.ZONE_START_M, cal.ZONE_END_M),
        )?;

        let mut scene = Self {
            width: vid.FRAME_WIDTH,
            height: vid.FRAME_HEIGHT,
            frames: vid.SCENE_FRAMES,
            fps: vid.FPS,
            homography,
            background: Mat::default(),
            vehicles: Vec::new(),
            occluder: Vector::new(),
            noise: Vec::new(),
        };

        // The order of these three draws from one generator, and the order is
        // part of the contract with the Python. Moving the noise bank ahead of
        // the vehicles would give every vehicle a different speed.
        let mut rng = NumpyRng::new(seed as u32);
        scene.background = scene.build_background(&mut rng)?;
        scene.vehicles = scene.build_vehicles(&mut rng);
        scene.occluder = scene.build_occluder();
        scene.noise = (0..8)
            .map(|_| scene.noise_tile(&mut rng))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(scene)
    }

    // -- construction ----------------------------------------------------

    fn project(&self, across: f64, along: f64) -> (f64, f64) {
        let p = self.homography.to_image(across, along);
        (p.x, p.y)
    }

    fn pixels_per_metre(&self, along: f64) -> f64 {
        let a = self.homography.to_image(3.65, along);
        let b = self.homography.to_image(3.65, along + 1.0);
        f64::max(1e-6, f64::hypot(a.x - b.x, a.y - b.y))
    }

    /// A polygon in the form OpenCV wants, truncating toward zero exactly as
    /// `np.array(..., dtype=np.int32)` does.
    fn polygon(&self, corners: &[(f64, f64)]) -> Vector<CvPoint> {
        corners
            .iter()
            .map(|&(x, y)| CvPoint::new(x as i32, y as i32))
            .collect()
    }

    /// The empty road: surface, markings, verge.
    ///
    /// The surface needs real texture. A flat grey road would let the
    /// background model reach near-zero variance everywhere, and then any
    /// sensor noise at all reads as foreground -- which is not how a real
    /// camera behaves and would make the whole difference stage look easier
    /// than it is.
    fn build_background(&self, rng: &mut NumpyRng) -> Result<Mat, String> {
        let mut canvas = Mat::new_rows_cols_with_default(
            self.height,
            self.width,
            CV_8UC3,
            // Overcast sky above the horizon, in BGR.
            Scalar::new(128.0, 122.0, 112.0, 0.0),
        )
        .map_err(|e| format!("cannot allocate the scene background: {e}"))?;

        // Ground plane, drawn as one very large quad. The extents are chosen
        // from what is actually in view, not from round numbers: at the near
        // edge one metre across the carriageway is over a hundred pixels, so a
        // world rectangle that looks modest on paper projects far outside the
        // frame and its corners end up behind the camera.
        //
        // The far edge is large rather than infinite because the projection
        // approaches the horizon asymptotically -- 400 m is within a few pixels
        // of it and stays comfortably inside the well-conditioned region.
        let (near, far) = (-8.0, 400.0);
        let (wide_left, wide_right) = (-26.0, 33.0);
        let ground = self.polygon(&[
            self.project(wide_left, near),
            self.project(wide_right, near),
            self.project(wide_right, far),
            self.project(wide_left, far),
        ]);
        // Verge green.
        opencv::imgproc::fill_convex_poly_def(&mut canvas, &ground, Scalar::new(52.0, 74.0, 54.0, 0.0))
            .map_err(|e| format!("cannot draw the verge: {e}"))?;

        // The carriageway itself, laid over the verge.
        let surface = self.polygon(&[
            self.project(-0.35, near),
            self.project(7.65, near),
            self.project(7.65, far),
            self.project(-0.35, far),
        ]);
        opencv::imgproc::fill_convex_poly_def(&mut canvas, &surface, Scalar::new(58.0, 60.0, 62.0, 0.0))
            .map_err(|e| format!("cannot draw the carriageway: {e}"))?;

        // Asphalt texture, projected so it foreshortens with the surface
        // instead of sitting flat over it.
        //
        // Repeated across channels rather than generated per channel: asphalt
        // grain is a brightness variation, not a colour one, and per-channel
        // noise would give the road a chroma speckle no camera produces. The
        // Python writes that as `np.repeat(..., 3, axis=2)` over an (h, w, 1)
        // draw, so the generator yields one value per *pixel*, not per channel.
        let grain = rng.integers_u8(0, 26, (self.height * self.width) as usize);
        let mut speckle =
            Mat::new_rows_cols_with_default(self.height, self.width, CV_8UC3, Scalar::all(0.0))
                .map_err(|e| format!("cannot allocate the asphalt speckle: {e}"))?;
        {
            let bytes = speckle
                .data_bytes_mut()
                .map_err(|e| format!("cannot fill the asphalt speckle: {e}"))?;
            for (i, value) in grain.iter().enumerate() {
                bytes[i * 3] = *value;
                bytes[i * 3 + 1] = *value;
                bytes[i * 3 + 2] = *value;
            }
        }

        // `np.where(mask > 0, cv2.add(canvas, speckle), canvas)` is a masked
        // copy: add everywhere, then keep the result only on the ground.
        let mut mask =
            Mat::new_rows_cols_with_default(self.height, self.width, CV_8UC1, Scalar::all(0.0))
                .map_err(|e| format!("cannot allocate the ground mask: {e}"))?;
        opencv::imgproc::fill_convex_poly_def(&mut mask, &ground, Scalar::all(255.0))
            .map_err(|e| format!("cannot draw the ground mask: {e}"))?;

        let mut textured = Mat::default();
        opencv::core::add(
            &canvas,
            &speckle,
            &mut textured,
            &opencv::core::no_array(),
            -1,
        )
        .map_err(|e| format!("cannot apply the asphalt texture: {e}"))?;
        textured
            .copy_to_masked(&mut canvas, &mask)
            .map_err(|e| format!("cannot mask the asphalt texture: {e}"))?;

        // Lane markings: solid edge lines, dashed centre.
        for across in [0.15, 7.15] {
            self.draw_road_line(
                &mut canvas,
                across,
                near,
                260.0,
                Scalar::new(188.0, 190.0, 188.0, 0.0),
                0.14,
            )?;
        }
        // `np.arange(-6.0, 190.0, 9.0)`: 22 values, each computed as
        // start + i*step rather than accumulated, which is what numpy does and
        // what keeps the dashes on the same pixels in both implementations.
        let dash_count = ((190.0f64 - -6.0) / 9.0).ceil() as i64;
        for i in 0..dash_count {
            let start = -6.0 + 9.0 * i as f64;
            self.draw_road_line(
                &mut canvas,
                3.65,
                start,
                start + 4.5,
                Scalar::new(198.0, 200.0, 198.0, 0.0),
                0.12,
            )?;
        }

        // The survey marks themselves, painted on the road as they would be.
        for (x, y) in [(crate::encf!(0.0), crate::encf!(0.0)), (crate::encf!(7.3), crate::encf!(0.0)), (crate::encf!(7.3), crate::encf!(40.0)), (crate::encf!(0.0), crate::encf!(40.0))] {
            let (px, py) = self.project(x, y);
            opencv::imgproc::circle(
                &mut canvas,
                CvPoint::new(px as i32, py as i32),
                4,
                Scalar::new(210.0, 210.0, 215.0, 0.0),
                -1,
                opencv::imgproc::LINE_8,
                0,
            )
            .map_err(|e| format!("cannot draw a survey mark: {e}"))?;
        }

        Ok(canvas)
    }

    fn draw_road_line(
        &self,
        canvas: &mut Mat,
        across: f64,
        y0: f64,
        y1: f64,
        colour: Scalar,
        half_width_m: f64,
    ) -> Result<(), String> {
        let quad = self.polygon(&[
            self.project(across - half_width_m, y0),
            self.project(across + half_width_m, y0),
            self.project(across + half_width_m, y1),
            self.project(across - half_width_m, y1),
        ]);
        opencv::imgproc::fill_convex_poly_def(canvas, &quad, colour)
            .map_err(|e| format!("cannot draw a road marking: {e}"))
    }

    /// Schedule vehicles down both lanes.
    ///
    /// Speeds straddle the limit deliberately, and the spread is wide enough
    /// that the enforcement tolerance is genuinely load-bearing -- a clip where
    /// every vehicle is obviously speeding or obviously not would never
    /// exercise the gate's margin logic.
    fn build_vehicles(&self, rng: &mut NumpyRng) -> Vec<SceneVehicle> {
        let mut vehicles = Vec::new();
        let mut vehicle_id = 1i64;
        for (across, direction) in LANES {
            let mut frame = rng.integer_i64(0, 90);
            while frame < self.frames {
                let speed_kph = rng.uniform(38.0, 78.0);
                let mut length = rng.uniform(3.9, 5.4);
                if rng.random() < 0.18 {
                    // The occasional lorry.
                    length = rng.uniform(7.5, 10.5);
                }
                let width = rng.uniform(1.7, 2.1);
                let height = rng.uniform(1.35, 1.7) * if length > 7.0 { 1.9 } else { 1.0 };
                let shade = rng.integers_i64(35, 205, 3);
                // Enter beyond the far mark or before the near one, depending
                // on direction, so the vehicle is already at speed when it
                // reaches the measurement zone.
                let start_along = if direction < 0 { 58.0 } else { -14.0 };
                vehicles.push(SceneVehicle {
                    vehicle_id,
                    across,
                    direction,
                    speed_ms: speed_kph / 3.6,
                    length,
                    width,
                    height,
                    shade: (shade[0], shade[1], shade[2]),
                    enter_frame: frame,
                    start_along,
                });
                vehicle_id += 1;
                // Headway between vehicles in the same lane. Short, because a
                // camera sited where enforcement is worth doing is looking at a
                // road with traffic on it -- and because every
                // interpreter-bound stage in this pipeline scales with the
                // number of vehicles while every native one scales with frame
                // area. An empty road measures the frame size and nothing else.
                frame += rng.integer_i64(26, 70);
            }
        }
        vehicles
    }

    /// A roadside pole the near lane passes behind.
    ///
    /// Its job is to interrupt a track mid-measurement, so the coasting path
    /// and the fit-residual guard are exercised by every clip rather than only
    /// by the unlucky ones.
    fn build_occluder(&self) -> Vector<CvPoint> {
        let top = self.project(-0.7, 21.0);
        let base = self.project(-0.7, 19.0);
        let width = f64::max(6.0, self.pixels_per_metre(20.0) * 0.35);
        self.polygon(&[
            (base.0 - width, base.1),
            (base.0 + width, base.1),
            (top.0 + width, top.1 - 190.0),
            (top.0 - width, top.1 - 190.0),
        ])
    }

    fn noise_tile(&self, rng: &mut NumpyRng) -> Result<Mat, String> {
        let values = rng.integers_u8(0, 9, (self.height * self.width * 3) as usize);
        let mut tile =
            Mat::new_rows_cols_with_default(self.height, self.width, CV_8UC3, Scalar::all(0.0))
                .map_err(|e| format!("cannot allocate a noise tile: {e}"))?;
        tile.data_bytes_mut()
            .map_err(|e| format!("cannot fill a noise tile: {e}"))?
            .copy_from_slice(&values);
        Ok(tile)
    }

    // -- per frame -------------------------------------------------------

    pub fn frame_count(&self) -> i64 {
        self.frames
    }

    /// Full-resolution homography. Tests use it; the app builds its own.
    pub fn homography(&self) -> &Homography {
        &self.homography
    }

    /// Image bounding box of a vehicle at a world position, or `None` if off.
    fn box_for(&self, vehicle: &SceneVehicle, along: f64) -> Option<Box> {
        if !(ALONG_WINDOW.0 < along && along < ALONG_WINDOW.1) {
            return None;
        }
        let half_w = vehicle.width / 2.0;
        let half_l = vehicle.length / 2.0;
        let corners = [
            self.project(vehicle.across - half_w, along - half_l),
            self.project(vehicle.across + half_w, along - half_l),
            self.project(vehicle.across + half_w, along + half_l),
            self.project(vehicle.across - half_w, along + half_l),
        ];
        let lift = vehicle.height * self.pixels_per_metre(along);
        let x0 = corners.iter().map(|c| c.0).fold(f64::INFINITY, f64::min);
        let x1 = corners.iter().map(|c| c.0).fold(f64::NEG_INFINITY, f64::max);
        let y0 = corners.iter().map(|c| c.1).fold(f64::INFINITY, f64::min) - lift;
        let y1 = corners.iter().map(|c| c.1).fold(f64::NEG_INFINITY, f64::max);
        if x1 < -60.0 || x0 > self.width as f64 + 60.0 || y1 < 0.0 || y0 > self.height as f64 {
            return None;
        }
        Some(Box::new(x0, y0, x1 - x0, y1 - y0))
    }

    /// Draw a vehicle as an extruded ground footprint.
    ///
    /// The footprint is projected properly through the homography, so it
    /// foreshortens correctly; the body is then extruded vertically in image
    /// space. That approximation is exact for a level road and a camera without
    /// roll, which is what this scene is.
    fn draw_vehicle(
        &self,
        canvas: &mut Mat,
        vehicle: &SceneVehicle,
        along: f64,
    ) -> Result<(), String> {
        let half_w = vehicle.width / 2.0;
        let half_l = vehicle.length / 2.0;
        let footprint = [
            self.project(vehicle.across - half_w, along - half_l),
            self.project(vehicle.across + half_w, along - half_l),
            self.project(vehicle.across + half_w, along + half_l),
            self.project(vehicle.across - half_w, along + half_l),
        ];
        let lift = vehicle.height * self.pixels_per_metre(along);
        let roof: Vec<(f64, f64)> = footprint.iter().map(|&(x, y)| (x, y - lift)).collect();

        let (b, g, r) = vehicle.shade;
        let body = Scalar::new(b as f64, g as f64, r as f64, 0.0);
        // `int(c * 0.55)` truncates, and so does `as i64`.
        let dark = Scalar::new(
            (b as f64 * 0.55) as i64 as f64,
            (g as f64 * 0.55) as i64 as f64,
            (r as f64 * 0.55) as i64 as f64,
            0.0,
        );
        let bright = Scalar::new(
            i64::min(255, (b as f64 * 1.25) as i64) as f64,
            i64::min(255, (g as f64 * 1.25) as i64) as f64,
            i64::min(255, (r as f64 * 1.25) as i64) as f64,
            0.0,
        );

        // Shadow first, offset along the road surface so it stays on the plane.
        let shadow = self.polygon(
            &footprint
                .iter()
                .map(|&(x, y)| (x + lift * 0.16, y + 2.0))
                .collect::<Vec<_>>(),
        );
        opencv::imgproc::fill_convex_poly_def(canvas, &shadow, Scalar::new(38.0, 40.0, 42.0, 0.0))
            .map_err(|e| format!("cannot draw a vehicle shadow: {e}"))?;

        // Sides, then roof, so the roof wins where they overlap.
        for (a, b) in [(0usize, 1usize), (1, 2), (2, 3), (3, 0)] {
            let side = self.polygon(&[footprint[a], footprint[b], roof[b], roof[a]]);
            let colour = if a % 2 == 1 { dark } else { body };
            opencv::imgproc::fill_convex_poly_def(canvas, &side, colour)
                .map_err(|e| format!("cannot draw a vehicle side: {e}"))?;
        }
        let roof_poly = self.polygon(&roof);
        opencv::imgproc::fill_convex_poly_def(canvas, &roof_poly, bright)
            .map_err(|e| format!("cannot draw a vehicle roof: {e}"))?;

        // A windscreen band, so the model sees internal structure rather than a
        // flat slab -- flat slabs merge with each other far too readily.
        let band = self.polygon(&[
            (roof[0].0, roof[0].1 + lift * 0.25),
            (roof[1].0, roof[1].1 + lift * 0.25),
            (roof[1].0, roof[1].1 + lift * 0.55),
            (roof[0].0, roof[0].1 + lift * 0.55),
        ]);
        opencv::imgproc::fill_convex_poly_def(canvas, &band, Scalar::new(28.0, 32.0, 36.0, 0.0))
            .map_err(|e| format!("cannot draw a windscreen: {e}"))?;
        Ok(())
    }

    /// Render one frame. Same input -> same bytes, on every machine.
    pub fn render(&self, frame_id: i64) -> Result<(Mat, GroundTruth), String> {
        if !(0..self.frames).contains(&frame_id) {
            return Err(format!(
                "frame {frame_id} is outside this scene (0..{}). The length is \
                 video.SCENE_FRAMES; raise it, or ask for fewer.",
                self.frames - 1
            ));
        }

        let mut canvas = self
            .background
            .clone();

        // Slow illumination drift, so the background model has something to
        // adapt to. A perfectly static scene lets it converge to zero variance
        // and stops testing the adaptation path entirely.
        let drift = (7.0 * (frame_id as f64 / 260.0).sin()) as i32;
        if drift != 0 {
            let mut drifted = Mat::default();
            opencv::core::add(
                &canvas,
                &Scalar::new(drift as f64, drift as f64, drift as f64, 0.0),
                &mut drifted,
                &opencv::core::no_array(),
                -1,
            )
            .map_err(|e| format!("cannot apply the illumination drift: {e}"))?;
            canvas = drifted;
        }

        // Far vehicles first, so nearer ones occlude them.
        let mut present: Vec<(f64, &SceneVehicle)> = Vec::new();
        for vehicle in &self.vehicles {
            if frame_id < vehicle.enter_frame {
                continue;
            }
            let along = vehicle.along_at(frame_id, self.fps);
            if !(ALONG_WINDOW.0 < along && along < ALONG_WINDOW.1) {
                continue;
            }
            present.push((along, vehicle));
        }
        // Descending distance. A stable sort, because the Python's is stable
        // and two vehicles at exactly the same distance must be drawn in the
        // same order for the pixels to match.
        present.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut truths = Vec::with_capacity(present.len());
        for (along, vehicle) in &present {
            self.draw_vehicle(&mut canvas, vehicle, *along)?;
            let box_ = self.box_for(vehicle, *along);
            truths.push(VehicleTruth {
                vehicle_id: vehicle.vehicle_id,
                across_m: vehicle.across,
                along_m: *along,
                speed_kph: vehicle.speed_kph(),
                length_m: vehicle.length,
                visible: box_.is_some(),
                box_,
            });
        }

        // The pole is drawn last: it is in front of the traffic.
        opencv::imgproc::fill_convex_poly_def(
            &mut canvas,
            &self.occluder,
            Scalar::new(96.0, 98.0, 100.0, 0.0),
        )
        .map_err(|e| format!("cannot draw the occluder: {e}"))?;

        let mut noisy = Mat::default();
        opencv::core::add(
            &canvas,
            &self.noise[(frame_id % self.noise.len() as i64) as usize],
            &mut noisy,
            &opencv::core::no_array(),
            -1,
        )
        .map_err(|e| format!("cannot apply sensor noise: {e}"))?;

        Ok((noisy, GroundTruth {
            frame_id,
            vehicles: truths,
        }))
    }
}
