//! The two detection timers an operator can no longer be offered.
//!
//! Detection preparation is decided by its own outcome — it completes, or it
//! reports an error — so there is no waiting period governing it. Two knobs
//! used to: one bounding how long startup waited before settling for the
//! processor, and one bounding how long a preparation was allowed to keep
//! going. Neither decides anything now.
//!
//! A knob that decides nothing is worse than no knob: an operator whose
//! hardware is slow reads a surface offering to lengthen a wait, spends their
//! time on it, and nothing changes. So they are gone from every surface — the
//! add-on options a Home Assistant user browses, the help beside them, the
//! declared settings a deployment resolves, and above all the remedies Vigil
//! itself prints when a preparation fails.
//!
//! What replaces the remedy is the one thing that can change the outcome:
//! check that the graphics device is usable by this container, then ask for
//! the backend again.

use std::fs;
use std::path::{Path, PathBuf};

const ADDON_CONFIG_PATH: &str = "addons/vigil/config.yaml";
const ADDON_TRANSLATIONS_PATH: &str = "addons/vigil/translations/en.yaml";

/// The two timers, by the names every surface knew them under.
const REMOVED_TIMERS: [&str; 2] = [
    "detection_probe_deadline_secs",
    "detection_late_window_secs",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn read(relative: &str) -> Option<String> {
    fs::read_to_string(repo_root().join(relative)).ok()
}

/// Every `.rs` file under one directory, so a scan reads the shipped source
/// rather than a list of files kept beside it.
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}

#[test]
fn neither_detection_timer_is_offered_as_an_add_on_option() {
    let Some(text) = read(ADDON_CONFIG_PATH) else {
        return;
    };
    for timer in REMOVED_TIMERS {
        assert!(
            !text.contains(timer),
            "{timer} decides nothing, so the add-on manifest must not offer it — an option a \
             person can set and that changes no behavior is a surface telling them something \
             untrue"
        );
    }
}

#[test]
fn no_add_on_help_describes_a_detection_timer() {
    let Some(text) = read(ADDON_TRANSLATIONS_PATH) else {
        return;
    };
    for timer in REMOVED_TIMERS {
        assert!(
            !text.contains(timer),
            "the add-on help must not describe {timer}; help for a knob that governs nothing \
             sends an operator to spend their time on a value with no effect"
        );
    }
}

#[test]
fn a_deployment_declares_neither_detection_timer_as_a_setting() {
    // The declared roster is what a deployment resolves, reports and accepts
    // writes for. A timer left declared keeps answering on the settings
    // surface with a value and a reason, which reads as a live control.
    for timer in REMOVED_TIMERS {
        assert!(
            !vigil::settings_store::is_declared_setting(timer),
            "{timer} must no longer be a declared setting"
        );
        assert!(
            vigil::settings_backends::automatic_default(timer).is_none(),
            "{timer} must not resolve to a value with a reason attached; Vigil choosing a \
             default for it says it still governs something"
        );
    }
}

#[test]
fn nothing_vigil_prints_offers_to_lengthen_a_detection_wait() {
    // The remedies are the surface that matters most here: they are what an
    // operator reads at the exact moment their hardware did not work. Offering
    // to lengthen a wait at that moment is advice that cannot help — no wait
    // decided the outcome — and it is what sent the last operator to raise a
    // number instead of checking their device.
    let mut offenders: Vec<String> = Vec::new();
    for source in rust_sources(&repo_root().join("crates/vigil/src")) {
        let Ok(text) = fs::read_to_string(&source) else {
            continue;
        };
        let name = source
            .strip_prefix(repo_root())
            .unwrap_or(&source)
            .display()
            .to_string();
        for timer in REMOVED_TIMERS {
            if text.contains(timer) {
                offenders.push(format!("{name} names {timer}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "no shipped source may name a detection timer, in a remedy or anywhere else: {offenders:?}"
    );
}
