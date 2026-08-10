//! A bit-exact reimplementation of `numpy.random.default_rng`.
//!
//! The Python generates its scene from `np.random.default_rng(seed)`, and the
//! port's correctness oracle is a per-frame comparison of output. That
//! comparison only means anything if the *input* is identical, so this module
//! has to reproduce numpy's stream exactly -- not "a PCG64", but numpy's PCG64,
//! seeded through numpy's `SeedSequence`, consumed through numpy's bounded
//! integer and double routines.
//!
//! That is a stricter requirement than it first looks, and it is the single
//! most likely place for a port to go quietly wrong. Three separate things all
//! have to match:
//!
//! 1. **The seeding.** `SeedSequence` is a hash-mixing construction, not a
//!    passthrough. `default_rng(7)` does not start the generator at 7.
//! 2. **The output function.** PCG64 in numpy is the XSL-RR 128/64 variant,
//!    and it steps the state *before* emitting, not after.
//! 3. **How each distribution consumes the stream.** This is the subtle one.
//!    `integers(..., dtype=uint8)` does not draw a 64-bit word per value: it
//!    draws 32 bits, dispenses four bytes from them, and applies Lemire's
//!    debiasing with rejection. Get the buffering wrong and the values are
//!    still uniform, still deterministic, and completely different.
//!
//! Everything here is checked against the real numpy in the tests at the
//! bottom, with reference values dumped from the Python interpreter this port
//! is being compared against. If you change anything in this file, those tests
//! are the only thing standing between you and a scene that looks perfectly
//! plausible and matches nothing.
//!
//! Scope note: only the routines `source/road.rs` actually uses are here --
//! `random`, `uniform`, and bounded `integers` for `u8` and `i64`. This is not
//! a general numpy-compatible RNG and should not be extended into one casually;
//! each additional distribution is another exact-consumption contract.

/// Multiplier for the 128-bit LCG. PCG's default; numpy does not change it.
const PCG_MULTIPLIER: u128 = 0x2360_ED05_1FC6_5DA4_4385_DF64_9FCC_F645;

// SeedSequence's mixing constants, from numpy/random/bit_generator.pyx.
const INIT_A: u32 = 0x43b0_d7e5;
const MULT_A: u32 = 0x931e_8875;
const INIT_B: u32 = 0x8b51_f9dd;
const MULT_B: u32 = 0x58f3_8ded;
const MIX_MULT_L: u32 = 0xca01_f9dd;
const MIX_MULT_R: u32 = 0x4973_f715;
/// Half the width of a `u32`. numpy calls it XSHIFT and derives it the same way.
const XSHIFT: u32 = 16;

/// numpy's default entropy pool size. `default_rng(int)` never changes it.
const POOL_SIZE: usize = 4;

/// `numpy.random.SeedSequence`, restricted to a single integer seed.
///
/// Restricted deliberately: the full class accepts arbitrary entropy, spawn
/// keys and pool sizes, and each of those changes the mixing. The application
/// only ever seeds from `video.SCENE_SEED`, so supporting more would be
/// untested surface that looks supported.
struct SeedSequence {
    pool: [u32; POOL_SIZE],
}

impl SeedSequence {
    fn new(entropy: u32) -> Self {
        // numpy coerces the seed to an array of u32 words. A seed that fits in
        // 32 bits is a one-element array, which is every seed this application
        // uses; larger seeds would contribute further words here.
        let entropy_array = [entropy];
        let mut pool = [0u32; POOL_SIZE];
        let mut hash_const = INIT_A;

        // A closure would need to borrow `hash_const` mutably across the two
        // loops below, so it is written out as a local fn taking it by
        // reference. Same arithmetic as numpy's nested `hash()`.
        fn hash(value: u32, hash_const: &mut u32) -> u32 {
            let mut value = value ^ *hash_const;
            *hash_const = hash_const.wrapping_mul(MULT_A);
            value = value.wrapping_mul(*hash_const);
            value ^= value >> XSHIFT;
            value
        }
        fn mix(x: u32, y: u32) -> u32 {
            let mut result = MIX_MULT_L
                .wrapping_mul(x)
                .wrapping_sub(MIX_MULT_R.wrapping_mul(y));
            result ^= result >> XSHIFT;
            result
        }

        // Seed the pool, padding with hashed zeros when the entropy is shorter
        // than the pool -- which it always is here.
        for (i, slot) in pool.iter_mut().enumerate() {
            let word = entropy_array.get(i).copied().unwrap_or(0);
            *slot = hash(word, &mut hash_const);
        }

        // Mix every word into every other, so a late bit can affect an early
        // one. O(pool^2), and pool is 4.
        for i_src in 0..POOL_SIZE {
            for i_dst in 0..POOL_SIZE {
                if i_src != i_dst {
                    let hashed = hash(pool[i_src], &mut hash_const);
                    pool[i_dst] = mix(pool[i_dst], hashed);
                }
            }
        }

        // numpy has a third loop folding in entropy beyond the pool size. With
        // a single-word seed there is none, so it is omitted rather than
        // written as a loop that provably never runs.
        Self { pool }
    }

    /// numpy's `generate_state(n, np.uint64)`.
    ///
    /// It generates `2n` 32-bit words and *reinterprets* them as `n` 64-bit
    /// words. That reinterpretation is little-endian, which is a property of
    /// the platforms numpy supports rather than a documented guarantee -- and
    /// it is why the pairing below is `low | high << 32` and not the reverse.
    /// Both Windows on x86-64 and Raspberry Pi OS on aarch64 are little-endian,
    /// so this is exact on both.
    fn generate_state_u64(&self, n: usize) -> Vec<u64> {
        let n_words = n * 2;
        let mut words = Vec::with_capacity(n_words);
        let mut hash_const = INIT_B;
        for i in 0..n_words {
            let mut data_val = self.pool[i % POOL_SIZE];
            data_val ^= hash_const;
            hash_const = hash_const.wrapping_mul(MULT_B);
            data_val = data_val.wrapping_mul(hash_const);
            data_val ^= data_val >> XSHIFT;
            words.push(data_val);
        }
        words
            .chunks_exact(2)
            .map(|pair| u64::from(pair[0]) | (u64::from(pair[1]) << 32))
            .collect()
    }
}

/// numpy's PCG64 bit generator plus the buffering its callers rely on.
///
/// `has_uint32`/`uinteger` are not an optimisation detail that can be dropped:
/// they are generator state. A 32-bit draw takes the low half of a 64-bit word
/// and *stores the high half* for the next 32-bit draw, so a routine that
/// consumes 32-bit values leaves the generator in a different position than one
/// that consumes 64-bit values. Mixing the two -- which `road.rs` does, drawing
/// doubles and bounded integers from one generator -- only reproduces if this
/// carry-over is modelled.
pub struct NumpyRng {
    state: u128,
    inc: u128,
    has_uint32: bool,
    uinteger: u32,
}

impl NumpyRng {
    /// Equivalent to `numpy.random.default_rng(seed)`.
    pub fn new(seed: u32) -> Self {
        let words = SeedSequence::new(seed).generate_state_u64(4);
        let initstate = (u128::from(words[0]) << 64) | u128::from(words[1]);
        let initseq = (u128::from(words[2]) << 64) | u128::from(words[3]);

        let mut rng = Self {
            state: 0,
            // The increment is forced odd; that is what makes the LCG
            // full-period.
            inc: (initseq << 1) | 1,
            has_uint32: false,
            uinteger: 0,
        };
        rng.step();
        rng.state = rng.state.wrapping_add(initstate);
        rng.step();
        rng
    }

    #[inline]
    fn step(&mut self) {
        self.state = self
            .state
            .wrapping_mul(PCG_MULTIPLIER)
            .wrapping_add(self.inc);
    }

    /// One 64-bit output. Steps first, then emits -- the order matters.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.step();
        // XSL-RR: xor the two halves together, then rotate by the top 6 bits.
        let xored = ((self.state >> 64) as u64) ^ (self.state as u64);
        let rot = (self.state >> 122) as u32;
        xored.rotate_right(rot)
    }

    #[inline]
    fn next_u32(&mut self) -> u32 {
        if self.has_uint32 {
            self.has_uint32 = false;
            return self.uinteger;
        }
        let next = self.next_u64();
        self.has_uint32 = true;
        self.uinteger = (next >> 32) as u32;
        next as u32
    }

    /// numpy's `next_double`: 53 bits of mantissa, never 1.0.
    #[inline]
    pub fn next_double(&mut self) -> f64 {
        // 2^53. Written as a literal division by the reciprocal exactly as
        // numpy does it, because `x / 9007199254740992.0` and
        // `x * (1.0 / 9007199254740992.0)` are the same here only because the
        // divisor is a power of two -- do not "simplify" this to a division by
        // a non-power-of-two constant elsewhere.
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }

    /// `Generator.random()`.
    pub fn random(&mut self) -> f64 {
        self.next_double()
    }

    /// `Generator.uniform(low, high)`.
    ///
    /// numpy computes `low + range * next_double`, and the order of those
    /// operations is observable in the last bits of the result.
    pub fn uniform(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.next_double()
    }

    /// One byte off the 32-bit buffer, refilling it when empty.
    ///
    /// `bcnt`/`buf` are owned by the *call site* -- numpy declares them local
    /// to each fill, so every call to `integers(..., dtype=uint8)` starts with
    /// an empty byte buffer and any leftover bytes from the previous call are
    /// discarded. Hoisting them into `NumpyRng` would be tidier and wrong.
    #[inline]
    fn buffered_u8(&mut self, bcnt: &mut i32, buf: &mut u32) -> u8 {
        if *bcnt == 0 {
            *buf = self.next_u32();
            *bcnt = 3;
        } else {
            *buf >>= 8;
            *bcnt -= 1;
        }
        (*buf & 0xff) as u8
    }

    /// Lemire's bounded generation for a byte, with numpy's rejection loop.
    ///
    /// The multiply-and-take-the-high-half trick is biased on its own; the
    /// rejection below removes the bias. The threshold is only computed when
    /// the low half could indicate a biased draw, which is why the common path
    /// costs one multiply and no division.
    #[inline]
    fn bounded_lemire_u8(&mut self, range: u8, bcnt: &mut i32, buf: &mut u32) -> u8 {
        debug_assert!(range != 0xFF, "Lemire's algorithm cannot represent a full-range u8");
        let range_excl = u16::from(range) + 1;
        let mut m = u16::from(self.buffered_u8(bcnt, buf)) * range_excl;
        let mut leftover = (m & 0xFF) as u8;
        if u16::from(leftover) < range_excl {
            let threshold = (u8::MAX - range) % (range_excl as u8);
            while leftover < threshold {
                m = u16::from(self.buffered_u8(bcnt, buf)) * range_excl;
                leftover = (m & 0xFF) as u8;
            }
        }
        (m >> 8) as u8
    }

    /// Lemire's bounded generation for a 32-bit range.
    #[inline]
    fn bounded_lemire_u32(&mut self, range: u32) -> u32 {
        let range_excl = u64::from(range) + 1;
        let mut m = u64::from(self.next_u32()) * range_excl;
        let mut leftover = m as u32;
        if u64::from(leftover) < range_excl {
            let threshold = ((u64::from(u32::MAX) - u64::from(range)) % range_excl) as u32;
            while leftover < threshold {
                m = u64::from(self.next_u32()) * range_excl;
                leftover = m as u32;
            }
        }
        (m >> 32) as u32
    }

    /// Lemire's bounded generation for a 64-bit range.
    #[inline]
    fn bounded_lemire_u64(&mut self, range: u64) -> u64 {
        let range_excl = u128::from(range) + 1;
        let mut m = u128::from(self.next_u64()) * range_excl;
        let mut leftover = m as u64;
        if u128::from(leftover) < range_excl {
            let threshold = ((u128::from(u64::MAX) - u128::from(range)) % range_excl) as u64;
            while leftover < threshold {
                m = u128::from(self.next_u64()) * range_excl;
                leftover = m as u64;
            }
        }
        (m >> 64) as u64
    }

    /// `Generator.integers(low, high, size=count, dtype=np.uint8)`, half-open.
    pub fn integers_u8(&mut self, low: u8, high: u8, count: usize) -> Vec<u8> {
        let range = high - low - 1;
        let mut out = Vec::with_capacity(count);
        // Fresh per fill -- see `buffered_u8`.
        let mut bcnt: i32 = 0;
        let mut buf: u32 = 0;
        if range == 0 {
            out.resize(count, low);
            return out;
        }
        for _ in 0..count {
            out.push(low + self.bounded_lemire_u8(range, &mut bcnt, &mut buf));
        }
        out
    }

    /// `Generator.integers(low, high, size=count)`, half-open, default dtype.
    ///
    /// numpy's 64-bit fill routes anything that fits in 32 bits through the
    /// 32-bit generator instead. That is not an optimisation detail either: it
    /// consumes half as much of the stream, so a port that always drew 64 bits
    /// would diverge on the first vehicle.
    pub fn integers_i64(&mut self, low: i64, high: i64, count: usize) -> Vec<i64> {
        let range = (high - low - 1) as u64;
        let mut out = Vec::with_capacity(count);
        if range == 0 {
            out.resize(count, low);
            return out;
        }
        if range <= u64::from(u32::MAX) {
            for _ in 0..count {
                out.push(low.wrapping_add(i64::from(self.bounded_lemire_u32(range as u32))));
            }
        } else {
            for _ in 0..count {
                out.push(low.wrapping_add(self.bounded_lemire_u64(range) as i64));
            }
        }
        out
    }

    /// Scalar form of [`Self::integers_i64`], for `rng.integers(a, b)`.
    pub fn integer_i64(&mut self, low: i64, high: i64) -> i64 {
        self.integers_i64(low, high, 1)[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every expected value below was dumped from the interpreter this port is
    // measured against:
    //
    //     venv/Scripts/python.exe -c "import numpy as np; ..."
    //
    // They are not derived from the algorithm description. That is the point --
    // the description is what a reimplementation gets wrong.

    #[test]
    fn seed_sequence_matches_numpy() {
        let ss = SeedSequence::new(7);
        assert_eq!(ss.pool, [0x7127d827, 0xdad86a24, 0x3d50d7a1, 0x3beaaf0c]);
        assert_eq!(
            ss.generate_state_u64(4),
            [
                0xead0f7017c326e58,
                0x0879c4f0f97e037a,
                0x623a8c4b6745675f,
                0xb3443fad60386cac
            ]
        );
    }

    #[test]
    fn raw_stream_matches_numpy() {
        let mut rng = NumpyRng::new(7);
        let got: Vec<u64> = (0..6).map(|_| rng.next_u64()).collect();
        assert_eq!(
            got,
            [
                0xa00641a9f1e54a8b,
                0xe5afcdbcaf266a95,
                0xc693565f940af962,
                0x39a72dabd56a2742,
                0x4cd7b2990e375145,
                0xdfa132d748fa2734
            ]
        );
    }

    // The literals below are pasted from numpy's own repr. Clippy suggests
    // truncating them to the shortest form that round-trips; leaving them
    // exactly as the reference printed them is the point, because that is what
    // makes a mismatch traceable to the interpreter it came from.
    #[allow(clippy::excessive_precision)]
    #[test]
    fn doubles_match_numpy() {
        let mut rng = NumpyRng::new(7);
        let got: Vec<f64> = (0..5).map(|_| rng.random()).collect();
        let expected: [f64; 5] = [
            0.62509546660466697,
            0.89721380096957548,
            0.77568569024519352,
            0.22520718999059186,
            0.30016628491122543,
        ];
        // Bit-for-bit, not approximately: a double that is merely close is a
        // double that will place a polygon corner on the other side of a pixel
        // boundary somewhere in a 1500-frame clip.
        for (g, e) in got.iter().zip(expected.iter()) {
            assert_eq!(g.to_bits(), e.to_bits(), "got {g:.17} expected {e:.17}");
        }
    }

    #[allow(clippy::excessive_precision)]
    #[test]
    fn uniform_matches_numpy() {
        let mut rng = NumpyRng::new(7);
        let got: Vec<f64> = (0..4).map(|_| rng.uniform(38.0, 78.0)).collect();
        let expected: [f64; 4] = [
            63.003818664186682,
            73.888552038783018,
            69.027427609807745,
            47.008287599623671,
        ];
        for (g, e) in got.iter().zip(expected.iter()) {
            assert_eq!(g.to_bits(), e.to_bits(), "got {g:.17} expected {e:.17}");
        }
    }

    #[test]
    fn bounded_bytes_match_numpy() {
        // The noise tiles: integers(0, 9, dtype=uint8).
        let mut rng = NumpyRng::new(7);
        assert_eq!(
            rng.integers_u8(0, 9, 24),
            [4, 2, 8, 8, 5, 2, 0, 5, 5, 3, 1, 6, 6, 7, 6, 8, 3, 8, 0, 5, 3, 3, 5, 6]
        );

        // The asphalt speckle: integers(0, 26, dtype=uint8).
        let mut rng = NumpyRng::new(7);
        assert_eq!(
            rng.integers_u8(0, 26, 24),
            [14, 7, 23, 24, 17, 6, 0, 16, 15, 10, 3, 17, 19, 20, 17, 23, 9, 25, 9, 8, 14, 20, 6, 3]
        );
    }

    #[test]
    fn bounded_integers_match_numpy() {
        // Vehicle entry frames: integers(0, 90), scalar.
        let mut rng = NumpyRng::new(7);
        assert_eq!(
            [
                rng.integer_i64(0, 90),
                rng.integer_i64(0, 90),
                rng.integer_i64(0, 90)
            ],
            [85, 56, 61]
        );

        // Vehicle colour: integers(35, 205, size=3). Two consecutive draws, so
        // the test also pins that a size-3 fill leaves the generator where
        // numpy leaves it.
        let mut rng = NumpyRng::new(7);
        assert_eq!(rng.integers_i64(35, 205, 3), [195, 141, 151]);
        assert_eq!(rng.integers_i64(35, 205, 3), [187, 133, 166]);
    }
}
