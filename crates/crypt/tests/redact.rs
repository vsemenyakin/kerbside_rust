//! `obfstr_err!` behaviour under the `redact` feature. Run both ways:
//!     cargo test -p crypt
//!     cargo test -p crypt --features redact

#[test]
fn obfstr_err_behaviour() {
    // Use the macro inline in the assertion: obfstr!'s result borrows a temporary
    // that lives only to the end of the statement, so it must not be `let`-bound.
    #[cfg(feature = "redact")]
    assert_eq!(crypt::obfstr_err!("a secret diagnostic message"), "");
    #[cfg(not(feature = "redact"))]
    assert_eq!(
        crypt::obfstr_err!("a secret diagnostic message"),
        "a secret diagnostic message"
    );
}
