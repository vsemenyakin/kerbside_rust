//! The `self-integrity` decode key: `getpagesize() ^ text_hash(.text) ^ SALT2`.
//!
//! `SALT2` is patched post-build by the `seal` tool to `page_size ^ text_hash ^ K`
//! (see `src/bin/seal.rs`), so the key resolves to `K` only for the exact code on
//! the intended hardware. There is no reference page size compiled in: the runtime
//! uses the *live* `getpagesize()`, and the seal tool takes the target page size as
//! a parameter -- so one compiled binary can be sealed for several page sizes.

use super::{fnv1a, SALT2_SENTINEL};
use core::hint::black_box;
use core::sync::atomic::{AtomicU64, Ordering};

// `#[used]` + a *volatile* read below keep this a real 8-byte word in writable
// `.data` -- so the offline tool can find and patch it, and the optimiser cannot
// fold the read-only atomic into an immediate baked into `.text` (which would
// leave nothing to patch and no memory load).
#[used]
static SALT2: AtomicU64 = AtomicU64::new(SALT2_SENTINEL);

/// Derived once by [`init`]; zero until then, so a decode that runs before
/// start-up (an emulator sweeping a block in isolation) gets a wrong key.
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

/// Hash this process's own executable segment (`.text`), located exactly via the
/// auxiliary vector so the byte range matches what the file-side `seal` tool
/// hashes. Returns 0 if the layout cannot be read -- which makes the key wrong and
/// the binary fail closed, never open.
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
pub fn init() {
    let probe = unsafe { getpagesize() } as u32 as u64;
    let th = text_hash();
    // Volatile read of the raw storage: the tool patches these bytes on disk
    // directly, and volatile forbids the compiler from assuming the value.
    let salt2 = unsafe { core::ptr::read_volatile(SALT2.as_ptr()) };
    RUNTIME_KEY.store(probe ^ th ^ salt2, Ordering::Release);
}

/// The decode key: `K` on real hardware running unpatched code once [`init`] has
/// run; garbage under an emulator, if the code was patched, or before start-up.
#[inline(always)]
pub fn key() -> u64 {
    black_box(RUNTIME_KEY.load(Ordering::Acquire))
}
