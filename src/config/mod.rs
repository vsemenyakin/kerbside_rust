//! One frozen `Settings` object, built once at startup.
//!
//! The Python module this ports says: *port the mechanism, not just the
//! values*, and warns that one piece of it does not translate at all. This is
//! that piece, so it is worth being precise about what changed.
//!
//! The contract
//! ------------
//!
//! * Every setting lives on exactly one group struct hanging off [`Settings`].
//!   Reads look like `settings.background.VAR_THRESHOLD`.
//! * Nothing is assignable through a shared reference. The Python enforces this
//!   with `frozen=True` and a runtime exception; here `current()` hands out an
//!   `Arc<Settings>`, and Rust refuses the assignment at compile time. That is
//!   strictly stronger and costs nothing.
//! * A setting changes at runtime only via [`apply`], and only if it is
//!   declared in [`VOLATILE`]. That is the entire volatility contract -- one
//!   line per name.
//!
//! Reading it per frame
//! --------------------
//!
//! The pipeline pulls the settings object **exactly once per frame**, at
//! ingress, and passes it down by argument. Nothing deeper re-pulls.
//!
//! In Python that is what makes a frame internally consistent without a lock:
//! `apply()` rebinds one module global, and the interpreter lock makes the
//! rebind atomic, so a reader sees either the whole old object or the whole new
//! one.
//!
//! **There is no interpreter lock here.** The equivalent is an
//! [`arc_swap::ArcSwap`]: `apply` swaps in a new `Arc<Settings>`, and
//! `current()` loads one. A reader that holds its `Arc` for the duration of the
//! frame holds a snapshot that cannot change underneath it, exactly as before.
//! A reader that called `current()` repeatedly *would* be able to see a change
//! mid-frame -- which is the failure the Python's once-per-frame discipline
//! rules out by convention and this one rules out by ownership: the pipeline
//! takes the `Arc` at ingress and passes `&Settings` down, so nothing deeper
//! *can* re-pull.
//!
//! Construction-time reads are a separate category and are declared as such:
//! the morphology kernels, the working-frame geometry and the homography are
//! read once in the constructor because they define the coordinate space and
//! cannot change without rebuilding the stage. A component holding a
//! construction-time snapshot must never read a `VOLATILE` name; the settings
//! test enforces that.

#![allow(non_snake_case)]

use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;

mod background;
mod calibration;
mod enforcement;
mod model;
mod telemetry;
mod tracking;
mod video;

pub use background::BackgroundSettings;
pub use calibration::CalibrationSettings;
pub use enforcement::EnforcementSettings;
pub use model::ModelSettings;
pub use telemetry::TelemetrySettings;
pub use tracking::TrackingSettings;
pub use video::VideoSettings;

// --------------------------------------------------------------------------
// Reflection
// --------------------------------------------------------------------------

/// A setting's value, in the few shapes settings actually take.
///
/// The Python gets name-based access for free from dataclass introspection.
/// Rust has no such thing, so the [`settings_group!`] macro generates the same
/// access from the same declaration -- which keeps the property that matters:
/// **adding a setting is one line, and nothing else has to be edited.** A
/// hand-kept list of names is exactly the sort of thing that goes stale and
/// then lies to `--dump-settings`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Points(Vec<(f64, f64)>),
}

impl Value {
    /// Render as Python's `repr` would, so `--dump-settings` output can be
    /// diffed directly against the Python's.
    pub fn repr(&self) -> String {
        match self {
            Value::Int(v) => format!("{v}"),
            // `{:?}` on an f64 is Rust's shortest round-tripping form, which is
            // the same rule CPython's repr uses -- `2.0`, `0.35`, `-1.0`.
            Value::Float(v) => format!("{v:?}"),
            Value::Bool(v) => (if *v { "True" } else { "False" }).to_string(),
            Value::Str(v) => format!("'{v}'"),
            Value::Points(points) => {
                let inner: Vec<String> = points
                    .iter()
                    .map(|(x, y)| format!("({x:?}, {y:?})"))
                    .collect();
                format!("({})", inner.join(", "))
            }
        }
    }
}

/// Conversion between a field's Rust type and [`Value`].
pub trait SettingValue: Sized {
    fn to_value(&self) -> Value;
    fn from_value(value: &Value) -> Result<Self, String>;
}

macro_rules! impl_int_setting {
    ($($t:ty),*) => {
        $(
            impl SettingValue for $t {
                fn to_value(&self) -> Value {
                    Value::Int(*self as i64)
                }
                fn from_value(value: &Value) -> Result<Self, String> {
                    match value {
                        Value::Int(v) => <$t>::try_from(*v)
                            .map_err(|_| format!("{v} is out of range for {}", stringify!($t))),
                        // Deliberately not accepting a Float here. An integer
                        // setting given 2.5 is a mistake in the caller, and
                        // silently truncating it is how a "why is the kernel 2
                        // pixels" afternoon starts.
                        other => Err(format!("expected an integer, got {other:?}")),
                    }
                }
            }
        )*
    };
}
impl_int_setting!(i32, i64, u8, u32, usize);

impl SettingValue for f64 {
    fn to_value(&self) -> Value {
        Value::Float(*self)
    }
    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Float(v) => Ok(*v),
            // The other direction is fine: an integer is an exact float, and
            // `--limit 50` should not have to be written `50.0`.
            Value::Int(v) => Ok(*v as f64),
            other => Err(format!("expected a number, got {other:?}")),
        }
    }
}

impl SettingValue for bool {
    fn to_value(&self) -> Value {
        Value::Bool(*self)
    }
    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Bool(v) => Ok(*v),
            other => Err(format!("expected a boolean, got {other:?}")),
        }
    }
}

impl SettingValue for String {
    fn to_value(&self) -> Value {
        Value::Str(self.clone())
    }
    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Str(v) => Ok(v.clone()),
            other => Err(format!("expected a string, got {other:?}")),
        }
    }
}

impl SettingValue for Vec<(f64, f64)> {
    fn to_value(&self) -> Value {
        Value::Points(self.clone())
    }
    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Points(v) => Ok(v.clone()),
            other => Err(format!("expected a list of points, got {other:?}")),
        }
    }
}

/// Name-based access to a settings group, generated by [`settings_group!`].
pub trait Group: Default {
    fn field_names() -> &'static [&'static str];
    fn get(&self, name: &str) -> Option<Value>;
    fn set(&mut self, name: &str, value: &Value) -> Result<(), String>;
}

/// Declare a settings group: its fields, their types, and their defaults.
///
/// Generates the struct, its `Default`, and the name-based access the dump and
/// override machinery need.
#[macro_export]
macro_rules! settings_group {
    (
        $(#[$meta:meta])*
        pub struct $name:ident {
            $(
                $(#[$fmeta:meta])*
                $field:ident : $ty:ty = $default:expr
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq)]
        #[allow(non_snake_case)]
        pub struct $name {
            $( $(#[$fmeta])* pub $field: $ty, )*
        }

        impl Default for $name {
            fn default() -> Self {
                Self { $( $field: $default, )* }
            }
        }

        impl $crate::config::Group for $name {
            fn field_names() -> &'static [&'static str] {
                &[ $( stringify!($field), )* ]
            }

            fn get(&self, name: &str) -> Option<$crate::config::Value> {
                match name {
                    $( stringify!($field) => {
                        Some($crate::config::SettingValue::to_value(&self.$field))
                    } )*
                    _ => None,
                }
            }

            fn set(&mut self, name: &str, value: &$crate::config::Value)
                -> Result<(), String>
            {
                match name {
                    $( stringify!($field) => {
                        self.$field = <$ty as $crate::config::SettingValue>::from_value(value)
                            .map_err(|e| format!("{}: {}", stringify!($field), e))?;
                        Ok(())
                    } )*
                    _ => Err(format!("unknown setting {:?}", name)),
                }
            }
        }
    };
}

// --------------------------------------------------------------------------
// The settings object
// --------------------------------------------------------------------------

/// The whole configuration of the process.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    pub video: VideoSettings,
    pub background: BackgroundSettings,
    pub model: ModelSettings,
    pub tracking: TrackingSettings,
    pub calibration: CalibrationSettings,
    pub enforcement: EnforcementSettings,
    pub telemetry: TelemetrySettings,
}

pub const GROUP_NAMES: [&str; 7] = [
    "video",
    "background",
    "model",
    "tracking",
    "calibration",
    "enforcement",
    "telemetry",
];

/// Dispatch a closure over the named group. One `match` rather than seven
/// copies of the same three lines in every accessor below.
macro_rules! with_group {
    ($settings:expr, $group:expr, |$g:ident| $body:expr) => {
        match $group {
            "video" => {
                let $g = &$settings.video;
                Some($body)
            }
            "background" => {
                let $g = &$settings.background;
                Some($body)
            }
            "model" => {
                let $g = &$settings.model;
                Some($body)
            }
            "tracking" => {
                let $g = &$settings.tracking;
                Some($body)
            }
            "calibration" => {
                let $g = &$settings.calibration;
                Some($body)
            }
            "enforcement" => {
                let $g = &$settings.enforcement;
                Some($body)
            }
            "telemetry" => {
                let $g = &$settings.telemetry;
                Some($body)
            }
            _ => None,
        }
    };
}

impl Settings {
    /// Every field name in a group, in declaration order.
    pub fn field_names_of(group: &str) -> Option<&'static [&'static str]> {
        match group {
            "video" => Some(VideoSettings::field_names()),
            "background" => Some(BackgroundSettings::field_names()),
            "model" => Some(ModelSettings::field_names()),
            "tracking" => Some(TrackingSettings::field_names()),
            "calibration" => Some(CalibrationSettings::field_names()),
            "enforcement" => Some(EnforcementSettings::field_names()),
            "telemetry" => Some(TelemetrySettings::field_names()),
            _ => None,
        }
    }

    pub fn get_dotted(&self, dotted_name: &str) -> Option<Value> {
        let (group, leaf) = dotted_name.split_once('.')?;
        with_group!(self, group, |g| g.get(leaf))?
    }

    pub fn set_dotted(&mut self, dotted_name: &str, value: &Value) -> Result<(), String> {
        let (group, leaf) = dotted_name
            .split_once('.')
            .ok_or_else(|| format!("{dotted_name:?} is not a dotted setting name"))?;
        match group {
            "video" => self.video.set(leaf, value),
            "background" => self.background.set(leaf, value),
            "model" => self.model.set(leaf, value),
            "tracking" => self.tracking.set(leaf, value),
            "calibration" => self.calibration.set(leaf, value),
            "enforcement" => self.enforcement.set(leaf, value),
            "telemetry" => self.telemetry.set(leaf, value),
            _ => Err(format!("unknown settings group {group:?} in {dotted_name:?}")),
        }
        .map_err(|e| format!("{group}.{e}"))
    }
}

/// Map a bare setting name to its owning group, rejecting collisions.
///
/// Built once and cached. The Python does this at import time and raises on a
/// duplicate; the equivalent here is that [`group_of`] returns the first owner
/// and the settings test asserts there is never a second.
pub fn group_of(name: &str) -> Option<&'static str> {
    for group in GROUP_NAMES {
        if let Some(fields) = Settings::field_names_of(group) {
            if fields.contains(&name) {
                return Some(group);
            }
        }
    }
    None
}

/// Normalise a bare or dotted setting name to `group.NAME`.
pub fn dotted(name: &str) -> Result<String, String> {
    if let Some((group, leaf)) = name.split_once('.') {
        if !GROUP_NAMES.contains(&group) {
            return Err(format!("unknown settings group {group:?} in {name:?}"));
        }
        return match group_of(leaf) {
            Some(owner) if owner == group => Ok(name.to_string()),
            _ => Err(format!("unknown setting {name:?}")),
        };
    }
    match group_of(name) {
        Some(group) => Ok(format!("{group}.{name}")),
        None => Err(format!("unknown setting {name:?}")),
    }
}

// --------------------------------------------------------------------------
// Volatility contract
// --------------------------------------------------------------------------

/// The only names [`apply`] will change at runtime.
///
/// Making a setting runtime-variable is this one-line declaration and nothing
/// else -- every per-frame read already comes from a snapshot. Adding a name
/// here is a claim that no component caches it at construction time.
///
/// The speed limit is here because a variable-limit gantry really does change
/// it mid-shift, and that was the case that forced this whole mechanism to
/// exist rather than settings simply being constants.
pub const VOLATILE: [&str; 2] = ["enforcement.SPEED_LIMIT_KPH", "telemetry.MEASURE_STAGES"];

// --------------------------------------------------------------------------
// Named profiles
// --------------------------------------------------------------------------

/// Startup profiles, applied over the declared defaults. Unknown names raise
/// rather than being ignored, so a typo fails at startup.
pub fn override_profile(name: &str) -> Option<Vec<(&'static str, Value)>> {
    match name {
        // Deterministic: every frame processed, no dropping, no wall-clock branch.
        "replay" => Some(vec![
            ("telemetry.MEASURE_STAGES", Value::Bool(false)),
            ("telemetry.EVIDENCE_RECORD", Value::Bool(true)),
            ("telemetry.RING_FRAMES", Value::Int(240)),
        ]),
        // What the benchmark runs: full instrumentation, no overlay encoding.
        "bench" => Some(vec![
            ("telemetry.MEASURE_STAGES", Value::Bool(true)),
            ("telemetry.WRITE_OVERLAY", Value::Bool(false)),
            ("telemetry.RING_FRAMES", Value::Int(500)),
        ]),
        // Small and quick, for the test suite.
        "test" => Some(vec![
            ("video.SCENE_FRAMES", Value::Int(240)),
            ("telemetry.MEASURE_STAGES", Value::Bool(false)),
            // Both, because the ring depth is derived to cover the pre-trigger
            // window -- shrinking one without the other has no effect.
            ("enforcement.EVIDENCE_PRE_FRAMES", Value::Int(40)),
            ("telemetry.RING_FRAMES", Value::Int(60)),
        ]),
        _ => None,
    }
}

pub const PROFILE_NAMES: [&str; 3] = ["bench", "replay", "test"];

// --------------------------------------------------------------------------
// Build path (pure -- returns, never publishes)
// --------------------------------------------------------------------------

fn with_overrides(mut settings: Settings, overrides: &[(&str, Value)]) -> Result<Settings, String> {
    for (raw_name, value) in overrides {
        let name = dotted(raw_name)?;
        settings.set_dotted(&name, value)?;
    }
    Ok(settings)
}

/// Recompute fields that are functions of other fields.
///
/// Run last, after every override layer, so a derived field always agrees with
/// whatever its inputs ended up being.
fn derive(settings: Settings) -> Result<Settings, String> {
    let budget_ms = 1000.0 / settings.video.FPS as f64;
    let ring_frames = settings
        .telemetry
        .RING_FRAMES
        .max(settings.enforcement.EVIDENCE_PRE_FRAMES);
    with_overrides(
        settings,
        &[
            // The per-frame budget is the frame interval, by definition.
            ("telemetry.BUDGET_MS", Value::Float(budget_ms)),
            // Evidence retention must cover the pre-trigger window it promises.
            ("telemetry.RING_FRAMES", Value::Int(ring_frames)),
        ],
    )
}

/// Build the settings object. Pure: returns it, does not publish it.
///
/// Layer order, last wins:
///   1. declared defaults
///   2. the named profile, if any
///   3. explicit `overrides`
///   4. `headless`, which forces its consequences over both
///   5. derived fields
pub fn resolve_settings(
    profile: Option<&str>,
    overrides: &[(&str, Value)],
    headless: bool,
) -> Result<Settings, String> {
    let mut settings = Settings::default();
    if let Some(profile) = profile {
        let layer = override_profile(profile).ok_or_else(|| {
            format!(
                "unknown profile {profile:?}; known: {:?}",
                PROFILE_NAMES.as_slice()
            )
        })?;
        settings = with_overrides(settings, &layer)?;
    }
    settings = with_overrides(settings, overrides)?;
    if headless {
        settings = with_overrides(settings, &[("telemetry.WRITE_OVERLAY", Value::Bool(false))])?;
    }
    derive(settings)
}

// --------------------------------------------------------------------------
// Publication
// --------------------------------------------------------------------------

/// The live settings object.
///
/// `ArcSwap` rather than `RwLock`: readers must never block, and a reader must
/// never be able to observe a half-applied change. The Python gets both from
/// the interpreter lock making a global rebind atomic; this is the same
/// guarantee spelled out.
static CURRENT: LazyLock<ArcSwap<Settings>> = LazyLock::new(|| {
    ArcSwap::from_pointee(
        resolve_settings(None, &[], false).expect("the declared defaults must resolve"),
    )
});

/// The live settings object.
///
/// Called once per frame at pipeline ingress; the resulting `Arc` is held for
/// the whole frame and passed down by reference. Nothing deeper calls it.
pub fn current() -> Arc<Settings> {
    CURRENT.load_full()
}

/// Publish the startup settings. Called once, from `main`.
pub fn initialize(settings: Settings) -> Arc<Settings> {
    CURRENT.store(Arc::new(settings));
    CURRENT.load_full()
}

/// Change a `VOLATILE` setting at runtime.
///
/// Returns an error for anything not declared in [`VOLATILE`]. The store at the
/// end is a single atomic pointer swap, so a reader mid-frame always holds one
/// coherent object. There is no lock anywhere in this module.
pub fn apply(changes: &[(&str, Value)]) -> Result<Arc<Settings>, String> {
    let mut normalised: Vec<(&str, Value)> = Vec::with_capacity(changes.len());
    for (raw_name, value) in changes {
        let name = dotted(raw_name)?;
        if !VOLATILE.contains(&name.as_str()) {
            return Err(format!(
                "{name:?} is not runtime-variable. To make it so, add it to \
                 VOLATILE in config/mod.rs -- but first check that no component \
                 caches it at construction time."
            ));
        }
        // `dotted` returned an owned String; the override list borrows, so keep
        // the canonical form by leaking only when it differs from the input.
        let canonical: &str = if name == *raw_name {
            raw_name
        } else {
            Box::leak(name.into_boxed_str())
        };
        normalised.push((canonical, value.clone()));
    }

    let previous = CURRENT.load_full();
    let updated = with_overrides((*previous).clone(), &normalised)?;
    if updated == *previous {
        return Ok(previous);
    }
    CURRENT.store(Arc::new(updated));
    Ok(CURRENT.load_full())
}

/// Swap in modified settings for the duration of a scope. Tests and tools.
///
/// Unlike [`apply`] this ignores [`VOLATILE`] -- it exists so a test can pin
/// any setting -- and it always restores, including on unwind, because the
/// restore is in `Drop`.
pub struct TemporarySettings {
    previous: Arc<Settings>,
}

impl Drop for TemporarySettings {
    fn drop(&mut self) {
        CURRENT.store(self.previous.clone());
    }
}

pub fn temporarily(overrides: &[(&str, Value)]) -> Result<TemporarySettings, String> {
    let previous = CURRENT.load_full();
    let updated = derive(with_overrides((*previous).clone(), overrides)?)?;
    CURRENT.store(Arc::new(updated));
    Ok(TemporarySettings { previous })
}

// --------------------------------------------------------------------------
// Introspection (derived from the declarations -- never a hand-kept list)
// --------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Record {
    pub name: String,
    pub value: Value,
    pub is_default: bool,
    pub is_volatile: bool,
}

/// Walk every setting. Adding a setting requires no edit here.
pub fn iter_records(settings: &Settings) -> Vec<Record> {
    let defaults = Settings::default();
    let mut records = Vec::new();
    for group in GROUP_NAMES {
        let fields = match Settings::field_names_of(group) {
            Some(fields) => fields,
            None => continue,
        };
        for field in fields {
            let name = format!("{group}.{field}");
            let value = match settings.get_dotted(&name) {
                Some(value) => value,
                None => continue,
            };
            let is_default = defaults.get_dotted(&name).as_ref() == Some(&value);
            records.push(Record {
                is_volatile: VOLATILE.contains(&name.as_str()),
                is_default,
                name,
                value,
            });
        }
    }
    records
}

/// The startup dump. `*` marks anything moved off its declared default, `!`
/// marks a runtime-variable name.
pub fn format_dump(settings: &Settings) -> String {
    iter_records(settings)
        .iter()
        .map(|record| {
            let mark = if record.is_default { ' ' } else { '*' };
            let vol = if record.is_volatile { '!' } else { ' ' };
            format!("{mark}{vol} {:<44} {}", record.name, record.value.repr())
        })
        .collect::<Vec<_>>()
        .join("\n")
}
