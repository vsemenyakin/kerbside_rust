//! Fixture for `tests/seal_roundtrip.rs`. Built with `--features self-integrity`,
//! sealed, and run: it must print the decoded constant only when the binary is
//! genuine and sealed for the running hardware. Patch a code byte and it prints
//! garbage instead.

const SECRET: i64 = 1234567;

fn main() {
    crypt::init_keying();
    println!("{}", crypt::enci!(SECRET));
}
