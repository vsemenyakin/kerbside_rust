//! The settings contract.
//!
//! The Python's equivalent suite spends several tests proving things Rust
//! enforces at compile time -- that a settings object cannot be assigned
//! through, that a group cannot grow an untracked field. Those are noted where
//! they would have been, and the tests here cover what is still a *runtime*
//! promise: the volatility contract, the override layering, and the guarantee
//! that introspection is derived from the declarations rather than from a list
//! somebody has to remember to update.

use std::sync::{Mutex, MutexGuard};

use kerbside::config::{
    self, apply, dotted, format_dump, group_of, iter_records, resolve_settings, temporarily,
    Settings, Value, GROUP_NAMES, VOLATILE,
};

/// The published settings are process-global, and Rust runs the tests in one
/// binary concurrently. Anything that touches the global takes this first.
/// (The Python gets the same protection for free from the interpreter lock plus
/// pytest running in one thread.)
fn global() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// `nothing_is_assignable` and `every_group_uses_slots` have no runtime
// equivalent here. `current()` hands out an `Arc<Settings>`, so a caller has no
// `&mut` to assign through and the compiler rejects the attempt; and a Rust
// struct has no `__dict__` fallback to guard against. Both properties are
// stronger than the Python's and cost nothing to keep.

#[test]
fn unknown_names_raise_rather_than_being_ignored() {
    assert!(dotted("video.NO_SUCH_SETTING").is_err());
    assert!(dotted("no_such_group.FPS").is_err());
    assert!(dotted("NO_SUCH_SETTING").is_err());
    assert_eq!(dotted("FPS").unwrap(), "video.FPS");
    assert_eq!(dotted("video.FPS").unwrap(), "video.FPS");

    // An override with a typo in it fails the whole resolve rather than being
    // silently dropped -- which is the entire reason profiles are not
    // dictionaries of strings.
    assert!(resolve_settings(None, &[("video.FSP", Value::Int(25))], false).is_err());
}

#[test]
fn setting_names_are_unique_across_groups() {
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for group in GROUP_NAMES {
        for field in Settings::field_names_of(group).expect("every group resolves") {
            if let Some((other, owner)) = seen.iter().find(|(name, _)| name == field) {
                panic!("setting {other:?} is declared in both {owner:?} and {group:?}");
            }
            seen.push((field, group));
        }
    }
    for (field, group) in seen {
        assert_eq!(
            group_of(field),
            Some(group),
            "{field} resolved to the wrong group"
        );
    }
}

#[test]
fn derived_fields_follow_their_inputs() {
    let settings = resolve_settings(None, &[("video.FPS", Value::Int(25))], false).unwrap();
    assert_eq!(settings.telemetry.BUDGET_MS, 40.0);

    // The ring must cover the pre-trigger window it promises.
    let settings = resolve_settings(
        None,
        &[
            ("enforcement.EVIDENCE_PRE_FRAMES", Value::Int(900)),
            ("telemetry.RING_FRAMES", Value::Int(10)),
        ],
        false,
    )
    .unwrap();
    assert_eq!(settings.telemetry.RING_FRAMES, 900);
}

#[test]
fn override_layer_order() {
    // The profile applies, and an explicit override beats it.
    let settings = resolve_settings(
        Some("bench"),
        &[("telemetry.RING_FRAMES", Value::Int(11))],
        false,
    )
    .unwrap();
    assert!(settings.telemetry.MEASURE_STAGES, "the profile still applies");
    // 11 loses to the derived floor, which is EVIDENCE_PRE_FRAMES.
    assert_eq!(
        settings.telemetry.RING_FRAMES,
        settings.enforcement.EVIDENCE_PRE_FRAMES
    );

    let settings =
        resolve_settings(None, &[("telemetry.WRITE_OVERLAY", Value::Bool(true))], true).unwrap();
    assert!(!settings.telemetry.WRITE_OVERLAY, "headless must win");
}

#[test]
fn unknown_profiles_raise() {
    assert!(resolve_settings(Some("no-such-profile"), &[], false).is_err());
}

#[test]
fn apply_refuses_anything_not_declared_volatile() {
    let _guard = global();
    let error = apply(&[("background.VAR_THRESHOLD", Value::Float(8.0))])
        .expect_err("a non-volatile setting must be refused");
    assert!(
        error.contains("not runtime-variable"),
        "unhelpful message: {error}"
    );
    assert_eq!(config::current().background.VAR_THRESHOLD, 28.0);
}

#[test]
fn apply_rebinds_and_is_visible_to_the_next_reader() {
    let _guard = global();
    let before = config::current();
    let previous = before.enforcement.SPEED_LIMIT_KPH;

    apply(&[("enforcement.SPEED_LIMIT_KPH", Value::Float(70.0))]).unwrap();
    assert_eq!(config::current().enforcement.SPEED_LIMIT_KPH, 70.0);

    // The snapshot taken before the change is unaffected -- that is the whole
    // guarantee the pipeline's once-per-frame pull depends on.
    assert_eq!(before.enforcement.SPEED_LIMIT_KPH, previous);

    apply(&[("enforcement.SPEED_LIMIT_KPH", Value::Float(previous))]).unwrap();
}

#[test]
fn apply_is_a_noop_when_nothing_changes() {
    let _guard = global();
    let before = config::current();
    let limit = before.enforcement.SPEED_LIMIT_KPH;
    let after = apply(&[("enforcement.SPEED_LIMIT_KPH", Value::Float(limit))]).unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&before, &after),
        "an unchanged apply() published a new settings object"
    );
}

#[test]
fn temporarily_restores_even_on_unwind() {
    let _guard = global();
    let before = config::current().background.VAR_THRESHOLD;

    let result = std::panic::catch_unwind(|| {
        let _scope = temporarily(&[("background.VAR_THRESHOLD", Value::Float(8.0))]).unwrap();
        assert_eq!(config::current().background.VAR_THRESHOLD, 8.0);
        panic!("boom");
    });
    assert!(result.is_err(), "the panic should have propagated");
    assert_eq!(
        config::current().background.VAR_THRESHOLD,
        before,
        "temporarily() did not restore on unwind"
    );
}

#[test]
fn every_volatile_name_exists() {
    for name in VOLATILE {
        assert!(
            dotted(name).is_ok(),
            "{name} is declared VOLATILE but is not a setting"
        );
    }
}

#[test]
fn iter_records_covers_everything_and_marks_non_defaults() {
    let settings = resolve_settings(None, &[("video.FPS", Value::Int(30))], false).unwrap();
    let records = iter_records(&settings);

    let declared: usize = GROUP_NAMES
        .iter()
        .map(|g| Settings::field_names_of(g).unwrap().len())
        .sum();
    assert_eq!(records.len(), declared, "a setting is missing from the dump");

    let fps = records.iter().find(|r| r.name == "video.FPS").unwrap();
    assert!(!fps.is_default);
    let width = records
        .iter()
        .find(|r| r.name == "video.FRAME_WIDTH")
        .unwrap();
    assert!(width.is_default);

    let limit = records
        .iter()
        .find(|r| r.name == "enforcement.SPEED_LIMIT_KPH")
        .unwrap();
    assert!(limit.is_volatile);

    let dump = format_dump(&settings);
    assert!(dump.contains("* "), "nothing was marked as non-default");
    assert!(dump.contains('!'), "nothing was marked as volatile");
    assert_eq!(dump.lines().count(), declared);
}
