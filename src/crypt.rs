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
//! value, so `ENC ^ key()` cannot be constant-folded back to the plaintext bits
//! at compile time -- only the ciphertext `ENC` reaches the binary. There is
//! deliberately **no `#[inline(never)] dec_*` function**: a shared leaf decoder
//! shows up as one tiny routine called (`bl`) from hundreds of sites with the
//! ciphertext as an immediate, exactly the fan-in an emulate-and-log tool keys
//! on. Expanding inline removes that single choke-point.
//!
//! Anti-emulation key (feature `anti-tamper`, dist only)
//! -----------------------------------------------------
//! The attack that actually reads these constants is dynamic: run the decode
//! under an emulator (Unicorn) and log the value it materialises. That works
//! because the key is a fixed constant, so the decode is correct *anywhere* --
//! including in an emulator that stubs the environment.
//!
//! Under `anti-tamper`, [`__key`] is no longer the constant `K`. It is derived
//! once at start-up (see [`init_keying`], called from `main::harden`) from a
//! probe of the real environment -- `getpagesize()`, which is a fixed value on
//! the target but which an emulator that no-ops the syscall returns wrong. The
//! embedded `SALT = C_REAL ^ K` folds the probe with the true key, so
//! `probe() ^ SALT == K` **only** when `probe()` returns the real value:
//!
//! * on the device: `getpagesize() == C_REAL` -> key is `K` -> decodes correct,
//!   the result CSV (and the oracle) does not move;
//! * under an emulator that stubs `getpagesize` (returns 0/garbage), or that
//!   sweeps basic blocks without ever running start-up, the global key is wrong
//!   -> every decode yields garbage, and the harvested floats are both un-round
//!   and fail any cross-check against the observed CSV.
//!
//! Honest limits. This defeats the offline emulator and the binary-only
//! adversary; it does **not** beat a live root run + RAM dump (the decoded value
//! is resident then), and it is an arms race -- once the probe is identified an
//! attacker can patch the emulator to return `C_REAL`. Its strength is how hard
//! the probe is to spot and fake; `getpagesize` is a first, obvious probe and
//! should later be widened (several probes / a heavier real computation). A
//! value kept register-only (never stored) also stays out of a memory-write log
//! regardless of the key.

/// The obfuscation key. The compile-time half of the scheme: ciphertext is
/// `plaintext ^ K`. At run time the *decode* key is [`__key`], which is `K` in
/// ordinary builds and an environment-derived value under `anti-tamper`.
pub const K: u64 = 0x9E3779B97F4A7C15;

// -- run-time decode key ----------------------------------------------------

/// The key XORed against the ciphertext at each decode site.
///
/// Without `anti-tamper` this is just `K`, laundered through `black_box` so the
/// XOR cannot be constant-folded; it inlines to in-register arithmetic with no
/// call.
#[cfg(not(feature = "anti-tamper"))]
#[inline(always)]
pub fn __key() -> u64 {
    ::core::hint::black_box(K)
}

/// No-op in ordinary builds: the key is the constant `K`, nothing to derive.
#[cfg(not(feature = "anti-tamper"))]
#[inline(always)]
pub fn init_keying() {}

#[cfg(feature = "anti-tamper")]
pub use keying::{init as init_keying, key as __key};

#[cfg(feature = "anti-tamper")]
mod keying {
    use super::K;
    use core::hint::black_box;
    use core::sync::atomic::{AtomicU64, Ordering};

    /// The environment probe's value on the real target. Measured on the device
    /// (`getconf PAGESIZE`), not assumed: this Raspberry Pi uses 16 KiB pages.
    /// If the decode key comes out wrong the whole binary breaks, so this must
    /// match the deployment hardware exactly.
    const C_REAL: u64 = 16384;

    /// Folds the probe with the true key. Shipped in `.rodata`; on its own it is
    /// `K` XORed with a known small integer, useless without also running the
    /// probe on real hardware (which is exactly what an emulator cannot do).
    const SALT: u64 = C_REAL ^ K;

    /// Derived once by [`init`]; zero until then, so any decode that runs before
    /// start-up (e.g. an emulator sweeping a block in isolation) gets a wrong
    /// key.
    static RUNTIME_KEY: AtomicU64 = AtomicU64::new(0);

    extern "C" {
        fn getpagesize() -> i32;
    }

    /// Probe the environment and assemble the decode key. **Must run before any
    /// `encf!`/`enci!`.** `main::harden` calls it first thing.
    #[inline(never)]
    pub fn init() {
        let probe = unsafe { getpagesize() } as u32 as u64;
        RUNTIME_KEY.store(probe ^ SALT, Ordering::Release);
    }

    /// The decode key: `K` on real hardware once [`init`] has run, garbage under
    /// an emulator that stubs the probe or never runs start-up.
    #[inline(always)]
    pub fn key() -> u64 {
        black_box(RUNTIME_KEY.load(Ordering::Acquire))
    }
}

// -- macros -----------------------------------------------------------------

/// An `obfstr!` for diagnostic strings that a `dist` build drops entirely.
///
/// A successful `--replay` run (the oracle) never reaches an error message, so
/// error/usage/version text is pure attack surface: string analysis and live
/// memory scans of the reverse-engineering reports leaned on exactly these. In
/// dev/release (`introspection` on) this is `obfstr!`, so messages still help;
/// in a `dist` build (`introspection` off, like the settings-name table) it
/// expands to an **empty `&str`**, so the literal never enters the binary at all.
///
/// Only for text absent from the oracle's output. Strings that reach the CSV
/// (the gate `reason` labels), the result summary, or the header must stay
/// `obfstr!` -- blanking them would move the digest.
#[cfg(feature = "introspection")]
#[macro_export]
macro_rules! obfstr_err {
    ($s:literal) => {
        ::obfstr::obfstr!($s)
    };
}

/// `dist` variant: the message is compiled out to an empty string.
#[cfg(not(feature = "introspection"))]
#[macro_export]
macro_rules! obfstr_err {
    ($s:literal) => {
        ""
    };
}

/// Encrypt an `f64` literal at compile time; decode it **inline** at run time.
///
/// `encf!(1.35)` reads as the value in source but ships only ciphertext. The
/// decode is inline (no `bl` to a shared decoder), and the key (see [`__key`])
/// passes through `black_box`, so the XOR cannot be constant-folded to plaintext.
#[macro_export]
macro_rules! encf {
    ($v:expr) => {{
        const ENC: u64 = ($v as f64).to_bits() ^ $crate::crypt::K;
        f64::from_bits(ENC ^ $crate::crypt::__key())
    }};
}

/// Encrypt an `i64` literal at compile time; decode it **inline** at run time.
///
/// Same barrier as [`encf!`]: inline XOR against the run-time key, no shared
/// decoder function.
#[macro_export]
macro_rules! enci {
    ($v:expr) => {{
        const ENC: u64 = ($v as i64 as u64) ^ $crate::crypt::K;
        (ENC ^ $crate::crypt::__key()) as i64
    }};
}
