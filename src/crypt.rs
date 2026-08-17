//! Data-at-rest encryption for the numeric constants (targets A2/A4).
//!
//! The OLLVM pass obfuscates *control flow*, but it leaves `f64`/`i64` constants
//! as plaintext immediates in `.rodata` -- the tuning values and the calibration
//! survey are still recoverable with a float scan of the shipped binary. This
//! module closes that: a constant is stored XORed against a key on its raw bit
//! pattern, and decoded at run time behind an optimisation barrier.
//!
//! The one trap this avoids
//! ------------------------
//! If the key and the decode were `const`, the optimiser would fold them back to
//! the plaintext bits at compile time and the constant would reappear in
//! `.rodata`. `dec_*` is therefore `#[inline(never)]` and reads the key through
//! [`std::hint::black_box`], which the optimiser must treat as an unknown value.
//! Only the ciphertext reaches the binary.
//!
//! Bit-exact by construction: XOR on `to_bits()`/`from_bits()` reproduces the
//! identical `f64`, so the result CSV still matches to the bit. The `derive`
//! step and every consumer read the decoded value exactly as before.
//!
//! Honest limit: the decoded value lives in registers and in the `Settings`
//! object at run time, so a **RAM dump still exposes it**. This defeats a static
//! read of the binary on the medium (the secondary adversary), not a live dump.
//! Also, the key is an immediate inside `dec_*`; an attacker who reverses the
//! decoder recovers every constant at once. Rotate the key per module, or derive
//! it at run time from an embedded salt, to raise that bar further.

use std::hint::black_box;

/// The obfuscation key. Embedded, so it only forces an attacker to run/reverse
/// the decoder rather than read `.rodata`.
pub const K: u64 = 0x9E3779B97F4A7C15;

/// Decode an encrypted `f64`. Not inlined, and the key is laundered through
/// `black_box`, so the XOR cannot be constant-folded back to the plaintext bits.
#[inline(never)]
pub fn dec_f64(enc: u64) -> f64 {
    f64::from_bits(enc ^ black_box(K))
}

/// Decode an encrypted `i64`, same barrier.
#[inline(never)]
pub fn dec_i64(enc: u64) -> i64 {
    (enc ^ black_box(K)) as i64
}

/// Encrypt an `f64` literal at compile time; decode it at run time.
///
/// `encf!(1.35)` reads as the value in source but ships only ciphertext.
#[macro_export]
macro_rules! encf {
    ($v:expr) => {{
        const ENC: u64 = ($v as f64).to_bits() ^ $crate::crypt::K;
        $crate::crypt::dec_f64(ENC)
    }};
}

/// Encrypt an `i64` literal at compile time; decode it at run time.
#[macro_export]
macro_rules! enci {
    ($v:expr) => {{
        const ENC: u64 = ($v as i64 as u64) ^ $crate::crypt::K;
        $crate::crypt::dec_i64(ENC)
    }};
}
