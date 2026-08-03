//! The camera configuration schema, driven through the REAL production
//! loader (`vigil::camera_source_summaries_from_args`, which calls
//! `config::load` exactly as `vigil run` does): one `[[cameras]]` entry
//! declares exactly one source kind; missing/conflicting source fields are
//! actionable errors naming the camera; USB/CSI identity is durable-
//! hardware-identity-keyed and `name` is display-only; MJPEG credentials
//! never leak into an error message; and adapter kinds are honestly
//! rejected as `unsupported_by_this_artifact` because no capture/encode
//! path for them exists yet — proven end to end, through the same `Result`
//! `vigil run` itself would get, not against a helper that could stay
//! permanently unwired from the real config-load path.

use std::ffi::OsString;
use std::fs;

use vigil::CameraSourceSummary;

fn args_for_config(path: &std::path::Path) -> Vec<OsString> {
    vec![OsString::from("--config"), path.as_os_str().to_owned()]
}

fn write_config(tmp: &tempfile::TempDir, toml: &str) -> std::path::PathBuf {
    let path = tmp.path().join("vigil.toml");
    fs::write(&path, toml).expect("write fixture config");
    path
}

#[test]
fn an_rtsp_only_camera_entry_resolves_and_loads_through_the_real_loader() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "front gate"
            rtsp_url = "rtsp://camera.local:554/substream"
            live_rtsp_url = "rtsp://camera.local:554/mainstream"
        "#,
    );

    let summaries = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect("an rtsp-only entry loads through the real production path");

    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0],
        CameraSourceSummary::Rtsp {
            rtsp_url: "rtsp://camera.local:554/substream".to_string(),
            live_rtsp_url: Some("rtsp://camera.local:554/mainstream".to_string()),
        }
    );
}

#[test]
fn a_camera_entry_declaring_no_source_field_fails_the_real_load_naming_the_camera_and_the_fix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "nameless"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect_err(
        "an entry with no source field must fail the real load, never be silently accepted",
    );

    assert!(
        error.contains("nameless"),
        "the real load error must name the camera, got: {error}"
    );
    assert!(
        error.to_ascii_lowercase().contains("rtsp_url")
            || error.to_ascii_lowercase().contains("source"),
        "the real load error must name the fix (which source field to add), got: {error}"
    );
}

#[test]
fn a_camera_entry_declaring_two_source_kinds_at_once_fails_the_real_load_as_a_conflict() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "conflicted-cam"
            rtsp_url = "rtsp://camera.local/substream"
            usb_device = "usb-1234:5678-serial-XYZ"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect_err(
        "declaring both rtsp_url and usb_device must fail the real load, never be guessed between",
    );

    assert!(
        error.contains("conflicted-cam"),
        "the real load conflict error must name the camera, got: {error}"
    );
}

#[test]
fn every_adapter_source_kind_is_honestly_rejected_by_the_real_load_as_unsupported_by_this_artifact()
{
    let tmp = tempfile::tempdir().expect("tempdir");

    for (fixture_field, camera_name, kind_str) in [
        (
            r#"usb_device = "usb-1234:5678-serial-ABC123""#,
            "workshop",
            "usb",
        ),
        (r#"csi_module = "csi-imx477-cam0""#, "porch", "csi"),
        (
            r#"mjpeg_url = "http://esp32cam.local/stream""#,
            "barn corner",
            "mjpeg",
        ),
    ] {
        let toml = format!(
            r#"
                [[cameras]]
                name = "{camera_name}"
                {fixture_field}
            "#
        );
        let path = write_config(&tmp, &toml);

        let error = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect_err(
            "an adapter-only entry must fail the real load: no USB/CSI/MJPEG capture/encode path exists yet",
        );

        assert!(
            error.contains("unsupported_by_this_artifact"),
            "the real load rejection must use the standard honest-capability-refusal vocabulary, got: {error}"
        );
        assert!(
            error.contains(camera_name),
            "the real load rejection must name the camera, got: {error}"
        );
        assert!(
            error.contains(kind_str),
            "the real load rejection must name the unsupported kind, got: {error}"
        );
    }
}

#[test]
fn usb_and_csi_identity_would_be_durable_hardware_identity_not_a_transient_device_path() {
    // USB/CSI are refused today (no capture path exists), so the ONLY
    // observable proof available through the real loader is that the
    // hardware-identity VALUE the operator wrote survives verbatim into
    // the refusal (it is resolved, not silently dropped or replaced by an
    // artifact-generated `/dev/videoN`-style path) before capability
    // refusal fires.
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "workshop"
            usb_device = "usb-1234:5678-serial-ABC123"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect_err("usb-only entry is refused as unsupported_by_this_artifact");

    assert!(
        error.contains("usb-1234:5678-serial-ABC123"),
        "the durable hardware identity value itself must not appear stripped from the refusal, got: {error}"
    );
}

#[test]
fn renaming_a_camera_never_changes_its_resolved_rtsp_identity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let original_path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "workshop"
            rtsp_url = "rtsp://camera.local/substream"
        "#,
    );
    let renamed_path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "workshop-renamed"
            rtsp_url = "rtsp://camera.local/substream"
        "#,
    );

    let original = vigil::camera_source_summaries_from_args(args_for_config(&original_path))
        .expect("original resolves through the real load");
    let renamed = vigil::camera_source_summaries_from_args(args_for_config(&renamed_path))
        .expect("renamed resolves through the real load");

    assert_eq!(
        original, renamed,
        "the resolved source (durable identity) must be unaffected by the display-only `name` field"
    );
}

#[test]
fn mjpeg_password_never_leaks_into_the_real_loads_error_text() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "barn corner"
            mjpeg_url = "http://esp32cam.local/stream"
            username = "vigil"
            password = "hunter2"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect_err("mjpeg-only entry is refused as unsupported_by_this_artifact");

    assert!(
        !error.contains("hunter2"),
        "the MJPEG password must never appear in the real load's error text, got: {error}"
    );
}

/// The password on the SEPARATE `password` field is `Secret`-protected and
/// never echoed at all (the sibling test above). A password embedded
/// directly IN the URL — `http://user:pass@host/...`, exactly how the
/// ESP32-CAM class of device is commonly configured — is a different code
/// path: it rides through as plain text on `mjpeg_url` itself, so the
/// refusal message must redact it, not merely omit an unrelated field.
#[test]
fn a_credentialed_mjpeg_url_redacts_the_embedded_password_but_keeps_the_host_in_the_refusal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "barn corner"
            mjpeg_url = "http://vigil:hunter2@esp32cam.local/stream"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect_err("mjpeg-only entry is refused as unsupported_by_this_artifact");

    assert!(
        !error.contains("hunter2"),
        "a password embedded in the mjpeg_url itself must never appear in the real load's \
         error text, got: {error}"
    );
    assert!(
        !error.contains("vigil:hunter2"),
        "the userinfo component must be fully redacted, not merely have its password \
         truncated, got: {error}"
    );
    assert!(
        error.contains("esp32cam.local"),
        "the host must survive redaction — it is what makes the refusal actionable, got: {error}"
    );
    assert!(
        error.contains("<redacted>"),
        "the redaction must be visible as such, not silently vanish, got: {error}"
    );
}

/// A credential does not need a `user:pass@` shape at all — a query
/// parameter is a common place for a camera/streaming endpoint to carry a
/// token (`?token=...`). Proven through the real refusal message, not
/// just the redaction helper directly.
#[test]
fn a_query_carried_token_in_an_mjpeg_url_never_leaks_into_the_real_loads_error_text() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "barn corner"
            mjpeg_url = "http://esp32cam.local/stream?token=abc123"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect_err("mjpeg-only entry is refused as unsupported_by_this_artifact");

    assert!(
        !error.contains("abc123") && !error.contains("token="),
        "a query-carried token must never appear in the real load's error text, even with no \
         user:pass@ userinfo present at all, got: {error}"
    );
    assert!(
        error.contains("esp32cam.local"),
        "the host must still survive — it is what makes the refusal actionable, got: {error}"
    );
}

// ---------------------------------------------------------------------
// All four FieldOutcome variants (Omitted, Empty, Invalid, Unavailable),
// obtained from the REAL loader — never from calling a classifier helper
// directly. Omitted and Unavailable are already exercised above (the
// "no source field" test and the "every adapter kind" test); these four
// tests pin ALL FOUR as genuinely distinct real-load outcomes, including
// the two the independent review found missing: Empty (conflated with
// Omitted) and Invalid (never producible at all).
// ---------------------------------------------------------------------

#[test]
fn omitted_and_empty_are_distinct_real_load_outcomes_with_different_messages() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let omitted_path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "nameless"
        "#,
    );
    let omitted_error = vigil::camera_source_summaries_from_args(args_for_config(&omitted_path))
        .expect_err("no source field declared at all must fail the real load");

    let empty_path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "blank-field"
            rtsp_url = ""
        "#,
    );
    let empty_error = vigil::camera_source_summaries_from_args(args_for_config(&empty_path))
        .expect_err("an explicitly empty source field must also fail the real load");

    assert_ne!(
        omitted_error, empty_error,
        "never touching a source field and explicitly setting it to an empty string must \
         produce DIFFERENT error text — they are different operator mistakes, got identical: \
         {omitted_error}"
    );
    assert!(
        empty_error.contains("declares an empty source field"),
        "the empty-field error must say so explicitly, got: {empty_error}"
    );
}

/// An explicitly empty source field beside a VALID one — the combination
/// the isolated-empty test above never exercises. Today's loader filters
/// `Empty` outcomes out of its "did anything get attempted" check BEFORE
/// looking for a conflict, so with `rtsp_url` valid and (say) `usb_device
/// = ""` present, the empty field vanishes from consideration entirely and
/// the entry silently loads as plain RTSP — the operator who typed the
/// field and left it blank gets no signal at all. An explicit empty value
/// is an actionable error, not a no-op, REGARDLESS of whether another
/// field on the same entry happens to be valid.
#[test]
fn an_explicitly_empty_source_field_is_rejected_even_alongside_a_valid_rtsp_url_naming_the_field() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let camera_name = "front-gate";

    let mut errors = Vec::new();
    for (empty_field_toml, empty_field_name) in [
        (r#"usb_device = """#, "usb_device"),
        (r#"csi_module = """#, "csi_module"),
        (r#"mjpeg_url = """#, "mjpeg_url"),
    ] {
        let toml = format!(
            r#"
                [[cameras]]
                name = "{camera_name}"
                rtsp_url = "rtsp://camera.local/substream"
                {empty_field_toml}
            "#
        );
        let path = write_config(&tmp, &toml);

        let error = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect_err(
            "a valid rtsp_url alongside an explicitly empty adapter field must still fail the \
             real load — the empty field is an actionable operator mistake and must never be \
             silently ignored just because another field on the same entry happens to be \
             valid",
        );

        assert!(
            error.contains(camera_name),
            "the real load error must name the camera, got: {error}"
        );
        assert!(
            error.contains("declares an empty source field"),
            "an explicitly empty {empty_field_name} beside a valid rtsp_url must fire the \
             empty-field wording, not some other branch, got: {error}"
        );
        assert!(
            !error.contains("declares more than one source"),
            "an explicitly empty {empty_field_name} beside a valid rtsp_url is a blank-field \
             mistake, not a multi-source conflict — an operator with one real URL and one blank \
             field must never be told they declared more than one source, got: {error}"
        );
        errors.push((empty_field_name, error));
    }

    // The SAME camera name and the SAME valid rtsp_url are used in all
    // three iterations above, so the only thing that differs between them
    // is WHICH field was left empty — the error text must reflect that
    // (never the identical generic sentence for all three), which is what
    // "naming ... the empty field" requires.
    assert_ne!(
        errors[0].1, errors[1].1,
        "an empty {} and an empty {} beside the identical valid rtsp_url must produce \
         DIFFERENT error text naming which field was left empty, got identical text: {}",
        errors[0].0, errors[1].0, errors[0].1
    );
    assert_ne!(
        errors[1].1, errors[2].1,
        "an empty {} and an empty {} beside the identical valid rtsp_url must produce \
         DIFFERENT error text naming which field was left empty, got identical text: {}",
        errors[1].0, errors[2].0, errors[1].1
    );
    assert_ne!(
        errors[0].1, errors[2].1,
        "an empty {} and an empty {} beside the identical valid rtsp_url must produce \
         DIFFERENT error text naming which field was left empty, got identical text: {}",
        errors[0].0, errors[2].0, errors[0].1
    );
}

#[test]
fn an_unparsable_rtsp_url_is_the_real_loads_invalid_outcome_and_never_echoes_the_raw_value() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "front gate"
            rtsp_url = "vigil:hunter2@camera.local/substream"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect_err("a value that does not parse as a real rtsp:// URL must fail the real load");

    assert!(
        !error.contains("unsupported_by_this_artifact"),
        "an unparsable value is an INVALID value, never conflated with the artifact-capability \
         refusal — a typo does not mean 'install different hardware', got: {error}"
    );
    assert!(
        !error.contains("hunter2") && !error.contains("vigil:hunter2@camera.local"),
        "an Invalid rtsp_url must not echo the raw value AT ALL (not even redacted) — the value \
         itself might be exactly the malformed credential-bearing string that made it invalid, \
         got: {error}"
    );
}

/// `rtsps://` (RTSP over TLS) is a real, legitimate camera scheme — an
/// operator who chose the encrypted transport must not be told they typed
/// something wrong. Pinned here, next to the invalid-URL tests, so scheme
/// validation is never narrowed back down to plain `rtsp` only.
#[test]
fn an_rtsps_url_is_accepted_as_a_valid_encrypted_rtsp_scheme_not_invalid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "front gate"
            rtsp_url = "rtsps://camera.local:322/stream"
        "#,
    );

    let summaries = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect("rtsps:// is RTSP over TLS, a valid rtsp_url scheme, and must load successfully");

    assert_eq!(
        summaries[0],
        CameraSourceSummary::Rtsp {
            rtsp_url: "rtsps://camera.local:322/stream".to_string(),
            live_rtsp_url: None,
        },
        "an rtsps:// camera must resolve exactly like an rtsp:// one, never refused as Invalid"
    );
}

#[test]
fn a_whitespace_prefixed_dev_video_path_for_usb_device_is_rejected_as_non_durable() {
    // The exact same defect as the unpadded `/dev/video0` case below, just
    // with leading whitespace: `CameraId::from_durable_hardware_value`
    // trims before checking the `/dev/` prefix, so a value that is
    // durable-shaped only after stripping padding must be refused
    // identically — a load-time validator that checks the raw,
    // untrimmed string diverges from what identity construction actually
    // does with it.
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "workshop"
            usb_device = " /dev/video0"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect_err(
        "a whitespace-padded /dev/videoN path is still a transient device path, not a durable \
         hardware identity, and must fail the real load",
    );

    assert!(
        !error.contains("unsupported_by_this_artifact"),
        "a malformed hardware identity is an INVALID value, never the artifact-capability \
         refusal, got: {error}"
    );
}

#[test]
fn a_transient_dev_video_path_for_usb_device_is_the_real_loads_invalid_outcome() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "workshop"
            usb_device = "/dev/video0"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect_err(
        "a transient /dev/videoN path is not a durable hardware identity and must fail the real load",
    );

    assert!(
        !error.contains("unsupported_by_this_artifact"),
        "a malformed hardware identity is an INVALID value, never the artifact-capability \
         refusal, got: {error}"
    );
}

#[test]
fn a_well_formed_but_unsupported_mjpeg_url_is_the_real_loads_unavailable_outcome_not_invalid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "barn corner"
            mjpeg_url = "http://esp32cam.local/stream"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect_err("mjpeg is refused as unsupported_by_this_artifact, not as invalid");

    assert!(
        error.contains("unsupported_by_this_artifact"),
        "a well-formed URL this artifact cannot carry is Unavailable, distinct from Invalid — \
         the fix is a different artifact, not a typo correction, got: {error}"
    );
    assert!(
        !error.to_ascii_lowercase().contains("invalid"),
        "an Unavailable outcome must not also be described as invalid — the two outcomes call \
         for different operator fixes, got: {error}"
    );
}

// ---------------------------------------------------------------------
// The userinfo-redaction bypass the independent review found: a value
// with no LITERAL `://` (scheme-relative or bare authority) used to pass
// `redact_url_userinfo` through unchanged, carrying the password intact.
// The real fix is that such a value never parses as a URL at all, so it
// is refused as Invalid (never echoed) before any redaction runs.
// ---------------------------------------------------------------------

#[test]
fn a_scheme_relative_credentialed_mjpeg_url_is_refused_as_invalid_never_echoed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "barn corner"
            mjpeg_url = "//vigil:hunter2@esp32cam.local/stream"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect_err("a scheme-relative value is not a real URL and must be refused as Invalid");

    assert!(
        !error.contains("hunter2"),
        "the bypass this closes: a scheme-relative value must never reach a refusal with its \
         password intact, got: {error}"
    );
}

#[test]
fn a_bare_authority_credentialed_mjpeg_url_is_refused_as_invalid_never_echoed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "barn corner"
            mjpeg_url = "vigil:hunter2@esp32cam.local/stream"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect_err(
        "a bare authority with no scheme separator at all is not a real URL and must be refused \
         as Invalid",
    );

    assert!(
        !error.contains("hunter2"),
        "the bypass this closes: a bare `user:pass@host` value with no `://` anywhere must \
         never reach a refusal with its password intact, got: {error}"
    );
}

#[test]
fn a_malformed_toml_config_reports_location_only_never_the_offending_source_line() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // An unterminated string literal: a genuine TOML syntax error whose
    // offending line happens to carry a credential-bearing value. The
    // `toml` crate's own `Display` impl for parse errors renders an
    // annotated snippet of exactly this source line by default — this
    // test proves vigil's own wrapping message does not repeat it.
    let path = write_config(
        &tmp,
        "[[cameras]]\nname = \"barn corner\"\nmjpeg_url = \"http://vigil:hunter2@esp32cam.local/stream\n",
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect_err("an unterminated string literal is a real TOML parse error");

    assert!(
        !error.contains("hunter2"),
        "a TOML parse error must never echo the offending source line's contents, got: {error}"
    );
    assert!(
        error.contains("line") && error.contains("column"),
        "a TOML parse error must still report WHERE the problem is (line/column), just not the \
         line's contents, got: {error}"
    );
}

// ── An explicit empty [[cameras]] list is a legitimate cameraless node ────
//
// `cameras = []`, written deliberately, must never be treated as though the
// field were absent: it names a worker/discovery node with no camera at
// all, not a request for the legacy single-camera fallback. An absent
// `cameras` key (no such line in the file at all) is the ONLY case that may
// still synthesize the legacy single-camera entry from the top-level
// `rtsp_url`/`usb_device`/`csi_module`/`mjpeg_url` fields.

#[test]
fn an_explicit_empty_cameras_list_produces_no_legacy_single_camera_fallback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(&tmp, "cameras = []\n");

    let summaries = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect("an explicitly empty cameras list must load through the real production path");

    assert!(
        summaries.is_empty(),
        "a deliberately empty [[cameras]] list names a legitimate cameraless node and must not \
         be filled in with the legacy single-camera synthesis (which would produce a Ready, \
         zero-camera runtime that looks healthy while watching nothing); got {} camera source(s): \
         {summaries:?}",
        summaries.len()
    );
}

#[test]
fn an_absent_cameras_list_still_synthesizes_the_legacy_single_camera_entry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            rtsp_url = "rtsp://legacy-cam.local:554/stream"
        "#,
    );

    let summaries = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect(
        "a config that never mentions `cameras` at all must still load through the real \
         production path",
    );

    assert_eq!(
        summaries,
        vec![CameraSourceSummary::Rtsp {
            rtsp_url: "rtsp://legacy-cam.local:554/stream".to_string(),
            live_rtsp_url: None,
        }],
        "an absent cameras list (the field never appears in the file) must keep synthesizing \
         exactly the legacy single-camera entry from the top-level single-camera fields, \
         unchanged by the explicit-empty-list behavior pinned above"
    );
}

// ── The same source kind must get the same accept/refuse answer on every
//    configuration route ─────────────────────────────────────────────────
//
// `--usb-device`/`--csi-module`/`--mjpeg-url` thread into the legacy
// single-camera entry exactly like `--rtsp-url` does. A source kind this
// artifact cannot carry is honestly refused when declared through a
// `[[cameras]]` table entry (proven above by
// `every_adapter_source_kind_is_honestly_rejected_by_the_real_load_as_unsupported_by_this_artifact`).
// The identical value, declared through the equivalent legacy CLI flag with
// no `[[cameras]]` table at all, must be refused the same way, naming the
// same kind — never silently accepted on one route while the other route
// honestly refuses it.

#[test]
fn usb_source_is_refused_identically_through_the_cameras_list_and_the_legacy_cli_flag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let table_path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "table-route"
            usb_device = "usb-1234:5678-serial-ABC123"
        "#,
    );

    let table_error = vigil::camera_source_summaries_from_args(args_for_config(&table_path))
        .expect_err("a [[cameras]] usb_device entry is refused as unsupported_by_this_artifact");

    let cli_error = vigil::camera_source_summaries_from_args(vec![
        OsString::from("--usb-device"),
        OsString::from("usb-1234:5678-serial-ABC123"),
    ])
    .expect_err(
        "the identical usb_device value, declared through the legacy --usb-device CLI flag with \
         no [[cameras]] table at all, must be refused exactly like the table route is — never \
         silently accepted just because it arrived through a different configuration route",
    );

    assert!(
        table_error.contains("unsupported_by_this_artifact"),
        "the table route's own refusal must use the standard vocabulary, got: {table_error}"
    );
    assert!(
        cli_error.contains("unsupported_by_this_artifact"),
        "the CLI-flag route's refusal must use the identical standard vocabulary the table route \
         uses, got: {cli_error}"
    );
    assert!(
        table_error.contains("usb") && cli_error.contains("usb"),
        "both routes must name the same refused kind (usb): table={table_error} cli={cli_error}"
    );
}

#[test]
fn csi_source_is_refused_identically_through_the_cameras_list_and_the_legacy_cli_flag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let table_path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "table-route"
            csi_module = "csi-imx477-cam0"
        "#,
    );

    let table_error = vigil::camera_source_summaries_from_args(args_for_config(&table_path))
        .expect_err("a [[cameras]] csi_module entry is refused as unsupported_by_this_artifact");

    let cli_error = vigil::camera_source_summaries_from_args(vec![
        OsString::from("--csi-module"),
        OsString::from("csi-imx477-cam0"),
    ])
    .expect_err(
        "the identical csi_module value, declared through the legacy --csi-module CLI flag with \
         no [[cameras]] table at all, must be refused exactly like the table route is — never \
         silently accepted just because it arrived through a different configuration route",
    );

    assert!(
        table_error.contains("unsupported_by_this_artifact"),
        "the table route's own refusal must use the standard vocabulary, got: {table_error}"
    );
    assert!(
        cli_error.contains("unsupported_by_this_artifact"),
        "the CLI-flag route's refusal must use the identical standard vocabulary the table route \
         uses, got: {cli_error}"
    );
    assert!(
        table_error.contains("csi") && cli_error.contains("csi"),
        "both routes must name the same refused kind (csi): table={table_error} cli={cli_error}"
    );
}

#[test]
fn mjpeg_source_is_refused_identically_through_the_cameras_list_and_the_legacy_cli_flag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let table_path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "table-route"
            mjpeg_url = "http://example.com/esp32cam-stream"
        "#,
    );

    let table_error = vigil::camera_source_summaries_from_args(args_for_config(&table_path))
        .expect_err("a [[cameras]] mjpeg_url entry is refused as unsupported_by_this_artifact");

    let cli_error = vigil::camera_source_summaries_from_args(vec![
        OsString::from("--mjpeg-url"),
        OsString::from("http://example.com/esp32cam-stream"),
    ])
    .expect_err(
        "the identical mjpeg_url value, declared through the legacy --mjpeg-url CLI flag with no \
         [[cameras]] table at all, must be refused exactly like the table route is — never \
         silently accepted just because it arrived through a different configuration route",
    );

    assert!(
        table_error.contains("unsupported_by_this_artifact"),
        "the table route's own refusal must use the standard vocabulary, got: {table_error}"
    );
    assert!(
        cli_error.contains("unsupported_by_this_artifact"),
        "the CLI-flag route's refusal must use the identical standard vocabulary the table route \
         uses, got: {cli_error}"
    );
    assert!(
        table_error.contains("mjpeg") && cli_error.contains("mjpeg"),
        "both routes must name the same refused kind (mjpeg): table={table_error} cli={cli_error}"
    );
}

// ---------------------------------------------------------------------
// Duplicate-camera detection: two entries whose canonical analysis
// endpoints are identical are a configuration error, not two silently
// accepted cameras. Only RTSP is exercised through the real loader here —
// this build artifact honestly refuses every other source kind as
// `unsupported_by_this_artifact` before duplicate detection would ever be
// reached (see `every_adapter_source_kind_is_honestly_rejected_...` above);
// the canonical-endpoint semantics themselves (path/query significant,
// case/port folded, userinfo stripped) are pinned directly against
// `CameraId` in `camera_identity_construction.rs`.
// ---------------------------------------------------------------------

#[test]
fn two_camera_entries_with_the_same_canonical_rtsp_analysis_endpoint_are_rejected_as_a_duplicate_camera_naming_both()
 {
    let tmp = tempfile::tempdir().expect("tempdir");
    // The two rtsp_url values are byte-different (mixed host case, and one
    // spells out the RTSP default port while the other omits it) but
    // resolve to the identical CANONICAL endpoint — this must be caught by
    // canonical-identity comparison, not a raw string-equality check.
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "front-yard"
            rtsp_url = "rtsp://Camera.LOCAL:554/substream"

            [[cameras]]
            name = "front-yard-again"
            rtsp_url = "rtsp://camera.local/substream"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect_err(
        "two [[cameras]] entries whose analysis endpoints canonically collide must fail the real load as a duplicate camera, never load as two separate cameras",
    );

    assert!(
        error.contains("front-yard") && error.contains("front-yard-again"),
        "the duplicate-camera error must name BOTH colliding entries so the operator knows which two to reconcile, got: {error}"
    );
    assert!(
        error.to_ascii_lowercase().contains("duplicate"),
        "the duplicate-camera error must say what is wrong (a duplicate camera), not just name the entries, got: {error}"
    );
}

#[test]
fn two_camera_entries_with_different_paths_on_the_same_rtsp_host_load_as_two_distinct_cameras() {
    // The positive companion to the duplicate-detection test above, and the
    // exact defect this ruling fixes stated in config-load terms: two
    // cameras behind one host at different paths are two real, distinct,
    // separately configured cameras, and must both load successfully.
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "driveway-ch1"
            rtsp_url = "rtsp://nvr.local:554/ch1"

            [[cameras]]
            name = "driveway-ch2"
            rtsp_url = "rtsp://nvr.local:554/ch2"
        "#,
    );

    let summaries = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect(
        "two entries with different paths on the same host are distinct cameras and must load, never be rejected as a duplicate",
    );

    assert_eq!(
        summaries.len(),
        2,
        "both distinctly-pathed cameras must be present in the resolved summaries, got: {summaries:?}"
    );
    assert_eq!(
        summaries[0],
        CameraSourceSummary::Rtsp {
            rtsp_url: "rtsp://nvr.local:554/ch1".to_string(),
            live_rtsp_url: None,
        }
    );
    assert_eq!(
        summaries[1],
        CameraSourceSummary::Rtsp {
            rtsp_url: "rtsp://nvr.local:554/ch2".to_string(),
            live_rtsp_url: None,
        }
    );
}

#[test]
fn an_entrys_live_rtsp_url_never_participates_in_duplicate_camera_detection() {
    // Entry-level role congruence: an entry's identity for duplicate
    // detection derives from its ANALYSIS endpoint (`rtsp_url`) alone.
    // Here entry "front-analysis" declares a `live_rtsp_url` that is
    // byte-identical to entry "shared-host-live"'s own `rtsp_url`
    // (analysis endpoint). If `live_rtsp_url` wrongly participated in
    // duplicate detection, this would be flagged as a collision; because it
    // must not, both entries are real, distinct cameras and must load.
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "front-analysis"
            rtsp_url = "rtsp://front.local:554/substream"
            live_rtsp_url = "rtsp://shared-host.local:554/live"

            [[cameras]]
            name = "shared-host-live"
            rtsp_url = "rtsp://shared-host.local:554/live"
        "#,
    );

    let summaries = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect(
        "an entry's live_rtsp_url coinciding with a different entry's rtsp_url must never be treated as a duplicate camera — live_rtsp_url never participates in identity",
    );

    assert_eq!(
        summaries.len(),
        2,
        "both entries are real, distinct cameras and must both load, got: {summaries:?}"
    );
}

// ---------------------------------------------------------------------
// A hostless opaque rtsp_url (e.g. `rtsp:camera/stream`, valid scheme but
// no `//host`) must be rejected at load, not merely at scheme-parse
// level: `CameraId::from_rtsp_url` builds identity from host and port, so
// a hostless value can never yield a durable identity, and duplicate
// detection (`reject_duplicate_camera_analysis_endpoints`) silently skips
// any entry `CameraId::from_rtsp_url` fails to construct — so accepting a
// hostless URL at load time doesn't just admit an unidentifiable camera,
// it also lets that exact defect evade the duplicate-camera check the
// canonical-endpoint tests above pin.
// ---------------------------------------------------------------------

#[test]
fn a_hostless_opaque_rtsp_url_is_rejected_at_the_real_load_with_an_actionable_message() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "opaque-camera"
            rtsp_url = "rtsp:camera/stream"
        "#,
    );

    let error = vigil::camera_source_summaries_from_args(args_for_config(&path)).expect_err(
        "a hostless rtsp: URL has a valid scheme but no host CameraId::from_rtsp_url can build \
         a durable identity from, and must fail the real load, never be silently accepted",
    );

    assert!(
        error.contains("opaque-camera"),
        "the real load error must name the camera, got: {error}"
    );
    assert!(
        !error.contains("unsupported_by_this_artifact"),
        "a hostless URL is an INVALID value, never the artifact-capability refusal, got: {error}"
    );
    assert!(
        error.to_ascii_lowercase().contains("rtsp_url"),
        "the real load error must name the fix, got: {error}"
    );
}

#[test]
fn two_camera_entries_with_the_same_hostless_opaque_rtsp_url_never_silently_load_as_two_distinct_cameras()
 {
    // Before the fix, `parse_camera_url` checked scheme only, so both
    // entries passed `resolve_camera_source_kind`; then
    // `reject_duplicate_camera_analysis_endpoints` tried
    // `CameraId::from_rtsp_url` on each, got `Err` (no host), and its
    // `let Ok(canonical) = ... else { continue }` silently skipped BOTH
    // entries from duplicate comparison — two byte-identical malformed
    // endpoints loaded as two separate cameras with no error at all. The
    // fix must not let a hostless value reach the cameras list in the
    // first place, so this can never recur.
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "opaque-a"
            rtsp_url = "rtsp:camera/stream"

            [[cameras]]
            name = "opaque-b"
            rtsp_url = "rtsp:camera/stream"
        "#,
    );

    let result = vigil::camera_source_summaries_from_args(args_for_config(&path));

    match result {
        Ok(summaries) => panic!(
            "two entries with the identical hostless rtsp_url must never silently load as \
             distinct cameras — got {} summaries: {summaries:?}",
            summaries.len()
        ),
        Err(error) => assert!(
            !error.is_empty(),
            "the real load must fail with an actionable message, not a silent empty error"
        ),
    }
}

#[test]
fn an_ordinary_hosted_rtsp_url_still_loads_unchanged_alongside_the_hostless_rejection() {
    // The companion positive case for the two hostless tests above: a
    // normal rtsp_url with a real host must be completely unaffected by
    // the hostless check.
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        &tmp,
        r#"
            [[cameras]]
            name = "driveway"
            rtsp_url = "rtsp://camera.local:554/stream"
        "#,
    );

    let summaries = vigil::camera_source_summaries_from_args(args_for_config(&path))
        .expect("an ordinary hosted rtsp_url must still load unchanged");

    assert_eq!(
        summaries,
        vec![CameraSourceSummary::Rtsp {
            rtsp_url: "rtsp://camera.local:554/stream".to_string(),
            live_rtsp_url: None,
        }]
    );
}
