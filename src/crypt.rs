//! Data-at-rest encryption for the numeric constants (targets A2/A4).
//!
//! The OLLVM pass obfuscates *control flow*, but it leaves `f64`/`i64` constants
//! as plaintext immediates in `.rodata` -- the tuning values and the calibration
//! survey are still recoverable with a float scan of the shipped binary. This
//! module closes that: a constant is stored XORed against a key on its raw bit
//! pattern, and decoded at run time behind an optimisation barrier.
//!
//! Inline, with no shared decoder
//! ------------------------------
//! The decode is expanded **inline at every use site** by [`encf!`] / [`enci!`],
//! with the key laundered through [`core::hint::black_box`]. `black_box` is an
//! optimisation barrier: the optimiser must treat its output as an unknown
//! value, so `ENC ^ black_box(K)` cannot be constant-folded back to the plaintext
//! bits at compile time -- only the ciphertext `ENC` reaches the binary.
//!
//! There is deliberately **no `#[inline(never)] dec_*` function**. A shared leaf
//! decoder shows up as one tiny routine called (`bl`) from hundreds of sites with
//! the ciphertext as an immediate -- exactly the fan-in an emulate-and-log tool
//! keys on: run that one function over every call site's immediate, read the
//! return register, and every constant falls out at once. Expanding inline
//! removes that single choke-point; the XOR happens in-register at the use site
//! and there is no return value to harvest.
//!
//! Bit-exact by construction: XOR on `to_bits()`/`from_bits()` reproduces the
//! identical `f64`, so the result CSV -- and thus the oracle -- does not move.
//!
//! Honest limit
//! ------------
//! This raises the bar against a *static* read of the binary and against the
//! specific "find the decoder, read its return" technique -- not against the
//! attack class. The decoded value still has to exist at run time:
//!
//! * if the compiler keeps it in a register and it is consumed immediately (a
//!   compare), a memory-write log never sees it -- this is why constants used
//!   in place survive an emulate-and-log sweep;
//! * but if it is spilled to the stack under register pressure, or stored into
//!   the long-lived `Settings`, that write is logged regardless of the `bl`; and
//! * a live RAM dump exposes any value resident at the moment of the dump.
//!
//! So keep the highest-value constants consumed in place (never stored) to stay
//! register-only, and treat this as cost against *this* tooling, not safety. The
//! key is an embedded immediate; deriving it at run time from state an emulator
//! cannot reproduce is the next bar (anti-emulation).

/// The obfuscation key. Embedded, so it only forces an attacker to run/emulate
/// the decode rather than read `.rodata`.
pub const K: u64 = 0x9E3779B97F4A7C15;

/// Encrypt an `f64` literal at compile time; decode it **inline** at run time.
///
/// `encf!(1.35)` reads as the value in source but ships only ciphertext. The
/// decode is inline (no `bl` to a shared decoder), and the key passes through
/// `black_box`, so the XOR cannot be constant-folded back to the plaintext bits.
#[macro_export]
macro_rules! encf {
    ($v:expr) => {{
        const ENC: u64 = ($v as f64).to_bits() ^ $crate::crypt::K;
        f64::from_bits(ENC ^ ::core::hint::black_box($crate::crypt::K))
    }};
}

/// Encrypt an `i64` literal at compile time; decode it **inline** at run time.
///
/// Same barrier as [`encf!`]: inline XOR against a `black_box`-laundered key, no
/// shared decoder function.
#[macro_export]
macro_rules! enci {
    ($v:expr) => {{
        const ENC: u64 = ($v as i64 as u64) ^ $crate::crypt::K;
        (ENC ^ ::core::hint::black_box($crate::crypt::K)) as i64
    }};
}
