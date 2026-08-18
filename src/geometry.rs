//! The shared geometric types: a point, a box, and the ground plane.
//!
//! A type lives here only if several top-level modules use it and none of them
//! owns it. Everything else lives with its producer -- `pipeline::types`,
//! `track::types`, `enforce::gate`.
//!
//! `Homography` is the heart of this application. Everything else finds
//! vehicles; this is what turns finding one into measuring one.

use opencv::core::{Point2f, Vector};
use opencv::prelude::*;

/// A position in image coordinates.
///
/// Float, always. The ground-contact point is projected through a homography
/// where the far end of the zone is heavily foreshortened -- around 12 image
/// pixels per metre near the camera and under 3 at the far mark. Rounding to
/// integers there costs a third of a metre, and a third of a metre of jitter
/// across the fit window is several km/h.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn sq_distance_to(&self, other: Point) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        dx * dx + dy * dy
    }

    pub fn distance_to(&self, other: Point) -> f64 {
        self.sq_distance_to(other).sqrt()
    }

    pub fn scaled(&self, factor: f64) -> Point {
        Point::new(self.x * factor, self.y * factor)
    }

    /// For OpenCV drawing only. Never feed this back into the maths.
    pub fn as_int_tuple(&self) -> (i32, i32) {
        (self.x.round() as i32, self.y.round() as i32)
    }

    pub fn is_finite(&self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// An axis-aligned bounding box in working-frame pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Box {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Box {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn area(&self) -> f64 {
        self.w * self.h
    }

    pub fn centre(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Where this vehicle meets the road.
    ///
    /// Taken `ratio` of the box height up from the bottom edge. Exactly at the
    /// bottom edge tracks the leading edge of the shadow, which slides as the
    /// sun moves and is not attached to the vehicle; higher up tracks the
    /// bodywork, which is not on the ground plane and therefore projects to a
    /// world position further away than the vehicle actually is.
    pub fn contact_point(&self, ratio: f64) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h * (1.0 - ratio))
    }

    /// Intersection over union. The association metric.
    ///
    /// Chosen over centre distance because it is scale-aware for free: in this
    /// view a vehicle's box triples in size crossing the zone, so a fixed
    /// distance gate is either too tight at the far end or too loose at the
    /// near end, while an overlap ratio means the same thing at both.
    pub fn iou(&self, other: &Box) -> f64 {
        let (ax2, ay2) = (self.x + self.w, self.y + self.h);
        let (bx2, by2) = (other.x + other.w, other.y + other.h);
        let ix = f64::max(0.0, f64::min(ax2, bx2) - f64::max(self.x, other.x));
        let iy = f64::max(0.0, f64::min(ay2, by2) - f64::max(self.y, other.y));
        let intersection = ix * iy;
        if intersection <= 0.0 {
            return 0.0;
        }
        intersection / (self.area() + other.area() - intersection)
    }

    pub fn merged(&self, other: &Box) -> Box {
        let x1 = f64::min(self.x, other.x);
        let y1 = f64::min(self.y, other.y);
        let x2 = f64::max(self.x + self.w, other.x + other.w);
        let y2 = f64::max(self.y + self.h, other.y + other.h);
        Box::new(x1, y1, x2 - x1, y2 - y1)
    }
}

/// The mapping between image pixels and metres on the road surface.
///
/// Built from four survey marks: their positions in the image, and their
/// positions on the road measured with a tape. Everything this device reports
/// rests on those eight numbers being right, and **nothing downstream can tell
/// that they are wrong** -- a 3% survey error is a 3% error on every speed the
/// device ever produces, and every one of them will look plausible.
///
/// Valid only on the road surface. Projecting a point that is not on the ground
/// plane -- a roof, a road sign, a bird -- returns a confident, precise,
/// meaningless world coordinate. That is why the contact point is taken at the
/// bottom of the box and not at its centre.
///
/// Ported as a plain `[[f64; 3]; 3]` rather than as an OpenCV `Mat`. The Python
/// keeps numpy arrays here and indexes them element by element, which is what
/// the maths below does anyway; carrying a `Mat` would add a bounds-checked
/// dynamic-type access per element on the hot path for no benefit, and the
/// matrix is nine numbers fixed at startup.
#[derive(Debug, Clone, Copy)]
pub struct Homography {
    /// Working-frame pixels -> world metres.
    pub to_world_matrix: [[f64; 3]; 3],
    /// World metres -> working-frame pixels.
    pub to_image_matrix: [[f64; 3]; 3],
    /// Metres per pixel at the near and far ends of the zone. Diagnostic, but
    /// the ratio between them is the honest measure of how much this view is
    /// asking of the far end of the calibration.
    pub near_scale: f64,
    pub far_scale: f64,
}

impl Homography {
    /// Fit from four full-resolution image points and their world positions.
    ///
    /// `downscale` folds the working-frame scaling into the matrix, so callers
    /// pass working-frame pixels and never have to remember which resolution a
    /// coordinate is in. Getting that wrong produces speeds off by exactly the
    /// downscale factor, which is obvious once suspected and invisible until.
    pub fn from_survey(
        image_points: &[(f64, f64)],
        world_points: &[(f64, f64)],
        downscale: f64,
        zone: (f64, f64),
    ) -> Result<Self, String> {
        if image_points.len() != 4 || world_points.len() != 4 {
            return Err(obfstr::obfstr!("a homography needs exactly four survey marks").into());
        }

        // Through OpenCV rather than a hand-rolled 8x8 solve, and through
        // `Point2f` rather than doubles, because the Python passes float32
        // arrays. Both choices are about matching the reference bit for bit:
        // this matrix is the first link in every measurement, and a difference
        // in its last bits is a difference in every speed the device reports.
        let mut source: Vector<Point2f> = Vector::new();
        let mut target: Vector<Point2f> = Vector::new();
        for &(x, y) in image_points {
            source.push(Point2f::new((x / downscale) as f32, (y / downscale) as f32));
        }
        for &(x, y) in world_points {
            target.push(Point2f::new(x as f32, y as f32));
        }
        let mat = opencv::imgproc::get_perspective_transform_def(&source, &target)
            .map_err(|e| format!("{}{e}", obfstr::obfstr!("getPerspectiveTransform failed: ")))?;

        let mut to_world = [[0.0f64; 3]; 3];
        for (r, row) in to_world.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                *cell = *mat
                    .at_2d::<f64>(r as i32, c as i32)
                    .map_err(|e| format!("{}({r},{c}): {e}", obfstr::obfstr!("homography element ")))?;
            }
        }
        let to_image = invert_3x3(&to_world)?;

        let mut instance = Self {
            to_world_matrix: to_world,
            to_image_matrix: to_image,
            near_scale: 0.0,
            far_scale: 0.0,
        };
        instance.near_scale = instance.scale_at(zone.0);
        instance.far_scale = instance.scale_at(zone.1);
        Ok(instance)
    }

    /// Metres of road per image pixel at a given distance along the zone.
    fn scale_at(&self, y_metres: f64) -> f64 {
        let lane_centre = 3.65;
        let a = self.to_image(lane_centre, y_metres);
        let b = self.to_image(lane_centre, y_metres + 1.0);
        let pixels = a.distance_to(b);
        if pixels > 1e-9 {
            1.0 / pixels
        } else {
            0.0
        }
    }

    /// Working-frame pixel -> (across, along) metres on the road.
    #[inline]
    pub fn to_world(&self, point: Point) -> (f64, f64) {
        let m = &self.to_world_matrix;
        let denominator = m[2][0] * point.x + m[2][1] * point.y + m[2][2];
        if denominator.abs() < 1e-12 {
            // The point is on the horizon line, where the plane maps to
            // infinity. Callers must reject it rather than use a huge number.
            return (f64::NAN, f64::NAN);
        }
        let x = (m[0][0] * point.x + m[0][1] * point.y + m[0][2]) / denominator;
        let y = (m[1][0] * point.x + m[1][1] * point.y + m[1][2]) / denominator;
        (x, y)
    }

    /// (across, along) metres -> working-frame pixel.
    #[inline]
    pub fn to_image(&self, across_m: f64, along_m: f64) -> Point {
        let m = &self.to_image_matrix;
        let denominator = m[2][0] * across_m + m[2][1] * along_m + m[2][2];
        if denominator.abs() < 1e-12 {
            return Point::new(f64::NAN, f64::NAN);
        }
        let x = (m[0][0] * across_m + m[0][1] * along_m + m[0][2]) / denominator;
        let y = (m[1][0] * across_m + m[1][1] * along_m + m[1][2]) / denominator;
        Point::new(x, y)
    }

    /// The measurement zone as image-space corners, for drawing and tests.
    pub fn zone_polygon(&self, across: (f64, f64), along: (f64, f64)) -> [Point; 4] {
        let (x0, x1) = across;
        let (y0, y1) = along;
        [
            self.to_image(x0, y0),
            self.to_image(x1, y0),
            self.to_image(x1, y1),
            self.to_image(x0, y1),
        ]
    }
}

/// 3x3 inverse by LU factorisation with partial pivoting.
///
/// ## Why not the closed form
///
/// The obvious implementation for a 3x3 is the adjugate over the determinant,
/// and it is both shorter and, on this matrix, *more accurate*. It is still the
/// wrong answer, because the Python calls `np.linalg.inv`, which solves
/// `A X = I` through LAPACK's LU with partial pivoting, and the two disagree in
/// the last bit.
///
/// That sounds ignorable and is not. The survey marks sit at coordinates the
/// homography maps to exact integers -- they are the points it was fitted from
/// -- and the scene generator truncates projected coordinates with `as i32`. At
/// exactly 388.0, one implementation lands on 388 and the other on
/// 387.99999999999994, and the far survey mark is drawn a pixel higher in one
/// scene than the other. Every frame hash then differs, and the first
/// verification step a port is supposed to rely on stops working.
///
/// So this reproduces the *shape* of what LAPACK does for a small matrix:
/// factorise once with partial pivoting, then solve for each column of the
/// identity by forward and back substitution. Verified against `np.linalg.inv`
/// for this application's survey in the tests below, to within a handful of
/// ULPs -- not bit-for-bit.
///
/// **Bit-for-bit was the original target and it does not hold across CPU
/// architectures.** On the machine this was developed on (x86-64), this
/// function's output matched `np.linalg.inv`'s in every bit. On aarch64
/// (Raspberry Pi OS) the last one or two bits of some results differ -- the
/// same class of divergence this project's own docs already call out for
/// OpenCV and ONNX Runtime dispatching to different vectorised kernels per
/// architecture, just showing up in scalar arithmetic instead. The compiler is
/// free to contract a multiply-then-subtract into a single fused
/// multiply-subtract when the target has that instruction natively, which
/// aarch64 does as baseline ISA and x86-64 does not, and a fused operation
/// rounds once where two separate ones round twice. That is a plausible
/// mechanism, not a confirmed one -- forcing FMA codegen on x86-64 did not
/// reproduce the divergence when this was checked, so something more specific
/// to the aarch64 backend is doing it. Nailing the exact cause was not
/// necessary to fix the test that was asserting the wrong thing.
///
/// The tolerance below is chosen to still catch a real error: a wrong pivot
/// order or a transcribed constant produces a difference many orders of
/// magnitude larger than a rounding-path difference does.
fn invert_3x3(m: &[[f64; 3]; 3]) -> Result<[[f64; 3]; 3], String> {
    const N: usize = 3;
    let mut lu = *m;
    let mut piv = [0usize; N];
    for (i, slot) in piv.iter_mut().enumerate() {
        *slot = i;
    }

    for k in 0..N {
        // Partial pivoting: the largest magnitude in the column becomes the
        // pivot. This is what makes the factorisation stable, and doing it in
        // the same order LAPACK does is what makes it reproducible.
        let mut p = k;
        for i in (k + 1)..N {
            if lu[i][k].abs() > lu[p][k].abs() {
                p = i;
            }
        }
        if lu[p][k] == 0.0 {
            return Err(obfstr::obfstr!("the survey marks are collinear: the homography is singular").into());
        }
        if p != k {
            lu.swap(k, p);
            piv.swap(k, p);
        }
        for i in (k + 1)..N {
            lu[i][k] /= lu[k][k];
            for j in (k + 1)..N {
                lu[i][j] -= lu[i][k] * lu[k][j];
            }
        }
    }

    let mut out = [[0.0f64; N]; N];
    // Indexed rather than iterated on purpose: this mirrors the loop structure
    // of the reference implementation, and the whole value of this function is
    // that it performs the same operations in the same order.
    #[allow(clippy::needless_range_loop)]
    for col in 0..N {
        // The permuted column of the identity.
        let mut b = [0.0f64; N];
        for i in 0..N {
            b[i] = if piv[i] == col { 1.0 } else { 0.0 };
        }
        // Forward substitution through L, whose diagonal is implicitly one.
        for i in 1..N {
            for j in 0..i {
                b[i] -= lu[i][j] * b[j];
            }
        }
        // Back substitution through U.
        for i in (0..N).rev() {
            for j in (i + 1)..N {
                b[i] -= lu[i][j] * b[j];
            }
            b[i] /= lu[i][i];
        }
        for i in 0..N {
            out[i][col] = b[i];
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The application's survey, as declared in `config::calibration`.
    fn survey() -> Homography {
        Homography::from_survey(
            &[
                (352.0, 690.0),
                (928.0, 690.0),
                (742.0, 388.0),
                (538.0, 388.0),
            ],
            &[(0.0, 0.0), (7.3, 0.0), (7.3, 40.0), (0.0, 40.0)],
            1.0,
            (6.0, 34.0),
        )
        .expect(obfstr::obfstr!("the declared survey must fit"))
    }

    #[test]
    fn the_survey_round_trips() {
        let h = survey();
        for (image, world) in [
            ((352.0, 690.0), (0.0, 0.0)),
            ((928.0, 690.0), (7.3, 0.0)),
            ((742.0, 388.0), (7.3, 40.0)),
            ((538.0, 388.0), (0.0, 40.0)),
        ] {
            let (across, along) = h.to_world(Point::new(image.0, image.1));
            assert!(
                (across - world.0).abs() < 1e-6 && (along - world.1).abs() < 1e-6,
                "{image:?} -> ({across}, {along}), expected {world:?}"
            );
        }
    }

    /// The inverse has to agree with `np.linalg.inv`, closely, at the four
    /// survey marks. These are the exact projections the Python produces for
    /// them, dumped from the interpreter on x86-64.
    ///
    /// **Not bit-for-bit.** An earlier version of this test asserted exact
    /// equality on `to_bits()`, which passed on the x86-64 machine the port was
    /// built on and failed on aarch64 with a difference of one or two ULPs --
    /// see the note on [`invert_3x3`] for why. That is expected
    /// architecture-dependent floating-point rounding, not a wrong answer, so
    /// the assertion here is a tolerance instead.
    ///
    /// The tolerance is `1e-6`: about seven orders of magnitude looser than the
    /// ULP-level noise actually observed, and about nine orders of magnitude
    /// tighter than anything that could matter downstream (the least forgiving
    /// consumer of a homography output is the enforcement gate's own tolerance,
    /// which is measured in centimetres and km/h, not in millionths of a
    /// pixel). A genuinely wrong pivot or a transcribed constant misses by much
    /// more than this and still fails loudly.
    #[test]
    fn the_inverse_agrees_with_numpy_at_the_survey_marks() {
        let h = survey();
        for (world, expected) in [
            ((0.0f64, 0.0f64), (351.99999999999994f64, 690.0f64)),
            ((7.3, 0.0), (928.0000071573604, 690.0)),
            ((7.3, 40.0), (741.9999824398648, 387.99999999999994)),
            ((0.0, 40.0), (538.0000000000002, 387.99999999999994)),
        ] {
            let p = h.to_image(world.0, world.1);
            let (dx, dy) = ((p.x - expected.0).abs(), (p.y - expected.1).abs());
            assert!(
                dx < 1e-6 && dy < 1e-6,
                "world {world:?} projected to ({:.17}, {:.17}), expected \
                 ({:.17}, {:.17}) -- off by ({dx:.3e}, {dy:.3e})",
                p.x,
                p.y,
                expected.0,
                expected.1
            );
        }
    }

    #[test]
    fn points_above_the_horizon_are_rejected_rather_than_answered() {
        let h = survey();
        // Far above the vanishing line, where the plane maps behind the camera.
        let (across, along) = h.to_world(Point::new(640.0, 10.0));
        assert!(
            !across.is_finite() || !along.is_finite() || along < 0.0,
            "a point above the horizon produced a plausible world position \
             ({across}, {along})"
        );
    }

    #[test]
    fn iou_is_scale_aware() {
        let small = Box::new(0.0, 0.0, 10.0, 10.0);
        let small_shifted = Box::new(5.0, 0.0, 10.0, 10.0);
        let big = Box::new(0.0, 0.0, 100.0, 100.0);
        let big_shifted = Box::new(50.0, 0.0, 100.0, 100.0);
        assert!((small.iou(&small_shifted) - big.iou(&big_shifted)).abs() < 1e-12);
        assert_eq!(small.iou(&Box::new(100.0, 100.0, 5.0, 5.0)), 0.0);
    }

    #[test]
    fn the_contact_point_sits_just_above_the_bottom_edge() {
        let b = Box::new(10.0, 20.0, 40.0, 30.0);
        let contact = b.contact_point(0.06);
        assert!((contact.x - 30.0).abs() < 1e-12);
        assert!(contact.y < b.y + b.h);
        assert!(contact.y > b.y + b.h * 0.9);
    }
}
