//! Hardware-independent unit tests: constant round-trip, FNV-1a vectors, and the
//! key-folding algebra. These run under default features (the decode key is `K`
//! behind a `black_box`), so `encf!`/`enci!` decode to exactly the original value.

// These assert that encf!/enci! are the identity when the decode key is the
// static `K` -- i.e. without `self-integrity`. Under `self-integrity` the key is
// zero until a *sealed* binary runs `init_keying()`, so round-trip there is proven
// end-to-end by `seal_roundtrip.rs` instead, not in an unsealed test binary.
#[cfg(not(feature = "self-integrity"))]
#[test]
fn encf_roundtrip() {
    // encf!/enci! encrypt at compile time, so they take *literals* (const exprs).
    // A spread including edges and the kind of values calibration/tuning holds.
    assert_eq!(crypt::encf!(0.0), 0.0);
    assert_eq!(crypt::encf!(1.0), 1.0);
    assert_eq!(crypt::encf!(-1.0), -1.0);
    assert_eq!(crypt::encf!(0.5), 0.5);
    assert_eq!(crypt::encf!(-0.5), -0.5);
    assert_eq!(crypt::encf!(1.35), 1.35);
    assert_eq!(crypt::encf!(9.81), 9.81);
    assert_eq!(crypt::encf!(1e-6), 1e-6);
    assert_eq!(crypt::encf!(1e9), 1e9);
    assert_eq!(crypt::encf!(-1e9), -1e9);
    assert_eq!(crypt::encf!(3.141592653589793), 3.141592653589793);
    assert_eq!(crypt::encf!(123456.789), 123456.789);
}

#[cfg(not(feature = "self-integrity"))]
#[test]
fn enci_roundtrip() {
    assert_eq!(crypt::enci!(0), 0);
    assert_eq!(crypt::enci!(1), 1);
    assert_eq!(crypt::enci!(-1), -1);
    assert_eq!(crypt::enci!(42), 42);
    assert_eq!(crypt::enci!(-42), -42);
    assert_eq!(crypt::enci!(i64::MIN), i64::MIN);
    assert_eq!(crypt::enci!(i64::MAX), i64::MAX);
    assert_eq!(crypt::enci!(1234567), 1234567);
    assert_eq!(crypt::enci!(-987654321), -987654321);
}

#[test]
fn fnv1a_known_vectors() {
    // Canonical FNV-1a-64 test vectors.
    assert_eq!(crypt::fnv1a(b""), 0xcbf29ce484222325);
    assert_eq!(crypt::fnv1a(b"a"), 0xaf63dc4c8601ec8c);
    assert_eq!(crypt::fnv1a(b"foobar"), 0x85944171f73967e8);
}

/// The runtime derives `key = probe ^ text_hash ^ SALT2`; the seal tool sets
/// `SALT2 = page ^ text_hash ^ K`. So on genuine hardware/code the key is `K`.
fn derive(probe: u64, text_hash: u64, salt2: u64) -> u64 {
    probe ^ text_hash ^ salt2
}
fn seal(page: u64, text_hash: u64) -> u64 {
    page ^ text_hash ^ crypt::K
}

#[test]
fn key_algebra_genuine() {
    // Genuine: probe == page and the hash matches what was sealed -> key is K.
    for &(page, h) in &[(4096u64, 0xdeadbeefu64), (16384, 0x0), (4096, u64::MAX)] {
        let salt2 = seal(page, h);
        assert_eq!(derive(page, h, salt2), crypt::K, "genuine key must be K");
    }
}

#[test]
fn key_algebra_patched_code_breaks() {
    // Patched code: the running text_hash differs from the sealed one -> key != K.
    let page = 4096;
    let sealed_hash = 0x1111_2222_3333_4444;
    let salt2 = seal(page, sealed_hash);
    let running_hash = sealed_hash ^ 1; // one bit flipped, as a patched byte would
    assert_ne!(derive(page, running_hash, salt2), crypt::K, "patched key must not be K");
}

#[test]
fn key_algebra_wrong_probe_breaks() {
    // Emulator: getpagesize returns the wrong value -> key != K.
    let h = 0xabcd;
    let salt2 = seal(16384, h);
    assert_ne!(derive(0, h, salt2), crypt::K, "stubbed-probe key must not be K");
}
