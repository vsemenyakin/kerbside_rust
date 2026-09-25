//! The `self-integrity` decode key: `getpagesize() ^ text_hash(.text) ^ SALT2`.
//!
//! `SALT2` is patched post-build by the `seal` tool to `page_size ^ text_hash ^ K`
//! (see `src/bin/seal.rs`), so the key resolves to `K` only for the exact code on
//! the intended hardware. There is no reference page size compiled in: the runtime
//! uses the *live* `getpagesize()`, and the seal tool takes the target page size as
//! a parameter -- so one compiled binary can be sealed for several page sizes.
//!
//! **The assembled key is never stored.** An earlier version derived the whole
//! key once and kept it in a single `static RUNTIME_KEY`, written by one `stlr`.
//! That is exactly the chokepoint the round-11 live-debugger attack broke on: a
//! hardware breakpoint on that one instruction read the entire key out of a
//! register. Here [`init`] stores only the two runtime-derived *shares* (the page
//! probe and the `.text` hash) in separate words, and [`key`] recombines them with
//! the patched `SALT2` **inline at every decode site**. There is no single global
//! equal to the key and no single instruction that assembles it; a passive reader
//! must recover shares from more than one place (or reverse a decode site under
//! MBA) rather than snapshot one `stlr`. The returned value is bit-identical to the
//! old `RUNTIME_KEY`, so behaviour (and the output oracle) is unchanged.

use super::{fnv1a, SALT2_SENTINEL};
use core::hint::black_box;
use core::sync::atomic::{AtomicU64, Ordering};

// `#[used]` + a *volatile* read below keep this a real 8-byte word in writable
// `.data` -- so the offline tool can find and patch it, and the optimiser cannot
// fold the read-only atomic into an immediate baked into `.text` (which would
// leave nothing to patch and no memory load).
#[used]
static SALT2: AtomicU64 = AtomicU64::new(SALT2_SENTINEL);

// The two runtime-derived key shares, stored *separately* so no word ever holds
// the assembled key. Zero until [`init`] runs: before start-up the recombination
// yields `SALT2` (= page ^ text_hash ^ K, not K), so an early decode -- an
// emulator sweeping a block in isolation -- still gets a wrong key and fails
// closed. `#[used]` keeps each a real word the store cannot be optimised into a
// single fused global.
#[used]
static PROBE_SHARE: AtomicU64 = AtomicU64::new(0);
#[used]
static TEXT_SHARE: AtomicU64 = AtomicU64::new(0);

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

/// Hash this process's own executable segment (`.text`), located exactly via the
/// auxiliary vector so the byte range matches what the file-side `seal` tool
/// hashes. Returns 0 if the layout cannot be read -- which makes the key wrong and
/// the binary fail closed, never open.
// @re-target-defense ignore-for="dev,mixed"
//   role: hashes the process's own .text so any code patch corrupts the decryption key
//   reveal: identified the .text self-hashing routine that feeds the decode key
//   reveal: described the mechanism (hashes own executable PT_LOAD segment via the auxv, so a patched or emulated .text yields a different hash and a wrong key)
//   reveal: demonstrated a WORKING BYPASS -- extracted the constants despite the self-hash (read decoded values from RAM, or reproduced the correct key, without altering .text)
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
/// **Must run before any `encf!`/`enci!`.**
#[inline(never)]
// @re-target-defense ignore-for="dev,mixed"
//   role: derives the constant-decryption key from page size, .text hash and a post-build salt -- binds decryption to unpatched code on real hardware
//   reveal: identified the runtime key-derivation routine that gates all constant decryption
//   reveal: described the mechanism (key = getpagesize XOR text_hash(.text) XOR SALT2; key is wrong under emulation, after any .text patch, or before init runs)
//   reveal: demonstrated a WORKING BYPASS -- recovered the true decrypted constants despite it (e.g. read RUNTIME_KEY or the decoded values from RAM at runtime WITHOUT patching .text)
pub fn init() {
    let probe = unsafe { getpagesize() } as u32 as u64;
    let th = text_hash();
    // Store the two shares separately -- never their XOR, and never combined with
    // SALT2. Neither stored word equals the decode key, so there is no single
    // assembled-key store for a hardware breakpoint to snapshot (the round-11
    // attack read the key from exactly such a store). SALT2 is not touched here:
    // it stays the patched word the `seal` tool wrote, and joins the shares only
    // transiently inside `key`, inline at each decode site.
    PROBE_SHARE.store(probe, Ordering::Release);
    TEXT_SHARE.store(th, Ordering::Release);
}

/// The decode key: `K` on real hardware running unpatched code once [`init`] has
/// run; garbage under an emulator, if the code was patched, or before start-up.
///
/// Recombines the shares with `SALT2` *here*, so with `#[inline(always)]` the
/// assembly happens at each decode site rather than in one place. The value is
/// bit-identical to the old single `RUNTIME_KEY` (`probe ^ text_hash ^ SALT2`).
#[inline(always)]
pub fn key() -> u64 {
    // Volatile reads of the raw storage: SALT2 is patched on disk by the tool, and
    // volatile forbids the compiler from assuming any of the three values or from
    // fusing the two shares into one hoisted global.
    let probe = unsafe { core::ptr::read_volatile(PROBE_SHARE.as_ptr()) };
    let th = unsafe { core::ptr::read_volatile(TEXT_SHARE.as_ptr()) };
    let salt2 = unsafe { core::ptr::read_volatile(SALT2.as_ptr()) };
    black_box(probe ^ th ^ salt2)
}
