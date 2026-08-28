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
//! once at start-up (see [`init_keying`], called from `main::harden`) from two
//! things: a probe of the real environment -- `getpagesize()`, a fixed value on
//! the target but wrong under an emulator that no-ops the syscall -- **and a hash
//! of this process's own `.text`**. The build tool patches an embedded salt to
//! `SALT2 = C_REAL ^ text_hash ^ K`, so `probe() ^ text_hash() ^ SALT2 == K`
//! **only** on the genuine hardware running the *unpatched* code:
//!
//! * patch any code byte (e.g. to splice in an argument logger, the way an
//!   LD_PRELOAD shim would from outside) and the `.text` hash moves, the key is
//!   wrong, and every constant/string decodes to garbage -- there is no compare
//!   to NOP, because the key *is* a function of the code bytes;
//!
//! and, from the environment probe alone:
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

    /// Placeholder for the code-integrity salt. `tools/patch_integrity.py`
    /// overwrites the shipped binary's copy of this word (found by this sentinel)
    /// with `C_REAL ^ text_hash() ^ K`, computed over the *final* `.text`. So at
    /// run time `probe ^ text_hash() ^ SALT2 == K` holds **only** on the genuine
    /// hardware running the *unpatched* code. Kept in `.data` (interior-mutable
    /// atomic, so it is a real memory load, never an immediate baked into `.text`)
    /// -- patching it therefore does not change the hash it feeds.
    ///
    /// If the build's patch step never ran, this sentinel stays in place and every
    /// decode yields garbage. That is the intended safe failure: a binary that was
    /// not integrity-sealed does not silently ship working.
    const SALT2_SENTINEL: u64 = 0xA1B2_C3D4_E5F6_0718;
    // `#[used]` + a *volatile* read below keep this as a real 8-byte word in
    // writable `.data` -- so the offline tool can find and patch it, and the
    // optimiser cannot fold the read-only atomic back into an immediate baked
    // into `.text` (which would leave nothing to patch and no memory load).
    #[used]
    static SALT2: AtomicU64 = AtomicU64::new(SALT2_SENTINEL);

    /// Derived once by [`init`]; zero until then, so any decode that runs before
    /// start-up (e.g. an emulator sweeping a block in isolation) gets a wrong
    /// key.
    static RUNTIME_KEY: AtomicU64 = AtomicU64::new(0);

    extern "C" {
        fn getpagesize() -> i32;
        fn getauxval(kind: u64) -> u64;
    }

    // Just enough of the ELF program-header ABI to find our own code segment.
    const AT_PHDR: u64 = 3;
    const AT_PHNUM: u64 = 5;
    const PT_LOAD: u32 = 1;
    const PT_PHDR: u32 = 6;
    const PF_X: u32 = 1;

    #[repr(C)]
    struct Phdr {
        p_type: u32,
        p_flags: u32,
        p_offset: u64,
        p_vaddr: u64,
        p_paddr: u64,
        p_filesz: u64,
        p_memsz: u64,
        p_align: u64,
    }

    /// FNV-1a over a byte slice. Cheap (a few ms over a multi-MB `.text`), and
    /// trivially reproducible by the offline patch tool so both sides agree.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Hash this process's own executable segment (`.text`), located exactly via
    /// the auxiliary vector so the byte range matches what the file-side tool
    /// hashes. Returns 0 if the layout cannot be read -- which makes the key wrong
    /// and the binary fail closed, never open.
    fn text_hash() -> u64 {
        unsafe {
            let phdr_addr = getauxval(AT_PHDR);
            let phnum = getauxval(AT_PHNUM) as usize;
            if phdr_addr == 0 || phnum == 0 {
                return 0;
            }
            let phdrs = phdr_addr as *const Phdr;
            // Load bias = (phdrs in memory) - (their vaddr as recorded in PT_PHDR).
            let mut bias: u64 = 0;
            let mut have_bias = false;
            for i in 0..phnum {
                let p = &*phdrs.add(i);
                if p.p_type == PT_PHDR {
                    bias = phdr_addr.wrapping_sub(p.p_vaddr);
                    have_bias = true;
                    break;
                }
            }
            if !have_bias {
                return 0;
            }
            for i in 0..phnum {
                let p = &*phdrs.add(i);
                if p.p_type == PT_LOAD && (p.p_flags & PF_X) != 0 {
                    let start = bias.wrapping_add(p.p_vaddr) as *const u8;
                    let bytes = core::slice::from_raw_parts(start, p.p_filesz as usize);
                    return fnv1a(bytes);
                }
            }
            0
        }
    }

    /// Probe the environment, hash our own code, and assemble the decode key.
    /// **Must run before any `encf!`/`enci!`.** `main::harden` calls it first.
    #[inline(never)]
    pub fn init() {
        let probe = unsafe { getpagesize() } as u32 as u64;
        let th = text_hash();
        // Volatile read of the raw storage: the tool patches these bytes on disk
        // directly, and volatile forbids the compiler from assuming the value.
        let salt2 = unsafe { core::ptr::read_volatile(SALT2.as_ptr()) };
        RUNTIME_KEY.store(probe ^ th ^ salt2, Ordering::Release);
    }

    /// The decode key: `K` on real hardware running unpatched code once [`init`]
    /// has run; garbage under an emulator that stubs the probe, if the code was
    /// patched, or before start-up.
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
