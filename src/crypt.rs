//! Re-export shim for the standalone [`crypt`](../../crates/crypt) crate.
//!
//! The constant/string obfuscation and the self-integrity decode key used to live
//! here; they now live in the in-repo `crypt` crate so any project can reuse them.
//! This module keeps the old `kerbside::crypt::…` paths working (`init_keying`,
//! `__key`, `K`); the macros (`encf!`, `enci!`, `obfstr_err!`) are re-exported at
//! the kerbside crate root in `lib.rs`, so `crate::encf!` etc. resolve unchanged.
//!
//! kerbside's `anti-tamper` feature forwards to `crypt/self-integrity` (the
//! code-bound key) and `crypt/redact` (message erasure); see `Cargo.toml`.

pub use ::crypt::{init_keying, __key, K};
