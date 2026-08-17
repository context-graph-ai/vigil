//! The environment is not a settings surface. A variable naming a behavior
//! setting does nothing — and is never silently dropped: it is reported as
//! ignored, with the reason it did nothing and the place to set it instead.
//!
//! The place to set it instead is a STORE surface — the config file, the add-on
//! options, the startup options, or a `vigil settings` change. Pointing an
//! operator at a different environment variable would keep the environment
//! alive as a settings surface under another name, so that is checked
//! adversarially rather than assumed.
//!
//! Environment mutation is serialized within this test binary by `ENV_LOCK`,
//! held for the whole of each test, and every variable is restored by a guard.
//! Each integration test file is its own process, so the lock is sufficient:
//! no other test binary shares this environment.

use std::sync::{Mutex, MutexGuard};

use vigil::settings_environment::{IgnoredBehaviorVariable, ignored_behavior_variables};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// A behavior setting the roster moves into the store: the recognition
/// threshold. Naming it through the environment must do nothing and say so.
const BEHAVIOR_VARIABLE: &str = "VIGIL_RECOGNITION_THRESHOLD";

/// A second behavior variable, deliberately left UNSET, so the report is proven
/// to describe what is present right now rather than reciting a static roster.
const UNSET_BEHAVIOR_VARIABLE: &str = "VIGIL_DETECTOR_SAMPLE_FRAMES";

fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sets one variable for the duration of a test and restores whatever was there
/// before, so a test never leaves the environment altered for its neighbours.
struct EnvGuard {
    name: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    // Reading the variable is what makes restore-on-drop possible; the guard
    // exists to drive the env surface under test (allow: test env fixture).
    #[allow(clippy::disallowed_methods)]
    fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var(name).ok();
        // SAFETY: serialized by ENV_LOCK, which the caller holds for the whole
        // test; this binary mutates the environment nowhere else.
        unsafe { std::env::set_var(name, value) };
        Self { name, previous }
    }

    #[allow(clippy::disallowed_methods)] // as above: the guard's own read
    fn clear(name: &'static str) -> Self {
        let previous = std::env::var(name).ok();
        // SAFETY: as above.
        unsafe { std::env::remove_var(name) };
        Self { name, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: as above — the test still holds ENV_LOCK while it drops.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

fn reported(variable: &str) -> Option<IgnoredBehaviorVariable> {
    ignored_behavior_variables()
        .into_iter()
        .find(|entry| entry.variable == variable)
}

#[test]
fn an_environment_variable_naming_a_behavior_setting_is_reported_as_ignored_with_the_reason_and_where_to_set_it()
 {
    // Unfakeable: a second behavior variable is held UNSET across the same
    // call, so a report assembled from a fixed list rather than from the live
    // environment fails on the absence check even if it words the present
    // entry perfectly.
    let _lock = env_lock();
    let _present = EnvGuard::set(BEHAVIOR_VARIABLE, "0.42");
    let _absent = EnvGuard::clear(UNSET_BEHAVIOR_VARIABLE);

    let entry = reported(BEHAVIOR_VARIABLE).unwrap_or_else(|| {
        panic!(
            "an environment variable naming a behavior setting must be reported as ignored, never \
             silently dropped; {BEHAVIOR_VARIABLE} was set and the report did not mention it: \
             {:#?}",
            ignored_behavior_variables()
        )
    });

    assert!(
        !entry.setting.is_empty(),
        "the report names the setting the variable named, so the operator knows what they were \
         reaching for: {entry:#?}"
    );

    let reason = entry.reason.to_lowercase();
    assert!(
        reason.contains("environment"),
        "the reason must say plainly that the environment is not a settings surface; got {:?}",
        entry.reason
    );
    assert!(
        reason.contains("not a settings surface")
            || reason.contains("not a setting surface")
            || reason.contains("is not a settings")
            || reason.contains("holds no rank"),
        "the reason must state WHY it did nothing — the environment holds no authority for \
         behavior settings — rather than merely noting that it was ignored; got {:?}",
        entry.reason
    );

    assert!(
        !entry.set_it_here.is_empty(),
        "an operator who typed something deserves to be told where it belongs instead: {entry:#?}"
    );

    assert!(
        reported(UNSET_BEHAVIOR_VARIABLE).is_none(),
        "the report describes the variables present right now; {UNSET_BEHAVIOR_VARIABLE} is unset \
         and must not appear: {:#?}",
        ignored_behavior_variables()
    );
}

#[test]
fn the_report_names_the_store_surface_not_another_environment_variable() {
    // Unfakeable: the adversarial case is a report that redirects the operator
    // to a DIFFERENT environment variable, which would read as helpful and
    // would keep the environment alive as a settings surface. Any `VIGIL_`-
    // prefixed spelling in the redirection fails, and a real store surface must
    // be named positively — so neither an empty string nor a vague sentence
    // passes.
    let _lock = env_lock();
    let _present = EnvGuard::set(BEHAVIOR_VARIABLE, "0.42");

    let entry = reported(BEHAVIOR_VARIABLE)
        .unwrap_or_else(|| panic!("{BEHAVIOR_VARIABLE} must be reported as ignored"));

    assert!(
        !entry.set_it_here.contains("VIGIL_"),
        "the place to set it instead must be a store surface, never another environment variable; \
         got {:?}",
        entry.set_it_here
    );

    let destination = entry.set_it_here.to_lowercase();
    assert!(
        destination.contains("add-on options")
            || destination.contains("addon options")
            || destination.contains("config file")
            || destination.contains("configuration file")
            || destination.contains("startup options")
            || destination.contains("vigil settings"),
        "the redirection must name a real settings surface — the config file, the add-on options, \
         the startup options, or a vigil settings change — so it is actionable; got {:?}",
        entry.set_it_here
    );
}
