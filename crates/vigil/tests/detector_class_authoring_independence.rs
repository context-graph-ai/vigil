//! Which classes Vigil looks for and which classes recognition puts a name to
//! are two settings a person sets separately.
//!
//! The store already holds them as two settings. What does not hold yet is the
//! surface an operator writes through: one field named for recognition is the
//! only author of the detector's class list, so a person who widens what
//! recognition covers silently widens what the machine detects, and a person
//! who wants to detect vehicles without recognizing anything has no field to
//! say it in. The promise is that changing either leaves the other's behavior
//! exactly as it was.
//!
//! Both directions are driven through the REAL production loader
//! (`vigil::surface_authoring_from_args` and
//! `vigil::recognition_covered_classes_from_args`, which call the same
//! `config::load` `vigil run` does), so what is asserted is what an operator's
//! own configuration file does — never a helper that could stay unwired from
//! the path a real install takes.

use std::ffi::OsString;
use std::fs;

use vigil::settings_model::{
    DETECTOR_CLASSES_SETTING, RECOGNITION_COVERED_CLASSES_SETTING, SettingValue,
};

fn args_for_config(path: &std::path::Path) -> Vec<OsString> {
    vec![OsString::from("--config"), path.as_os_str().to_owned()]
}

fn write_config(directory: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
    let path = directory.path().join("vigil.toml");
    fs::write(&path, body).expect("write the configuration file under test");
    path
}

/// Every setting the file surface asserts, flattened across the surfaces the
/// loader produced, so an assertion is read from where a record would be
/// authored rather than from a field on the resolved configuration.
fn authored_entries(path: &std::path::Path) -> Vec<(String, SettingValue)> {
    vigil::surface_authoring_from_args(args_for_config(path))
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

/// The classes recognition covers when nobody has named any — Vigil's own
/// deployment default, read from the seam that owns it
/// (`settings_backends::default_recognition_covered_classes`) rather than
/// copied here. This is NOT `RecognitionConfig::default()`'s own
/// `covered_classes` — that is the wider caller-facing engine baseline (every
/// COCO class the engine can recognize), not what a fresh Vigil install
/// covers by default; recognition coverage has its own independent default
/// (person and the household pet) precisely so that authoring the detector's
/// class list can never silently widen it. Reading the engine baseline here
/// would still exercise this test's independence assertion — an
/// implementation that let detector-class authoring leak into recognition
/// would fail it regardless of which baseline is expected — but asserting
/// against the wrong baseline is itself a false claim about what Vigil ships.
fn recognition_default_coverage() -> Vec<String> {
    vigil::settings_backends::default_recognition_covered_classes()
        .iter()
        .map(|class| (*class).to_string())
        .collect()
}

#[test]
fn naming_the_classes_to_detect_authors_the_detector_setting_and_leaves_recognition_alone() {
    // Unfakeable because both halves are asserted from one load of one file:
    // the detector's class list must arrive exactly as written, AND recognition
    // coverage must still be Vigil's own default. An implementation that kept
    // one field feeding both settings passes neither half — it would either
    // author nothing under the detector's own name or drag recognition
    // coverage along with it.
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = write_config(
        &directory,
        r#"
            detector_classes = ["car", "truck"]

            [[cameras]]
            name = "front gate"
            rtsp_url = "rtsp://camera.local:554/substream"
        "#,
    );

    let entries = authored_entries(&path);
    assert_eq!(
        authored_value(&entries, DETECTOR_CLASSES_SETTING),
        Some(SettingValue::list(["car", "truck"])),
        "the field a person names the classes to detect in must author the detector's own class \
         setting, got {entries:?}"
    );
    assert_eq!(
        authored_value(&entries, RECOGNITION_COVERED_CLASSES_SETTING),
        None,
        "and it must say nothing about what recognition covers, got {entries:?}"
    );
    assert_eq!(
        vigil::recognition_covered_classes_from_args(args_for_config(&path))
            .expect("the same file must load"),
        recognition_default_coverage(),
        "so recognition still covers what Vigil chose for it"
    );
}

#[test]
fn naming_the_classes_recognition_covers_authors_only_recognition_coverage() {
    // Unfakeable in the same shape, from the other side: the recognition field
    // must reach recognition's own setting AND must leave the detector's class
    // list unauthored, so the machine keeps detecting what it was detecting.
    // The value chosen is deliberately not Vigil's own default coverage, so a
    // surface that quietly answered from the default could not produce it.
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = write_config(
        &directory,
        r#"
            recognition_covered_classes = ["dog", "cat"]

            [[cameras]]
            name = "front gate"
            rtsp_url = "rtsp://camera.local:554/substream"
        "#,
    );

    let entries = authored_entries(&path);
    assert_eq!(
        authored_value(&entries, RECOGNITION_COVERED_CLASSES_SETTING),
        Some(SettingValue::list(["dog", "cat"])),
        "the recognition field must author recognition's own coverage setting, got {entries:?}"
    );
    assert_eq!(
        authored_value(&entries, DETECTOR_CLASSES_SETTING),
        None,
        "and must not author the detector's class list, or widening what gets a name put to it \
         silently widens what the machine looks for: got {entries:?}"
    );
    assert_ne!(
        recognition_default_coverage(),
        vec!["dog".to_string(), "cat".to_string()],
        "sanity: the value under test must differ from Vigil's own default coverage"
    );
    assert_eq!(
        vigil::recognition_covered_classes_from_args(args_for_config(&path))
            .expect("the same file must load"),
        vec!["dog".to_string(), "cat".to_string()],
        "and recognition covers exactly what was named"
    );
}
