//! Seal a shipped binary's decode key to its own code (`.text`), in place.
//!
//!     seal <binary> --page-size <N>
//!
//! Computes `text_hash` over the final executable segment (the same FNV-1a and
//! byte range that `crypt::keying` hashes at run time) and patches the embedded
//! `SALT2` placeholder to `page_size ^ text_hash ^ K`, so at run time the key
//! resolves to `K` only for this exact code on hardware whose page size is `N`.
//! Patch any instruction byte afterwards and `text_hash` moves, the key is wrong,
//! and every `encf!`/`enci!` constant and `obfstr_err!` string decodes to garbage.
//!
//! Must run **last** in the build (after strip and any other post-processing),
//! because it hashes the final `.text`. It writes only the 8-byte `SALT2` word,
//! which lives in `.data` (not in the hashed segment), so the patch does not
//! disturb the hash it just computed. Size-preserving. Reads its constants and
//! hash from the `crypt` crate, so it can never drift from the runtime.

use std::process::ExitCode;

use crypt::{fnv1a, K, SALT2_SENTINEL};

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// (p_offset, p_filesz) of the executable PT_LOAD -- the range keying::text_hash
/// hashes at run time.
fn exec_segment(d: &[u8]) -> Result<(usize, usize), String> {
    if d.len() < 0x40 || &d[..4] != b"\x7fELF" || d[4] != 2 {
        return Err("not a 64-bit ELF".into());
    }
    if d[5] != 1 {
        return Err("not a little-endian ELF".into());
    }
    let e_phoff = u64le(d, 0x20) as usize;
    let e_phentsize = u16le(d, 0x36) as usize;
    let e_phnum = u16le(d, 0x38) as usize;
    for i in 0..e_phnum {
        let base = e_phoff + i * e_phentsize;
        if base + 40 > d.len() {
            break;
        }
        let p_type = u32::from_le_bytes(d[base..base + 4].try_into().unwrap());
        let p_flags = u32::from_le_bytes(d[base + 4..base + 8].try_into().unwrap());
        let p_offset = u64le(d, base + 8) as usize;
        let p_filesz = u64le(d, base + 32) as usize;
        if p_type == PT_LOAD && (p_flags & PF_X) != 0 {
            return Ok((p_offset, p_filesz));
        }
    }
    Err("no executable PT_LOAD segment found".into())
}

fn run() -> Result<(), String> {
    let mut path: Option<String> = None;
    let mut page_size: Option<u64> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--page-size" => {
                let v = args.next().ok_or("--page-size needs a value")?;
                page_size = Some(v.parse().map_err(|_| format!("bad --page-size: {v}"))?);
            }
            _ if path.is_none() => path = Some(a),
            _ => return Err(format!("unexpected argument: {a}")),
        }
    }
    let path = path.ok_or("usage: seal <binary> --page-size <N>")?;
    let page_size = page_size.ok_or("missing --page-size")?;

    let mut data = std::fs::read(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let size = data.len();

    let (off, len) = exec_segment(&data)?;
    if off + len > data.len() {
        return Err("executable segment truncated in file".into());
    }
    let th = fnv1a(&data[off..off + len]);
    let salt2 = page_size ^ th ^ K;

    let sentinel = SALT2_SENTINEL.to_le_bytes();
    let mut hits = Vec::new();
    let mut start = 0;
    while let Some(rel) = find(&data[start..], &sentinel) {
        hits.push(start + rel);
        start += rel + 8;
    }
    match hits.len() {
        0 => return Err("SALT2 sentinel not found -- already sealed, or self-integrity \
                         is not compiled in".into()),
        1 => {}
        n => return Err(format!("sentinel appears {n} times, expected 1")),
    }
    data[hits[0]..hits[0] + 8].copy_from_slice(&salt2.to_le_bytes());
    assert_eq!(data.len(), size, "seal must preserve file size");
    std::fs::write(&path, &data).map_err(|e| format!("cannot write {path}: {e}"))?;

    println!(
        "seal: text_hash={th:#018x} over {len} bytes at file {off:#x}; \
         page_size={page_size}; sealed SALT2={salt2:#018x} at {:#x}",
        hits[0]
    );
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("seal: {e}");
            ExitCode::FAILURE
        }
    }
}
