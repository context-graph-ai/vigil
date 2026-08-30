//! A declared setting nobody has given a value renders as absent, in a form an
//! operator can tell apart from a value that IS empty.
//!
//! Two path settings arrive with no value on an ordinary install: the detector
//! model path and the recognition weights directory. Leaving them off the
//! operator surface entirely makes a real setting invisible, which is the
//! hidden knob the whole model refuses; rendering them with an empty `value=`
//! makes "nobody has set this" and "somebody set this to nothing" read
//! identically, and those two mean different things — the first is Automatic,
//! the second is a choice. So absence gets its own rendered form.
//!
//! Why these are unfakeable: the rendering is read from the real operator
//! answer the `vigil settings` surface produces, and the empty-value half is
//! written through the real store first, so a surface that rendered one form
//! for both states fails on the comparison rather than on wording.

use vigil::settings_model::{
    Author, ControlState, DETECTOR_MODEL_PATH_SETTING, RECOGNITION_WEIGHTS_DIR_SETTING, Scope,
    SettingValue, Surface,
};
use vigil::settings_projection::{
    AUTHOR_KEY, CONTROL_STATE_KEY, NAME_KEY, NONE, SETTING_LINE_PREFIX, VALUE_KEY,
};
use vigil::settings_store::SettingsStore;

/// The rendered form absence takes. Spelled here because production has no
/// constant for it yet; the implementation declares it beside the other
/// projection keys and this test then reads that spelling from one place.
const ABSENT: &str = "absent";

fn token_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn setting_named<'a>(rendered: &'a str, name: &str) -> Option<&'a str> {
    rendered
        .lines()
        .filter(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} ")))
        .find(|line| token_field(line, NAME_KEY) == Some(name))
}

fn line_for<'a>(rendered: &'a str, name: &str) -> &'a str {
    setting_named(rendered, name).unwrap_or_else(|| {
        panic!(
            "{name} is a declared setting an operator can choose, so the operator surface answers \
             for it — a setting missing from the listing reads as nothing at all; got:\n{rendered}"
        )
    })
}

#[test]
fn a_declared_path_setting_with_no_value_renders_as_absent() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let rendered = vigil::settings_command::answer(
        deployment.path(),
        &SettingsStore::store_path(deployment.path()),
        "",
    );

    for setting in [DETECTOR_MODEL_PATH_SETTING, RECOGNITION_WEIGHTS_DIR_SETTING] {
        let line = line_for(&rendered, setting);
        assert_eq!(
            token_field(line, VALUE_KEY),
            Some(ABSENT),
            "nobody has given {setting} a value, and the surface says so in its own form rather \
             than with a blank a reader has to interpret; got: {line}"
        );
        assert_ne!(
            token_field(line, VALUE_KEY),
            Some(NONE),
            "absence of a VALUE is not the same fact as a field with nothing to report, and the \
             two must not share a spelling; got: {line}"
        );
    }
}

#[test]
fn an_absent_value_and_an_explicitly_empty_value_never_render_the_same() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");

    let absent_rendering = vigil::settings_command::answer(
        deployment.path(),
        &SettingsStore::store_path(deployment.path()),
        "",
    );
    let absent_line = line_for(&absent_rendering, RECOGNITION_WEIGHTS_DIR_SETTING);
    let absent_value = token_field(absent_line, VALUE_KEY)
        .unwrap_or_else(|| panic!("every setting line carries a value field; got: {absent_line}"));

    let store = SettingsStore::open(deployment.path()).expect("open the node-side settings store");
    let node = deployment
        .path()
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .expect("the deployment directory has a name");
    store
        .set_local(
            RECOGNITION_WEIGHTS_DIR_SETTING,
            Surface::VigilSettings,
            Scope::node(node),
            SettingValue::text(""),
        )
        .expect("an operator may deliberately set a path setting to nothing at all");
    drop(store);

    let empty_rendering = vigil::settings_command::answer(
        deployment.path(),
        &SettingsStore::store_path(deployment.path()),
        "",
    );
    let empty_line = line_for(&empty_rendering, RECOGNITION_WEIGHTS_DIR_SETTING);
    let empty_value = token_field(empty_line, VALUE_KEY)
        .unwrap_or_else(|| panic!("every setting line carries a value field; got: {empty_line}"));

    assert!(
        !empty_value.is_empty(),
        "a value field is always a whitespace-free token, so a value that is empty is rendered as \
         something rather than as a gap the line reader walks past; got: {empty_line}"
    );
    assert_ne!(
        empty_value, absent_value,
        "somebody setting this to nothing is a choice, and nobody having set it is not — the two \
         must be distinguishable on the face of the surface; absent rendered as {absent_value:?} \
         and empty rendered the same way"
    );
}

#[test]
fn an_absent_value_reads_automatic_and_names_nobody_as_its_author() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let rendered = vigil::settings_command::answer(
        deployment.path(),
        &SettingsStore::store_path(deployment.path()),
        "",
    );
    let line = line_for(&rendered, DETECTOR_MODEL_PATH_SETTING);

    assert_eq!(
        token_field(line, CONTROL_STATE_KEY),
        Some(ControlState::Automatic.token().as_str()),
        "a setting nobody has touched reads Automatic: rendering absence must not invent a control \
         state, and must not read as a choice somebody made; got: {line}"
    );
    assert_eq!(
        token_field(line, AUTHOR_KEY),
        Some(Author::Automatic.as_str()),
        "and it is attributed to Vigil's own floor rather than to a person or a management \
         server; got: {line}"
    );
}
