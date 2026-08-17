//! Three values a person can type into the add-on and the configuration file
//! actually reach the loader.
//!
//! The add-on schema declares the two stream-retry waits and the hardware-probe
//! deadline, and the operator surface answers for all three — so a Home
//! Assistant user opens the configuration page, sets one, and is told nothing
//! went wrong. Nothing did: the loader has no field for any of them, so the
//! value is read by nobody and the surface goes on reporting Vigil's own
//! choice. A knob that accepts a value and ignores it is the silently-ignored
//! input this model refuses; it is worse than a missing key, because a missing
//! key at least says so.
//!
//! Each value below is deliberately not Vigil's own default for that setting,
//! so a loader that quietly answered from the automatic floor could not produce
//! it by accident. Driven through the REAL production loader
//! (`vigil::surface_authoring_from_args`, which calls the same `config::load`
//! `vigil run` does), so what is asserted is what an operator's own file does.

use std::ffi::OsString;
use std::fs;

use vigil::settings_model::{
    HARDWARE_PROBE_DEADLINE_SECS_SETTING, RTSP_RETRY_INITIAL_MS_SETTING, RTSP_RETRY_MAX_MS_SETTING,
    SettingValue,
};

/// A camera entry, so the file under test is one a real deployment could run
/// rather than a fragment the loader might treat specially.
const CAMERA_ENTRY: &str = r#"
[[cameras]]
name = "front gate"
rtsp_url = "rtsp://camera.local:554/substream"
"#;

fn args_for_config(path: &std::path::Path) -> Vec<OsString> {
    vec![OsString::from("--config"), path.as_os_str().to_owned()]
}

/// Every setting the surfaces assert, read from where a record would be
/// authored rather than off a field of the resolved configuration.
fn authored_entries(directory: &tempfile::TempDir, body: &str) -> Vec<(String, SettingValue)> {
    let path = directory.path().join("vigil.toml");
    fs::write(&path, format!("{body}\n{CAMERA_ENTRY}"))
        .expect("write the configuration file under test");
    vigil::surface_authoring_from_args(args_for_config(&path))
        .expect("the configuration file must load through the real production path")
        .into_iter()
        .flat_map(|surface| surface.entries)
        .collect()
}

fn authored_value(entries: &[(String, SettingValue)], setting: &str) -> Option<SettingValue> {
    entries
        .iter()
        .find(|(name, _)| name == setting)
        .map(|(_, value)| value.clone())
}

/// Vigil's own choice for `setting`, read from the seam that owns it, so the
/// divergence check below cannot go stale against a changed default.
fn automatic_value(setting: &str) -> SettingValue {
    vigil::settings_backends::automatic_default(setting)
        .unwrap_or_else(|| panic!("`{setting}` must carry an automatic floor"))
        .0
}

/// Assert one knob: the file's value arrives under the setting's own name, and
/// it is a value the automatic floor could not have produced.
fn assert_file_value_authors_setting(field: &str, setting: &str, written: i64) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let entries = authored_entries(&directory, &format!("{field} = {written}"));
    assert_ne!(
        automatic_value(setting),
        SettingValue::Int(written),
        "sanity: the value under test must differ from Vigil's own choice for `{setting}`"
    );
    assert_eq!(
        authored_value(&entries, setting),
        Some(SettingValue::Int(written)),
        "a value typed into `{field}` must author `{setting}`, or the person who typed it is \
         watching a number that changes nothing: got {entries:?}"
    );
}

#[test]
fn the_first_stream_retry_wait_typed_into_a_file_authors_its_setting() {
    assert_file_value_authors_setting("rtsp_retry_initial_ms", RTSP_RETRY_INITIAL_MS_SETTING, 750);
}

#[test]
fn the_widest_stream_retry_wait_typed_into_a_file_authors_its_setting() {
    assert_file_value_authors_setting("rtsp_retry_max_ms", RTSP_RETRY_MAX_MS_SETTING, 45_000);
}

#[test]
fn the_hardware_probe_deadline_typed_into_a_file_authors_its_setting() {
    assert_file_value_authors_setting(
        "hardware_probe_deadline_secs",
        HARDWARE_PROBE_DEADLINE_SECS_SETTING,
        25,
    );
}

#[test]
fn all_three_set_together_author_three_distinct_settings() {
    // Unfakeable as a set: one file names all three at once with three
    // different numbers, so a loader that wired one field to the wrong setting,
    // or that let a later field overwrite an earlier one, returns the wrong
    // number for at least one of them — which the three tests above, each with
    // one field in the file, cannot see on their own.
    let directory = tempfile::tempdir().expect("temporary directory");
    let entries = authored_entries(
        &directory,
        "rtsp_retry_initial_ms = 750\nrtsp_retry_max_ms = 45000\n\
         hardware_probe_deadline_secs = 25",
    );
    for (setting, written) in [
        (RTSP_RETRY_INITIAL_MS_SETTING, 750),
        (RTSP_RETRY_MAX_MS_SETTING, 45_000),
        (HARDWARE_PROBE_DEADLINE_SECS_SETTING, 25),
    ] {
        assert_eq!(
            authored_value(&entries, setting),
            Some(SettingValue::Int(written)),
            "`{setting}` must carry the value the file gave it and no other: got {entries:?}"
        );
    }
}
