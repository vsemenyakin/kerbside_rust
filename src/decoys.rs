//! Decoy sealed constants (feature `anti-tamper`, shipped build only).
//!
//! The real tuning/calibration constants are each a tiny getter that materialises
//! one `encf!`/`enci!` ciphertext. This module adds a pile of *fake* getters with
//! plausible values, folds them into a sink so they are not dead-code-eliminated,
//! and changes no output. They are indistinguishable from the real getters (same
//! per-site mixing, same crown obfuscation), so an attacker who has reversed the
//! mixing and recovers per-site values cannot tell which are the real thresholds.
//!
//! `seed_decoys` is called once at start-up from `main::harden`.

use core::hint::black_box;
use core::sync::atomic::{AtomicU64, Ordering};

// `#[used]` + the store below keep the whole computation live: to store `acc` the
// compiler must evaluate every getter, and each getter's value depends on the
// opaque run-time `__key()`, so none can be folded away -- every decoy ciphertext
// reaches `.text`.
#[used]
static DECOY_SINK: AtomicU64 = AtomicU64::new(0);

macro_rules! decoy_f {
    ($name:ident, $v:literal) => {
        #[inline(never)]
        fn $name() -> f64 {
            black_box(crate::encf!($v))
        }
    };
}
macro_rules! decoy_i {
    ($name:ident, $v:literal) => {
        #[inline(never)]
        fn $name() -> i64 {
            black_box(crate::enci!($v))
        }
    };
}

decoy_f!(df01, 0.47);  decoy_f!(df02, 0.28);  decoy_f!(df03, 0.55);  decoy_f!(df04, 0.71);
decoy_f!(df05, 0.18);  decoy_f!(df06, 0.63);  decoy_f!(df07, 2.4);   decoy_f!(df08, 3.7);
decoy_f!(df09, 5.2);   decoy_f!(df10, 8.9);   decoy_f!(df11, 11.3);  decoy_f!(df12, 0.09);
decoy_f!(df13, 0.44);  decoy_f!(df14, 1.9);   decoy_f!(df15, 0.83);  decoy_f!(df16, 15.5);
decoy_f!(df17, 22.0);  decoy_f!(df18, 0.37);  decoy_f!(df19, 0.66);  decoy_f!(df20, 0.13);

decoy_i!(di01, 313);   decoy_i!(di02, 7);     decoy_i!(di03, 18);    decoy_i!(di04, 45);
decoy_i!(di05, 260);   decoy_i!(di06, 9800);  decoy_i!(di07, 6);     decoy_i!(di08, 27);
decoy_i!(di09, 3);     decoy_i!(di10, 14);    decoy_i!(di11, 1200);  decoy_i!(di12, 33);
decoy_i!(di13, 8);     decoy_i!(di14, 16);    decoy_i!(di15, 55);    decoy_i!(di16, 4200);
decoy_i!(di17, 11);    decoy_i!(di18, 2);     decoy_i!(di19, 19);    decoy_i!(di20, 640);

/// Materialise the decoys. Behaviour-neutral: it only folds their values into a
/// sink and returns nothing.
pub fn seed_decoys() {
    let mut acc: u64 = 0;
    let fs = [
        df01(), df02(), df03(), df04(), df05(), df06(), df07(), df08(), df09(), df10(),
        df11(), df12(), df13(), df14(), df15(), df16(), df17(), df18(), df19(), df20(),
    ];
    for v in fs {
        acc ^= v.to_bits();
    }
    let is = [
        di01(), di02(), di03(), di04(), di05(), di06(), di07(), di08(), di09(), di10(),
        di11(), di12(), di13(), di14(), di15(), di16(), di17(), di18(), di19(), di20(),
    ];
    for v in is {
        acc ^= v as u64;
    }
    DECOY_SINK.store(black_box(acc), Ordering::Relaxed);
}
