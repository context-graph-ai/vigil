//! The five control states an operator sees, proven on the surface the
//! operator actually reads — the rendered settings report — not only in the
//! enum. Automatic and Set-by-you render from the records that produce them;
//! a value inside an on domain reads Managed by that domain and never
//! top-level Auto-adjusted; and a pushed value renders with what the server
//! said and when.
//!
//! Why these are unfakeable: every state is asserted through
//! [`SettingsReport::render_lines`], so a type that can hold a state but
//! never renders it fails here — "no state is accepted on the grounds that
//! the type could render it". Each state is also produced by writing a real
//! record and read back through a fresh direct read of the store, so the
//! rendering is driven by stored records rather than by a value the test
//! handed the renderer. The late-promotion test compares the line BEFORE
//! and AFTER the domain revises the value, so a renderer that printed a
//! constant would show no movement. The management-server test pins the
//! store's persisted-time seam to a fixed instant and repeats the whole
//! exercise at a second instant: the rendered line must differ, which no
//! implementation can satisfy without genuinely carrying the "when".

use vigil::PersistedClock;
use vigil::settings_backends::{DETECTION_BACKEND_SETTING, available_detection_backends};
use vigil::settings_domains::{ACCELERATED_DETECTION_DOMAIN, governing_domain};
use vigil::settings_model::{
    Author, ControlState, Scope, ScopeTarget, SettingRecord, SettingValue, Surface,
};
use vigil::settings_projection::{HELD_LINE_PREFIX, SETTING_LINE_PREFIX, report_by_direct_read_at};
use vigil::settings_reflection::RESTART_ON_REFLECT_SETTING;
use vigil::settings_store::SettingsStore;

const TENANT: &str = "acme";
const SITE: &str = "harbour-yard";
const NODE: &str = "node-a";

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: TENANT.to_string(),
        site: SITE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

/// Lowercased, punctuation-flattened text with `_` preserved, so a phrase
/// check reads the same whether the surface writes `Managed by accelerated
/// detection`, `managed_by=accelerated_detection`, or either wrapped in
/// punctuation.
fn normalized(text: &str) -> String {
    let flattened: String = text
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    flattened.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every rendered line carrying `prefix` that names `setting`. The prefixes
/// are the ones the projection declares, so this reads the surface by its
/// own published shape rather than by a format guessed here.
fn lines_for(lines: &[String], prefix: &str, setting: &str) -> Vec<String> {
    lines
        .iter()
        .filter(|line| line.trim_start().starts_with(prefix) && line.contains(setting))
        .cloned()
        .collect()
}

/// The operator surface for a deployment whose runtime is not up, resolved
/// at the SAME deployment identity the records below are written against —
/// a node named [`NODE`] in site [`SITE`] of tenant [`TENANT`], which is
/// what [`target`] names. The target-less `report_by_direct_read` cannot be
/// used here: it derives its own identity, so an empty temporary directory
/// would resolve at some default site and never see a record written at
/// `Scope::node(NODE)` under tenant [`TENANT`], leaving every assertion
/// below unsatisfiable however the surface is implemented.
fn rendered(deployment: &std::path::Path) -> Vec<String> {
    report_by_direct_read_at(deployment, &target())
        .expect("build the settings report by reading the store directly")
        .render_lines()
}

/// The phrase an operator reads for one control state, normalized the same
/// way a rendered line is. Single-sourced from [`ControlState::label`] so
/// the surface's wording has one home: a test that spelled the phrase out
/// itself would be a second, silently drifting copy of the vocabulary.
fn state_phrase(state: &ControlState) -> String {
    normalized(&state.label())
}

/// The phrase for a value the accelerated-detection domain is managing.
fn managed_by_accelerated_detection() -> String {
    state_phrase(&ControlState::ManagedBy(
        ACCELERATED_DETECTION_DOMAIN.to_string(),
    ))
}

fn one_setting_line(lines: &[String], setting: &str) -> String {
    let matches = lines_for(lines, SETTING_LINE_PREFIX, setting);
    assert_eq!(
        matches.len(),
        1,
        "exactly one `{SETTING_LINE_PREFIX}` line names {setting}; got {matches:?}"
    );
    matches[0].clone()
}

fn a_real_backend() -> String {
    let available = available_detection_backends();
    assert!(
        !available.is_empty(),
        "every artifact carries at least one detection backend; the compiled inventory is empty"
    );
    available[0].to_string()
}

#[test]
fn automatic_and_set_by_you_render_from_the_records_that_produce_them() {
    assert!(
        governing_domain(RESTART_ON_REFLECT_SETTING).is_none(),
        "{RESTART_ON_REFLECT_SETTING} must be ungoverned for this test to read the ungoverned \
         states rather than the gate's"
    );

    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let scope = Scope::node(NODE);
    let automatic_reason = "vigil's own default for reflecting pushed values";

    {
        let store =
            SettingsStore::open(deployment.path()).expect("open the node-side settings store");
        store
            .write_record(SettingRecord::automatic(
                RESTART_ON_REFLECT_SETTING,
                scope.clone(),
                SettingValue::Bool(true),
                automatic_reason,
            ))
            .expect("vigil's own product-selected value is a record like any other");

        let effective = store
            .resolve(RESTART_ON_REFLECT_SETTING, &target())
            .expect("resolve the reflection setting");
        assert_eq!(effective.control_state, ControlState::Automatic);
        assert_eq!(effective.author, Author::Automatic);
    }

    let automatic_line = one_setting_line(&rendered(deployment.path()), RESTART_ON_REFLECT_SETTING);
    let automatic_text = normalized(&automatic_line);
    assert!(
        automatic_text.contains(&state_phrase(&ControlState::Automatic)),
        "a value vigil chose renders as Automatic; got {automatic_line:?}"
    );
    assert!(
        !automatic_text.contains(&state_phrase(&ControlState::SetByYou)),
        "nobody set this value; got {automatic_line:?}"
    );
    assert!(
        automatic_line.contains(automatic_reason),
        "an automatic record owes a reason naming its derivation input, and the surface shows \
         it; got {automatic_line:?}"
    );

    // Now the operator pins it. The state must follow the new record — and
    // the automatic record it displaced must still be visible, shadowed.
    {
        let store =
            SettingsStore::open(deployment.path()).expect("reopen the node-side settings store");
        store
            .set_local(
                RESTART_ON_REFLECT_SETTING,
                Surface::AddonOptions,
                scope,
                SettingValue::Bool(false),
            )
            .expect("an ungoverned setting is open to being set");

        let effective = store
            .resolve(RESTART_ON_REFLECT_SETTING, &target())
            .expect("resolve the reflection setting after the pin");
        assert_eq!(effective.control_state, ControlState::SetByYou);
        assert_eq!(effective.author, Author::LocalExplicit);
        assert_eq!(effective.surface, Surface::AddonOptions);
    }

    let pinned_lines = rendered(deployment.path());
    let pinned_line = one_setting_line(&pinned_lines, RESTART_ON_REFLECT_SETTING);
    let pinned_text = normalized(&pinned_line);
    assert!(
        pinned_text.contains(&state_phrase(&ControlState::SetByYou))
            && !pinned_text.contains(&state_phrase(&ControlState::SetByManagementServer)),
        "a pin here reads Set by you, distinct from Set by your management server; got \
         {pinned_line:?}"
    );
    assert_ne!(
        normalized(&automatic_line),
        pinned_text,
        "the rendered state must follow the records; an unchanged line means it does not"
    );
    let held = lines_for(&pinned_lines, HELD_LINE_PREFIX, RESTART_ON_REFLECT_SETTING);
    assert_eq!(
        held.len(),
        1,
        "the displaced automatic record stays stored and is shown on the face of the surface; \
         held lines: {held:?}"
    );
}

#[test]
fn a_governed_value_reads_managed_by_its_domain_never_top_level_auto_adjusted() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let reason = "the accelerated-detection domain selected this backend at startup";

    {
        let store =
            SettingsStore::open(deployment.path()).expect("open the node-side settings store");
        store
            .write_record(SettingRecord::automatic(
                DETECTION_BACKEND_SETTING,
                Scope::node(NODE),
                SettingValue::text(a_real_backend()),
                reason,
            ))
            .expect("the domain's own choice is stored as an automatic record");

        let effective = store
            .resolve(DETECTION_BACKEND_SETTING, &target())
            .expect("resolve the detection backend with the domain on");
        assert_eq!(
            effective.control_state,
            ControlState::ManagedBy(ACCELERATED_DETECTION_DOMAIN.to_string()),
            "governed beats revised: the state names the domain, so it carries its own \
             instructions"
        );
        assert_ne!(
            effective.control_state,
            ControlState::AutoAdjusted,
            "top-level Auto-adjusted belongs to an ungoverned value, and this build has none"
        );
    }

    let lines = rendered(deployment.path());
    let line = one_setting_line(&lines, DETECTION_BACKEND_SETTING);
    let text = normalized(&line);
    assert!(
        text.contains(&managed_by_accelerated_detection()),
        "the surface names the domain that is managing the value; got {line:?}"
    );
    for related in lines_for(&lines, SETTING_LINE_PREFIX, DETECTION_BACKEND_SETTING)
        .into_iter()
        .chain(lines_for(
            &lines,
            HELD_LINE_PREFIX,
            DETECTION_BACKEND_SETTING,
        ))
    {
        assert!(
            !normalized(&related).contains(&state_phrase(&ControlState::AutoAdjusted)),
            "a governed value never reads top-level Auto-adjusted; got {related:?}"
        );
    }
}

/// The late promotion needs the accelerated detection backend to exist in
/// this artifact — a processor-only build never makes this move — so it is
/// exercised on the shape that compiles it in.
#[test]
#[cfg(feature = "detect-burn-wgpu")]
fn the_late_accelerated_promotion_renders_as_managed_by_accelerated_detection_with_revision_reason_and_effect()
 {
    let backends = available_detection_backends();
    assert!(
        backends.len() >= 2,
        "an artifact carrying the accelerated detection backend offers the processor backend as \
         well; compiled inventory: {backends:?}"
    );
    let at_startup = backends[0].to_string();
    let after_the_probe = backends[1].to_string();
    assert_ne!(at_startup, after_the_probe);

    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let startup_reason = "no accelerated backend has proved itself on this machine yet";
    let promotion_reason = "the background accelerated-detection probe succeeded on this machine";

    {
        let store =
            SettingsStore::open(deployment.path()).expect("open the node-side settings store");
        store
            .write_record(SettingRecord::automatic(
                DETECTION_BACKEND_SETTING,
                Scope::node(NODE),
                SettingValue::text(&at_startup),
                startup_reason,
            ))
            .expect("the domain's startup choice is stored");
    }
    let before = one_setting_line(&rendered(deployment.path()), DETECTION_BACKEND_SETTING);
    assert!(
        before.contains(&at_startup),
        "before the probe succeeds the surface shows the startup choice; got {before:?}"
    );

    {
        let store =
            SettingsStore::open(deployment.path()).expect("reopen the node-side settings store");
        store
            .write_record(SettingRecord::automatic(
                DETECTION_BACKEND_SETTING,
                Scope::node(NODE),
                SettingValue::text(&after_the_probe),
                promotion_reason,
            ))
            .expect("the domain revises its own choice while the node runs");

        let effective = store
            .resolve(DETECTION_BACKEND_SETTING, &target())
            .expect("resolve the detection backend after the promotion");
        assert_eq!(
            effective.control_state,
            ControlState::ManagedBy(ACCELERATED_DETECTION_DOMAIN.to_string()),
            "the revision inside an on domain still reads Managed by that domain"
        );
        assert_eq!(effective.requested, SettingValue::text(&after_the_probe));
        assert_eq!(
            effective.reason, promotion_reason,
            "the revision carries the reason it happened"
        );
    }

    let after = one_setting_line(&rendered(deployment.path()), DETECTION_BACKEND_SETTING);
    assert_ne!(
        normalized(&before),
        normalized(&after),
        "the promotion is a revision: the surface must move, not repeat itself"
    );
    let text = normalized(&after);
    assert!(
        text.contains(&managed_by_accelerated_detection()),
        "the state stays Managed by accelerated detection through the revision; got {after:?}"
    );
    assert!(
        !text.contains(&state_phrase(&ControlState::AutoAdjusted)),
        "the revision shows underneath as the domain's own adjustment, never as the top-level \
         state; got {after:?}"
    );
    assert!(
        after.contains(promotion_reason),
        "the reason the value moved is on the surface; got {after:?}"
    );
    assert!(
        after.contains(&after_the_probe),
        "the effect — the backend now in force — is on the surface; got {after:?}"
    );
}

#[test]
fn set_by_your_management_server_renders_with_what_the_server_said_and_when() {
    assert!(
        governing_domain(RESTART_ON_REFLECT_SETTING).is_none(),
        "{RESTART_ON_REFLECT_SETTING} must be ungoverned so this test reads the pushed state \
         rather than the gate's"
    );

    let server_said = "the fleet policy requires reflected values to restart the add-on";

    // The same push, written twice against two different pinned instants.
    // Anything the surface renders for the "when" must differ between them.
    let first = push_and_render(1_700_000_000_000, server_said);
    let second = push_and_render(1_700_086_400_000, server_said);

    let first_text = normalized(&first.line);
    assert!(
        first_text.contains(&state_phrase(&ControlState::SetByManagementServer)),
        "a value the hub set names the management server as its author; got {:?}",
        first.line
    );
    assert!(
        first.line.contains(server_said),
        "what the server said travels with the value and is shown; got {:?}",
        first.line
    );
    assert_ne!(
        first.line, second.line,
        "the surface carries WHEN the server said it: two pushes differing only in the store's \
         persisted-time seam must not render identically"
    );

    // And the stamp really is the seamed clock's, not the wall clock.
    assert_eq!(first.written_at_ms, 1_700_000_000_000);
    assert_eq!(second.written_at_ms, 1_700_086_400_000);
}

struct PushedRendering {
    line: String,
    written_at_ms: i64,
}

/// Push one record through the hub-role handle against a pinned persisted
/// clock, then read it back exactly as an ordinary node would.
fn push_and_render(at_millis: u64, reason: &str) -> PushedRendering {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let clock = PersistedClock::from_millis_source(move || at_millis);
    let hub = SettingsStore::open_hub_role_with_clock(deployment.path(), clock)
        .expect("open the harness-side hub-role handle");
    hub.write_pushed_record(SettingRecord::pushed(
        RESTART_ON_REFLECT_SETTING,
        Scope::site(SITE),
        SettingValue::Bool(false),
        reason,
    ))
    .expect("the hub-role handle writes the down-only pushed table");

    let node = hub.node_view();
    let effective = node
        .resolve(RESTART_ON_REFLECT_SETTING, &target())
        .expect("an ordinary node resolves the pushed value");
    assert_eq!(effective.control_state, ControlState::SetByManagementServer);
    assert_eq!(effective.author, Author::Pushed);
    assert_eq!(effective.requested, SettingValue::Bool(false));

    let written_at_ms = node
        .records(RESTART_ON_REFLECT_SETTING)
        .expect("read the stored records")
        .into_iter()
        .find(|record| record.author == Author::Pushed)
        .unwrap_or_else(|| panic!("the pushed record is stored"))
        .written_at_ms;

    drop(node);
    drop(hub);
    let line = one_setting_line(&rendered(deployment.path()), RESTART_ON_REFLECT_SETTING);
    PushedRendering {
        line,
        written_at_ms,
    }
}
