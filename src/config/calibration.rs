//! Ground-plane calibration: the mapping from image pixels to road metres.
//!
//! A speed camera is only as good as its survey. Four points are marked on the
//! road surface at known real-world coordinates -- in practice painted marks
//! measured with a tape during commissioning -- and the homography between them
//! and their image positions turns any pixel on the road into a position in
//! metres.
//!
//! Everything downstream depends on this being right, and nothing downstream
//! can detect that it is wrong. A 3% error in the survey is a 3% error in every
//! speed this device ever reports, and it will look completely plausible.
//!
//! Reverse-engineering note (target T3)
//! -----------------------------------
//! These numbers are the difference between a device that measures metres and
//! one that measures pixels. In the Python they are plain text; a naive compiled
//! build turns them into `f64` immediates in `.rodata`, which a constant scan
//! recovers in seconds.
//!
//! So they are not stored in the clear here. The values are held as their
//! IEEE-754 bit patterns XOR'd against a keystream ([`CAL_ENC`]), which reads as
//! random bytes, and reconstructed once at startup by [`decode_scalar`]. What
//! this buys, precisely, is that the survey is no longer recoverable
//! *statically*: a `strings`/immediate scan finds nothing. What it does **not**
//! buy is secrecy against someone who holds the device -- the keystream ships in
//! the binary, and the decoded values (and the homography built from them) are
//! in memory the moment the scene is constructed, so a debugger or a memory dump
//! at startup still recovers them. This moves T3 from "static scan" to "requires
//! dynamic analysis"; the report should record it as that, not as "protected".
//!
//! The transform is bitwise exact by construction -- it round-trips
//! `f64::to_bits`/`from_bits`, never re-derives a value arithmetically -- because
//! the survey marks project to exactly integer image coordinates and a last-bit
//! change moves a painted mark one pixel and breaks every frame hash. The
//! `decodes_bit_exactly` test pins that, and `make_clip --hash` is the
//! end-to-end check.

crate::settings_group! {
    pub struct CalibrationSettings {
        // Image coordinates of the four survey marks, in FULL-resolution
        // pixels, clockwise from the near-left. These correspond to
        // WORLD_POINTS below. Decoded from CAL_ENC[0..8] as four (x, y) pairs.
        IMAGE_POINTS: Vec<(f64, f64)> = decode_points(0, 4),
        // The same four marks in road coordinates, metres. X across the
        // carriageway, Y along it, origin at the near-left mark.
        // Decoded from CAL_ENC[8..16].
        WORLD_POINTS: Vec<(f64, f64)> = decode_points(8, 4),

        // The stretch of road, in metres along Y, over which speed is measured.
        // Starting past the near mark and ending before the far one keeps the
        // fit away from both edges of the calibrated quad, where a small survey
        // error has the most leverage.
        ZONE_START_M: f64 = decode_scalar(16),
        ZONE_END_M: f64 = decode_scalar(17),

        // A vehicle's ground-contact point is taken this far up from the bottom
        // of its box, as a fraction of box height. Exactly at the bottom edge
        // picks up the shadow; higher up picks up the bonnet, which is not on
        // the road.
        CONTACT_POINT_RATIO: f64 = decode_scalar(18),
    }
}

// --------------------------------------------------------------------------
// The encoded survey and its decoder.
//
// CAL_ENC is `value.to_bits() ^ keystream(index)` for each of the 19 values, in
// the flat layout the defaults above index into:
//   0..8   IMAGE_POINTS (4 xy pairs)
//   8..16  WORLD_POINTS (4 xy pairs)
//   16     ZONE_START_M
//   17     ZONE_END_M
//   18     CONTACT_POINT_RATIO
//
// To change the survey, edit EXPECTED in the tests below and regenerate:
//   cargo test --lib config::calibration::tests::regenerate_cal_enc \
//       -- --ignored --nocapture
// then paste the printed array over CAL_ENC. The decodes_bit_exactly test then
// guards that the two agree, and make_clip --hash that the frames are unchanged.
// --------------------------------------------------------------------------

const CAL_SEED: u64 = 0x5F3E_9C17_A24B_D081;

const CAL_ENC: [u64; 19] = [
    0xA11596E5D33B95B0,
    0xC1E069B67743D4CF,
    0x3F385C067AB3D795,
    0x198B1D201E5B8C1E,
    0xE01E111BFF2859EC,
    0xEEF9A935AD970C10,
    0x55FED0396C0647CE,
    0x00123698FA7A2B4C,
    0xF01412D4583870A1,
    0x16A5B5F4141E71BD,
    0x0D2BE2E43E7F876D,
    0x584F58C03AEA33E6,
    0xC6647DE1023DF830,
    0xDDAA3C818E10DA4C,
    0x0F5E10B951A731F2,
    0xFDC238F473B47071,
    0x84807D4054DF52CB,
    0x228F12D9EB52A9FD,
    0x06F6213EFDE275D7,
];

/// splitmix64 over the seed and index. `#[inline(never)]` plus a `black_box` on
/// the seed is what stops the optimiser proving the whole decode at compile time
/// and re-materialising the plaintext immediates this is meant to remove.
#[inline(never)]
fn keystream(index: u64) -> u64 {
    let seed = std::hint::black_box(CAL_SEED);
    let mut z = seed.wrapping_add(index.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Reconstruct one value, bit-exactly, from its encoded slot.
#[inline(never)]
fn decode_scalar(index: usize) -> f64 {
    f64::from_bits(CAL_ENC[index] ^ keystream(index as u64))
}

/// Reconstruct `count` consecutive (x, y) pairs starting at `start`.
fn decode_points(start: usize, count: usize) -> Vec<(f64, f64)> {
    (0..count)
        .map(|k| (decode_scalar(start + 2 * k), decode_scalar(start + 2 * k + 1)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The survey in the clear. Test-only: this file's test module is compiled
    // into the test harness, never into a shipped binary, so the plaintext here
    // is a maintenance aid and a regression guard, not a leak.
    const EXPECTED: [f64; 19] = [
        352.0, 690.0, 928.0, 690.0, 742.0, 388.0, 538.0, 388.0, // IMAGE_POINTS
        0.0, 0.0, 7.3, 0.0, 7.3, 40.0, 0.0, 40.0, // WORLD_POINTS
        6.0,  // ZONE_START_M
        34.0, // ZONE_END_M
        0.06, // CONTACT_POINT_RATIO
    ];

    /// The whole contract: the decoder reproduces the survey to the last bit.
    #[test]
    fn decodes_bit_exactly() {
        let c = CalibrationSettings::default();
        let mut got: Vec<f64> = Vec::new();
        for (x, y) in &c.IMAGE_POINTS {
            got.push(*x);
            got.push(*y);
        }
        for (x, y) in &c.WORLD_POINTS {
            got.push(*x);
            got.push(*y);
        }
        got.push(c.ZONE_START_M);
        got.push(c.ZONE_END_M);
        got.push(c.CONTACT_POINT_RATIO);

        assert_eq!(got.len(), EXPECTED.len(), "decoded the wrong number of values");
        for (i, (g, e)) in got.iter().zip(EXPECTED).enumerate() {
            assert_eq!(
                g.to_bits(),
                e.to_bits(),
                "value {i} decoded to {g} (bits {:#018x}), expected {e} (bits {:#018x})",
                g.to_bits(),
                e.to_bits(),
            );
        }
    }

    /// Print a fresh CAL_ENC for the current EXPECTED. Ignored by default; run
    /// with `-- --ignored --nocapture` after editing the survey, then paste the
    /// output over CAL_ENC.
    #[test]
    #[ignore]
    fn regenerate_cal_enc() {
        println!("const CAL_ENC: [u64; {}] = [", EXPECTED.len());
        for (i, v) in EXPECTED.iter().enumerate() {
            println!("    0x{:016X},", v.to_bits() ^ keystream(i as u64));
        }
        println!("];");
    }
}
