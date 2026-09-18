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

/// Fixed non-magic default key for dev/test builds (when `CRYPT_K` is unset).
const DEFAULT_K: u64 = 0x3063_2F5A_7B1C_9E4D;

/// Parse a `u64` from a decimal or `0x`-prefixed hex string, at compile time.
/// `u64::from_str_radix` is not `const`, so parse by hand. A bad digit is a
/// compile error (const `panic!`), never silent garbage.
const fn parse_k(s: &str) -> u64 {
    let b = s.as_bytes();
    let (mut i, base) = if b.len() >= 2 && b[0] == b'0' && (b[1] | 32) == b'x' {
        (2usize, 16u64)
    } else {
        (0usize, 10u64)
    };
    let mut v: u64 = 0;
    while i < b.len() {
        let c = b[i];
        let d = match c {
            b'0'..=b'9' => (c - b'0') as u64,
            b'a'..=b'f' => (c - b'a' + 10) as u64,
            b'A'..=b'F' => (c - b'A' + 10) as u64,
            b'_' => {
                i += 1;
                continue;
            }
            _ => panic!("CRYPT_K: invalid digit"),
        };
        v = v.wrapping_mul(base).wrapping_add(d);
        i += 1;
    }
    v
}

/// The compile-time XOR key: ciphertext is `plaintext ^ K`. Baked from the
/// `CRYPT_K` env var **at compile time** (see scripts/build.sh) so a shipped build
/// uses a fresh random value rather than a recognisable magic constant; a dev
/// build with `CRYPT_K` unset falls back to a fixed non-magic default.
pub const K: u64 = match option_env!("CRYPT_K") {
    Some(s) => parse_k(s),
    None => DEFAULT_K,
};

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

/// Per-call-site salt, from the invocation's line and column. Diversifies the key
/// per site so that one recovered global key does not decrypt every constant with
/// a single XOR. A **macro**, not a fn, so it expands to a compile-time constant
/// expression with no call to recognise.
#[doc(hidden)]
#[macro_export]
macro_rules! __site_salt {
    ($line:expr, $col:expr) => {
        (($line as u64).wrapping_mul(0xff51_afd7_ed55_8ccd)
            ^ ($col as u64).wrapping_mul(0xc4ce_b9fe_1a85_ec53)
            ^ 0x2545_f491_4f6c_dd1d)
    };
}

/// Non-linear per-site key mix: rotate the key by a site-dependent amount, then
/// fold in the salt. Non-linear over XOR, so the effective key differs at every
/// site and cannot be collapsed to one global key by a linear scan.
///
/// A **macro**, not a fn: it expands textually at every use site, so there is no
/// shared `bl`-called decoder to fingerprint, and MBA obfuscates each expanded
/// copy independently. `$salt` is always a `const`, so its double use is free of
/// side effects.
#[doc(hidden)]
#[macro_export]
macro_rules! __mix {
    ($key:expr, $salt:expr) => {
        (($key).rotate_left((($salt) & 63) as u32) ^ ($salt).wrapping_mul(0x9ddf_ea08_eb38_2d69))
    };
}

/// Encrypt an `f64` literal at compile time; decode it **inline** at run time.
///
/// `encf!(1.35)` reads as the value in source but ships only ciphertext, decoded
/// against a *per-site* key ([`__mix`] of [`__key`] and a site salt) behind a
/// `black_box` so it cannot be constant-folded.
#[macro_export]
macro_rules! encf {
    ($v:expr) => {{
        const SALT: u64 = $crate::__site_salt!(line!(), column!());
        const ENC: u64 = ($v as f64).to_bits() ^ $crate::__mix!($crate::K, SALT);
        f64::from_bits(ENC ^ $crate::__mix!($crate::__key(), SALT))
    }};
}

/// Encrypt an `i64` literal at compile time; decode it **inline** at run time.
#[macro_export]
macro_rules! enci {
    ($v:expr) => {{
        const SALT: u64 = $crate::__site_salt!(line!(), column!());
        const ENC: u64 = ($v as i64 as u64) ^ $crate::__mix!($crate::K, SALT);
        (ENC ^ $crate::__mix!($crate::__key(), SALT)) as i64
    }};
}

/// Re-export of `obfstr`'s macro, so `obfstr_err!` can reach it through `$crate`
/// from any downstream crate. Without this the macro expanded to `::obfstr::obfstr!`,
/// which resolves in the *caller's* crate -- so every user had to add their own
/// `obfstr` dependency (kerbside happened to have one, which hid the bug). Now the
/// caller only needs to depend on `crypt`.
#[cfg(not(feature = "redact"))]
#[doc(hidden)]
pub use ::obfstr::obfstr as __obfstr;

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
        $crate::__obfstr!($s)
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
