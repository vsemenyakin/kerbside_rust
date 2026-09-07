//! Inline obfuscation of numeric constants and diagnostic strings, with an
//! optional decode key bound to the running code's integrity.
//!
//! Constant encryption (always available)
//! --------------------------------------
//! [`encf!`]/[`enci!`] store an `f64`/`i64` XORed against [`K`] and decode it
//! **inline at every use site**, with the key laundered through
//! [`core::hint::black_box`] so the XOR cannot be constant-folded back to the
//! plaintext bits -- only the ciphertext reaches `.rodata`. There is deliberately
//! **no shared `#[inline(never)]` decoder**: one leaf routine called from hundreds
//! of sites with the ciphertext as an immediate is exactly the fan-in an
//! emulate-and-log tool keys on. Expanding inline removes that choke-point.
//!
//! Code-integrity-bound key (feature `self-integrity`, Linux/ELF)
//! -------------------------------------------------------------
//! Without the feature, [`__key`] is just `K` behind a `black_box`. With it, the
//! key is derived once at start-up (call [`init_keying`] first) as
//! `getpagesize() ^ text_hash(.text) ^ SALT2`, where the shipped binary's `SALT2`
//! is patched post-build (by the `seal` tool) to `page_size ^ text_hash ^ K`. So
//! the key resolves to `K` **only** on the genuine hardware running the unpatched
//! code:
//!
//! * patch any code byte (to splice in a logger, a breakpoint, a detour) and the
//!   `.text` hash moves, the key is wrong, and every constant/string decodes to
//!   garbage -- there is no compare to NOP, the key *is* a function of the code;
//! * under an emulator that stubs `getpagesize` the probe is wrong, so the same
//!   failure closed applies.
//!
//! Honest limits: this defeats the offline emulator and the binary-only patcher;
//! it does not beat a live-root RAM dump, and it is an arms race.

/// The compile-time XOR key: ciphertext is `plaintext ^ K`.
pub const K: u64 = 0x9E3779B97F4A7C15;

/// Placeholder the `seal` tool overwrites in a shipped binary. Public so the tool
/// finds it by value; see [`crate::fnv1a`] and `src/bin/seal.rs`.
pub const SALT2_SENTINEL: u64 = 0xA1B2_C3D4_E5F6_0718;

/// FNV-1a over a byte slice. Cheap, `core`-only, and the single definition shared
/// by the run-time [`self-integrity`] key and the offline `seal` tool, so the two
/// always agree on the `.text` hash.
///
/// [`self-integrity`]: index.html
#[inline]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// -- run-time decode key ----------------------------------------------------

// The code-bound key needs the ELF program headers (`getauxval`), so it is
// Linux-only. Without `self-integrity`, or on any other OS, the key is just `K`
// laundered through `black_box` -- so the crate compiles and behaves correctly
// everywhere, and `self-integrity` is simply a no-op off Linux.

/// The key XORed against ciphertext at each decode site. Here it is `K` laundered
/// through `black_box`; it inlines to in-register arithmetic with no call.
#[cfg(not(all(feature = "self-integrity", target_os = "linux")))]
#[inline(always)]
pub fn __key() -> u64 {
    ::core::hint::black_box(K)
}

/// No-op here: the key is the constant `K`.
#[cfg(not(all(feature = "self-integrity", target_os = "linux")))]
#[inline(always)]
pub fn init_keying() {}

#[cfg(all(feature = "self-integrity", target_os = "linux"))]
mod keying;

#[cfg(all(feature = "self-integrity", target_os = "linux"))]
pub use keying::{init as init_keying, key as __key};

// -- macros -----------------------------------------------------------------

/// Encrypt an `f64` literal at compile time; decode it **inline** at run time.
///
/// `encf!(1.35)` reads as the value in source but ships only ciphertext, decoded
/// against [`__key`] behind a `black_box` so it cannot be constant-folded.
#[macro_export]
macro_rules! encf {
    ($v:expr) => {{
        const ENC: u64 = ($v as f64).to_bits() ^ $crate::K;
        f64::from_bits(ENC ^ $crate::__key())
    }};
}

/// Encrypt an `i64` literal at compile time; decode it **inline** at run time.
#[macro_export]
macro_rules! enci {
    ($v:expr) => {{
        const ENC: u64 = ($v as i64 as u64) ^ $crate::K;
        (ENC ^ $crate::__key()) as i64
    }};
}

/// An `obfstr!` for diagnostic strings that a shipped build drops entirely.
///
/// Without `redact` this is `obfstr!` (the message is encrypted but still helps in
/// a debuggable build). With `redact` it expands to an **empty `&str`**, so the
/// literal never enters the binary -- for text that never reaches the program's
/// real output, which is pure attack surface.
#[cfg(not(feature = "redact"))]
#[macro_export]
macro_rules! obfstr_err {
    ($s:literal) => {
        ::obfstr::obfstr!($s)
    };
}

/// `redact` variant: the message is compiled out to an empty string.
#[cfg(feature = "redact")]
#[macro_export]
macro_rules! obfstr_err {
    ($s:literal) => {
        ""
    };
}
