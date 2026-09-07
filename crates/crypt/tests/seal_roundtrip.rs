//! End-to-end proof of the self-integrity seal, on the local machine.
//!
//! Builds the `sealed_probe` example with `--features self-integrity`, seals it
//! for the *local* page size (so it runs correctly here), and checks it prints the
//! secret. Then seals a second copy, flips one byte of its `.text`, and checks it
//! no longer prints the secret -- the whole mechanism, on whatever Linux box runs
//! `cargo test`. Skipped on non-Linux (self-integrity is a no-op there).
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

extern "C" {
    fn getpagesize() -> i32;
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

/// Build with `cargo ... --message-format=json` and pull out the artifact path of
/// the target called `name`.
fn build_and_locate(args: &[&str], name: &str) -> PathBuf {
    let out = Command::new(cargo())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(args)
        .arg("--message-format=json")
        .output()
        .expect("run cargo");
    assert!(out.status.success(), "cargo build failed: {}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if line.contains(&format!("\"name\":\"{name}\"")) {
            if let Some(i) = line.find("\"executable\":\"") {
                let rest = &line[i + "\"executable\":\"".len()..];
                if let Some(end) = rest.find('"') {
                    if end > 0 {
                        return PathBuf::from(&rest[..end]);
                    }
                }
            }
        }
    }
    panic!("could not locate built artifact '{name}' in cargo json output");
}

/// (file offset, size) of the executable PT_LOAD -- to pick a byte to patch.
fn exec_segment(d: &[u8]) -> (usize, usize) {
    let phoff = u64::from_le_bytes(d[0x20..0x28].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(d[0x36..0x38].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(d[0x38..0x3a].try_into().unwrap()) as usize;
    for i in 0..phnum {
        let b = phoff + i * phentsize;
        let p_type = u32::from_le_bytes(d[b..b + 4].try_into().unwrap());
        let p_flags = u32::from_le_bytes(d[b + 4..b + 8].try_into().unwrap());
        if p_type == 1 && (p_flags & 1) != 0 {
            let off = u64::from_le_bytes(d[b + 8..b + 16].try_into().unwrap()) as usize;
            let len = u64::from_le_bytes(d[b + 32..b + 40].try_into().unwrap()) as usize;
            return (off, len);
        }
    }
    panic!("no executable segment");
}

fn seal(seal_bin: &PathBuf, target: &PathBuf, page_size: i32) {
    let out = Command::new(seal_bin)
        .arg(target)
        .args(["--page-size", &page_size.to_string()])
        .output()
        .expect("run seal");
    assert!(out.status.success(), "seal failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn run_stdout(bin: &PathBuf) -> (bool, String) {
    let out = Command::new(bin).output().expect("run fixture");
    (out.status.success(), String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn seal_roundtrip_genuine_and_patched() {
    let probe = build_and_locate(
        &["build", "-p", "crypt", "--example", "sealed_probe", "--features", "self-integrity"],
        "sealed_probe",
    );
    let seal_bin = build_and_locate(&["build", "-p", "crypt", "--bin", "seal"], "seal");
    let page = unsafe { getpagesize() };

    let dir = std::env::temp_dir();
    let genuine = dir.join("crypt_seal_genuine");
    let patched = dir.join("crypt_seal_patched");
    std::fs::copy(&probe, &genuine).unwrap();
    std::fs::copy(&probe, &patched).unwrap();
    for f in [&genuine, &patched] {
        seal(&seal_bin, f, page);
    }

    // Genuine, sealed for this machine -> prints the secret.
    let (ok, out) = run_stdout(&genuine);
    assert!(ok && out == "1234567", "genuine sealed probe must print the secret, got ok={ok} out={out:?}");

    // Patch one byte inside .text AFTER sealing -> hash moves -> key wrong -> not the secret.
    let mut data = std::fs::read(&patched).unwrap();
    let (off, len) = exec_segment(&data);
    let victim = off + len / 2;
    data[victim] ^= 0xff;
    std::fs::write(&patched, &data).unwrap();
    let (ok2, out2) = run_stdout(&patched);
    assert!(!(ok2 && out2 == "1234567"),
            "patched probe must NOT print the secret (fail-closed), got ok={ok2} out={out2:?}");

    let _ = std::fs::remove_file(&genuine);
    let _ = std::fs::remove_file(&patched);
}
