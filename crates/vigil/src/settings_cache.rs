//! A non-authoritative cache of the values a start consumes before its store
//! can answer.
//!
//! The liveness surface binds before the store opens, deliberately, so a slow
//! open is never mistaken for a wedged add-on; and the store is opened WITH the
//! vision embedder the recognition pair builds, which is the same call rather
//! than a sequencing preference. Being consumed early is a fact about WHEN a
//! value is read, not about who chooses it — so the operator sets these through
//! `vigil settings` like any other value, and this cache is what carries the
//! store's answer across the start boundary.
//!
//! The STORE is the truth. This file records what the store last resolved, and
//! it loses to everything: a value a surface names on this start is what this
//! start uses, and the store's own answer replaces the cache the moment the
//! store opens. It is written only during store-backed operation — the same
//! contract the service-identity sidecar carries — so a run with no store
//! behind it never writes one.

use std::path::{Path, PathBuf};

use crate::settings_model::{
    Author, HEALTH_PORT_SETTING, RECOGNITION_SPACE_ID_SETTING, RECOGNITION_WEIGHTS_DIR_SETTING,
    REVIEW_PORT_SETTING, ScopeTarget, SettingValue,
};
use crate::settings_store::SettingsStore;

/// The settings this cache carries: the two ports the surfaces bind on, and the
/// weights directory and embedding space the store is opened with.
const CACHED_SETTINGS: [&str; 4] = [
    HEALTH_PORT_SETTING,
    REVIEW_PORT_SETTING,
    RECOGNITION_WEIGHTS_DIR_SETTING,
    RECOGNITION_SPACE_ID_SETTING,
];

/// Where the cache lives inside a deployment directory. One place fixes the
/// relationship so no caller has to assume a filename.
pub(crate) fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("startup-values")
}

/// What one setting was cached as, or nothing where nobody has chosen it.
fn cached_text(contents: &str, setting: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name == setting).then(|| value.to_string())
    })
}

/// Put the cached values onto the configuration this start is coming up with,
/// for every value no surface named on THIS start.
///
/// A value a surface names now is a value somebody is choosing now: it wins,
/// exactly as it did before anything was cached, so changing a port in the
/// config file still takes effect at the next start rather than the one after
/// it. The cache only fills the gap where nothing was said.
pub(crate) fn apply_cached_startup_values(config: &mut crate::config::RuntimeConfig) {
    let Ok(contents) = std::fs::read_to_string(cache_path(&config.data_dir)) else {
        return;
    };
    let unsaid = |setting: &str| -> Option<String> {
        if names_setting(config, setting) {
            return None;
        }
        cached_text(&contents, setting)
    };
    let health_port = unsaid(HEALTH_PORT_SETTING);
    let review_port = unsaid(REVIEW_PORT_SETTING);
    let weights_dir = unsaid(RECOGNITION_WEIGHTS_DIR_SETTING);
    let space_id = unsaid(RECOGNITION_SPACE_ID_SETTING);

    if let Some(port) = health_port.and_then(|value| value.parse().ok()) {
        config.health_port = port;
    }
    if let Some(port) = review_port.and_then(|value| value.parse().ok()) {
        config.review_port = port;
    }
    if let Some(weights_dir) = weights_dir {
        // Recognition is on because a weights directory was chosen, which is
        // the same thing a weights directory on a surface means.
        config.recognition.enabled = true;
        config.recognition.weights_dir = Some(PathBuf::from(weights_dir));
    }
    if let Some(space) = space_id {
        config.recognition.embedding_space_id = space;
    }
}

/// Whether an input surface named one setting on this start. Read off the
/// assertions each surface produced, so it is what the surface actually said
/// rather than a comparison against a value that could have arrived either way.
fn names_setting(config: &crate::config::RuntimeConfig, setting: &str) -> bool {
    [
        config.file_surface.as_ref(),
        config.startup_surface.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|assertions| {
        assertions
            .entries
            .iter()
            .any(|(name, _)| name.as_str() == setting)
    })
}

/// Write what the store resolves for these values right now.
///
/// Called wherever a store-backed resolution of them happens: as a start brings
/// them into force, and after an operator writes one through `vigil settings`.
/// Only a value somebody CHOSE is cached — where nobody has chosen, Vigil's own
/// value is what the next start would use anyway, and caching it would put a
/// number nobody asked for in front of a default that may change.
pub(crate) fn refresh(store: &SettingsStore, data_dir: &Path, target: &ScopeTarget) {
    let mut contents = String::new();
    for setting in CACHED_SETTINGS {
        let Ok(effective) = store.resolve(setting, target) else {
            continue;
        };
        if effective.author == Author::Automatic {
            continue;
        }
        if let SettingValue::Text(text) = &effective.requested
            && text.is_empty()
        {
            // A path set to nothing at all is a choice about the setting, not a
            // path to open a store with; carrying it here would turn it into
            // one at the next start.
            continue;
        }
        contents.push_str(setting);
        contents.push('=');
        contents.push_str(&effective.requested.to_string());
        contents.push('\n');
    }
    let path = cache_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == contents => return,
        // Nothing chosen and nothing cached: a run that wrote an empty file here
        // would be writing where there is nothing to say.
        Err(_) if contents.is_empty() => return,
        _ => {}
    }
    if let Err(error) = std::fs::write(&path, contents) {
        println!(
            "startup_value_cache_write_failed=true path={} error={error}",
            path.display()
        );
    }
}
