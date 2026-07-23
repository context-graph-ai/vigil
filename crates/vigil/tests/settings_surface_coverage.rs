// Reads real production surfaces (the Home Assistant add-on options and
// each setting's own documentation page) to prove that every setting
// declared through the typed settings registry (`vigil::settings`) really
// appears where its declaration says an owner can see and change it. This
// is deliberately a production-source text scan — see the reviewed
// allowlist entry for this file.

use std::fs;

use vigil::settings::{
    CoverageGap, ObservedSurfaces, SettingCoverageEntry, SettingSurfaces, SurfaceKind,
    check_coverage,
};

#[path = "deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::workspace_root;

/// The one setting currently declared through the registry (see
/// `stationary_interval_setting_spec` in `config.rs`) — named directly so a
/// reader does not have to chase an index to know which setting the
/// canaries below target.
const PILOT_SETTING_NAME: &str = "detector_stationary_interval_secs";

fn pilot_entry(entries: &[SettingCoverageEntry]) -> SettingCoverageEntry {
    *entries
        .iter()
        .find(|entry| entry.name == PILOT_SETTING_NAME)
        .unwrap_or_else(|| panic!("expected {PILOT_SETTING_NAME} to be a declared setting"))
}

/// The config-file behavioral check needs a value to probe with, and the
/// probe value is type-dependent — this map is deliberately exhaustive
/// rather than defaulted, so declaring a new setting without adding its
/// probe value here fails loud instead of silently skipping the check.
fn probe_value_for(setting_name: &str) -> &'static str {
    match setting_name {
        "detector_stationary_interval_secs" => "45",
        other => panic!(
            "settings surface coverage has no config-file probe value registered for {other}; add one alongside its declaration"
        ),
    }
}

/// The existing, already-reviewed `<!-- vigil-claim: ... -->` marker (see
/// `.config/documentation-contracts.toml`) whose immediately preceding
/// paragraph is each setting's own contract-bearing documentation — the
/// exact table row naming it in `docs/configuration.md`'s Detection-fields
/// table. This mapping is TEST-ONLY bookkeeping: which verifier marker on
/// the documentation page carries a given setting's contract is a fact
/// about how this guard reads the page, not something the production
/// `SettingSurfaces` type (which only ever declares the page itself)
/// should carry. Deliberately exhaustive like [`probe_value_for`], so a
/// new setting without a registered claim id fails loud rather than
/// silently reverting to a page-wide check.
fn documentation_claim_id_for(setting_name: &str) -> &'static str {
    match setting_name {
        "detector_stationary_interval_secs" => {
            "vigil.docs-configuration.field-default-validation-and-behavior-detectormodelid-yoloxtinyburncpu"
        }
        other => panic!(
            "settings surface coverage has no documentation-claim id registered for {other}; add one alongside its declaration"
        ),
    }
}

/// The ONE observation function: checks all three of `entry`'s surfaces
/// against the given content and folds any hit into `observed`. Both the
/// real coverage test below and every parse-level canary call this SAME
/// function — never a second copy of its logic — so a canary that plants
/// an absence in stripped content genuinely exercises the production
/// observation path. If this function's matching ever drifted from what it
/// claims to check, every caller — the real test included — would drift
/// with it identically; that is the point of there being exactly one.
fn observe_entry_surfaces(
    entry: &SettingCoverageEntry,
    addon_config_text: &str,
    doc_text: &str,
    claim_id: &str,
    probe_value: &str,
    observed: &mut ObservedSurfaces,
) {
    // The config-file surface is checked behaviorally: parsing a fragment
    // that names the key must actually set the field it names, using the
    // real config-file parser.
    if vigil::config_file_recognizes_setting(entry.surfaces.config_key, probe_value) {
        observed
            .config_keys
            .insert(entry.surfaces.config_key.to_string());
    }

    // The add-on options surface is checked by looking for the declared
    // add-on option key itself, matching what `check_coverage` verifies —
    // not the setting's stable name, which can differ from its add-on key.
    if addon_config_text.contains(entry.surfaces.addon_option_key) {
        observed
            .addon_option_keys
            .insert(entry.surfaces.addon_option_key.to_string());
    }

    // The documentation surface is checked against the setting's own
    // contract-bearing paragraph ONLY — the text bound to `claim_id` by
    // the existing doc-claim marker convention
    // (`.config/documentation-contracts.toml`), never the whole page. An
    // incidental mention of the setting's name elsewhere on the same page
    // — prose describing a different surface, for instance — must not
    // satisfy this check; only its own bound paragraph counts. The
    // paragraph itself is found by CALLING the same parser the doc-claim
    // registry already freezes a hash of
    // (`xtask::test_estate::adjacent_paragraph`), not a second
    // reimplementation of its backward scan.
    let lines: Vec<&str> = doc_text.lines().collect();
    let marker = format!("<!-- vigil-claim: `{claim_id}` -->");
    if let Some(tag_start) = lines.iter().position(|line| line.trim() == marker)
        && let Some(paragraph) = xtask::test_estate::adjacent_paragraph(&lines, tag_start)
        && paragraph.contains(entry.name)
    {
        observed
            .documented_setting_names
            .insert(entry.name.to_string());
    }
}

/// Returns `doc_text` with `name` stripped ONLY from the lines inside the
/// paragraph bound to `claim_id`, leaving every other line — including an
/// incidental mention of `name` elsewhere on the page — byte-for-byte
/// untouched. Used to isolate exactly the page-wide substring hazard: a
/// page-wide substring search would still see a leftover incidental
/// mention and wrongly report the surface covered even after the real
/// documenting paragraph lost the setting's name. Uses the SAME shared
/// parser's line-range primitive (`xtask::test_estate::
/// adjacent_paragraph_line_range`) `observe_entry_surfaces` calls through
/// `adjacent_paragraph`, so the paragraph this mutates is guaranteed to be
/// the identical one the real check reads — never a second, independently
/// computed boundary that could quietly drift from it.
fn doc_text_with_name_stripped_from_bound_paragraph_only(
    doc_text: &str,
    claim_id: &str,
    name: &str,
) -> String {
    let mut lines: Vec<String> = doc_text.lines().map(str::to_string).collect();
    let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
    let marker = format!("<!-- vigil-claim: `{claim_id}` -->");
    let tag_start = borrowed
        .iter()
        .position(|line| line.trim() == marker)
        .unwrap_or_else(|| panic!("expected claim {claim_id} to be present in the real doc"));
    let (start, end) = xtask::test_estate::adjacent_paragraph_line_range(&borrowed, tag_start)
        .unwrap_or_else(|| panic!("expected a bound paragraph above claim {claim_id}"));
    for line in &mut lines[start..end] {
        *line = line.replace(name, "");
    }
    lines.join("\n")
}

#[test]
fn declared_settings_are_covered_by_every_promised_surface() {
    let root = workspace_root();
    let addon_config_path = root.join("addons/vigil/config.yaml");
    let addon_config = fs::read_to_string(&addon_config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", addon_config_path.display()));

    let entries = vigil::declared_settings();
    let mut observed = ObservedSurfaces::default();

    for entry in &entries {
        let probe_value = probe_value_for(entry.name);
        let claim_id = documentation_claim_id_for(entry.name);
        let doc_path = root.join(entry.surfaces.documentation_page);
        let doc_text = fs::read_to_string(&doc_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", doc_path.display()));
        observe_entry_surfaces(
            entry,
            &addon_config,
            &doc_text,
            claim_id,
            probe_value,
            &mut observed,
        );
    }

    let gaps = check_coverage(&entries, &observed);
    assert!(
        gaps.is_empty(),
        "every declared setting must appear on its config, add-on, and documentation surface: {gaps:?}"
    );
}

// ---------------------------------------------------------------------
// PARSE-LEVEL proof that the PRODUCTION observation path itself notices an
// absence, not just that `check_coverage`'s pure logic does (the settings.rs
// unit tests already prove the pure logic bites, with fixture entries and
// fixture surfaces). Each canary here reads the SAME real files the test
// above reads, strips the pilot setting's own key/name out of that content
// IN MEMORY, and pushes the mutated content through `observe_entry_surfaces`
// — the SAME function the real test calls, not a second copy of its logic.
// This proves the parsing/matching is wired correctly — it does NOT prove
// the reader opens the right file on disk, because the content is supplied
// in-process either way; a reader that silently swallowed a read error and
// fell back to empty content would look identical to a correct one here.
// That FILE-LEVEL property (does the add-on check really read
// `addons/vigil/config.yaml`, does the doc check really read the page the
// declaration names) is proven separately, on disk, in a throwaway
// worktree: the pilot setting's key was actually deleted from the real
// `addons/vigil/config.yaml`, the real test was run unmodified, and it
// failed exactly on the add-on-options surface — banked as evidence, not a
// repo test (a disk mutation has no fixture to assert against in-process,
// so there is nothing durable to keep here beyond the recorded outcome).
// ---------------------------------------------------------------------

#[test]
fn observation_path_notices_the_pilot_setting_missing_from_the_addon_options_surface() {
    let root = workspace_root();
    let addon_config_path = root.join("addons/vigil/config.yaml");
    let addon_config = fs::read_to_string(&addon_config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", addon_config_path.display()));
    assert!(
        addon_config.contains(PILOT_SETTING_NAME),
        "canary host file must really carry the pilot setting's key before it is stripped"
    );
    let stripped_addon_config = addon_config.replace(PILOT_SETTING_NAME, "");

    let entries = vigil::declared_settings();
    let entry = pilot_entry(&entries);
    let probe_value = probe_value_for(entry.name);
    let claim_id = documentation_claim_id_for(entry.name);
    let doc_path = root.join(entry.surfaces.documentation_page);
    let doc_text = fs::read_to_string(&doc_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", doc_path.display()));

    let mut observed = ObservedSurfaces::default();
    observe_entry_surfaces(
        &entry,
        &stripped_addon_config,
        &doc_text,
        claim_id,
        probe_value,
        &mut observed,
    );

    let gaps = check_coverage(&[entry], &observed);
    assert_eq!(
        gaps,
        vec![CoverageGap {
            setting_name: PILOT_SETTING_NAME,
            missing_surface: SurfaceKind::AddonOption,
        }],
        "stripping the pilot setting's key from the real add-on config in memory must be the \
         ONLY gap the real observation path reports: {gaps:?}"
    );

    // The pilot setting's `name` and `addon_option_key` happen to be equal
    // (both "detector_stationary_interval_secs"), so the assertion above
    // alone cannot distinguish "the observation keys on the declared
    // add-on key" from "it keys on the name instead" — stripping one
    // strips the other identically either way. This closes that gap with a
    // SYNTHETIC entry whose `name` and `addon_option_key` deliberately
    // differ, fed content that contains the NAME but not the KEY: if the
    // observation ever regressed to searching for the name, this would
    // wrongly report the surface as covered.
    let differing_entry = SettingCoverageEntry {
        name: "synthetic_pilot_name_distinct_from_its_key",
        surfaces: SettingSurfaces {
            config_key: entry.surfaces.config_key,
            addon_option_key: "synthetic_pilot_addon_option_key_distinct_from_its_name",
            documentation_page: entry.surfaces.documentation_page,
        },
    };
    let mut differing_observed = ObservedSurfaces::default();
    observe_entry_surfaces(
        &differing_entry,
        "synthetic_pilot_name_distinct_from_its_key: true\n",
        &doc_text,
        claim_id,
        probe_value,
        &mut differing_observed,
    );
    assert!(
        !differing_observed
            .addon_option_keys
            .contains(differing_entry.surfaces.addon_option_key),
        "content containing only the setting's NAME (never its declared add-on key) must not be \
         observed as covering the add-on-options surface — the observation must follow the key"
    );
    let mut matching_observed = ObservedSurfaces::default();
    observe_entry_surfaces(
        &differing_entry,
        "synthetic_pilot_addon_option_key_distinct_from_its_name: true\n",
        &doc_text,
        claim_id,
        probe_value,
        &mut matching_observed,
    );
    assert!(
        matching_observed
            .addon_option_keys
            .contains(differing_entry.surfaces.addon_option_key),
        "sanity: content containing the declared add-on key itself must be observed as covering \
         the surface"
    );
}

#[test]
fn observation_path_notices_the_pilot_setting_missing_from_the_documentation_surface() {
    let root = workspace_root();
    let addon_config_path = root.join("addons/vigil/config.yaml");
    let addon_config = fs::read_to_string(&addon_config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", addon_config_path.display()));

    let entries = vigil::declared_settings();
    let entry = pilot_entry(&entries);
    let probe_value = probe_value_for(entry.name);
    let claim_id = documentation_claim_id_for(entry.name);

    let doc_path = root.join(entry.surfaces.documentation_page);
    let doc_text = fs::read_to_string(&doc_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", doc_path.display()));
    assert!(
        doc_text.contains(PILOT_SETTING_NAME),
        "canary host page must really carry the pilot setting's name before it is stripped"
    );
    let stripped_doc_text = doc_text.replace(PILOT_SETTING_NAME, "");

    let mut observed = ObservedSurfaces::default();
    observe_entry_surfaces(
        &entry,
        &addon_config,
        &stripped_doc_text,
        claim_id,
        probe_value,
        &mut observed,
    );

    let gaps = check_coverage(&[entry], &observed);
    assert_eq!(
        gaps,
        vec![CoverageGap {
            setting_name: PILOT_SETTING_NAME,
            missing_surface: SurfaceKind::Documentation,
        }],
        "stripping the pilot setting's name from the real documentation page in memory must be \
         the ONLY gap the real observation path reports: {gaps:?}"
    );
}

/// Proves the documentation surface is bound to the setting's own
/// contract-bearing paragraph, not the whole page: the real
/// `docs/configuration.md` names `detector_stationary_interval_secs` a
/// SECOND time, outside its bound paragraph, in the prose sentence
/// listing which settings the Home Assistant add-on schema currently
/// exposes. A page-wide substring search would still see that incidental
/// mention and wrongly report the surface covered even after the actual
/// documenting paragraph loses the setting's name — the exact hazard this
/// binding closes.
#[test]
fn observation_path_is_not_fooled_by_an_incidental_mention_outside_the_bound_paragraph() {
    let root = workspace_root();
    let addon_config_path = root.join("addons/vigil/config.yaml");
    let addon_config = fs::read_to_string(&addon_config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", addon_config_path.display()));

    let entries = vigil::declared_settings();
    let entry = pilot_entry(&entries);
    let probe_value = probe_value_for(entry.name);
    let claim_id = documentation_claim_id_for(entry.name);

    let doc_path = root.join(entry.surfaces.documentation_page);
    let doc_text = fs::read_to_string(&doc_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", doc_path.display()));

    let lines: Vec<&str> = doc_text.lines().collect();
    let marker = format!("<!-- vigil-claim: `{claim_id}` -->");
    let tag_start = lines
        .iter()
        .position(|line| line.trim() == marker)
        .unwrap_or_else(|| panic!("expected claim {claim_id} to be present in the real doc"));
    let bound_paragraph = xtask::test_estate::adjacent_paragraph(&lines, tag_start)
        .unwrap_or_else(|| panic!("expected a paragraph bound to {claim_id}"));
    assert!(
        bound_paragraph.contains(PILOT_SETTING_NAME),
        "sanity: the bound paragraph must really carry the pilot setting's name"
    );
    let total_occurrences = doc_text.matches(PILOT_SETTING_NAME).count();
    let occurrences_in_bound_paragraph = bound_paragraph.matches(PILOT_SETTING_NAME).count();
    assert!(
        total_occurrences > occurrences_in_bound_paragraph,
        "canary host page must carry an incidental mention of the pilot setting's name OUTSIDE \
         its bound paragraph before this canary means anything real: found {total_occurrences} \
         total, {occurrences_in_bound_paragraph} inside the bound paragraph"
    );

    let mutated_doc_text = doc_text_with_name_stripped_from_bound_paragraph_only(
        &doc_text,
        claim_id,
        PILOT_SETTING_NAME,
    );
    assert!(
        mutated_doc_text.contains(PILOT_SETTING_NAME),
        "the incidental mention outside the bound paragraph must survive this mutation, or this \
         canary is not isolating what it claims to"
    );

    let mut observed = ObservedSurfaces::default();
    observe_entry_surfaces(
        &entry,
        &addon_config,
        &mutated_doc_text,
        claim_id,
        probe_value,
        &mut observed,
    );

    let gaps = check_coverage(&[entry], &observed);
    assert_eq!(
        gaps,
        vec![CoverageGap {
            setting_name: PILOT_SETTING_NAME,
            missing_surface: SurfaceKind::Documentation,
        }],
        "stripping the pilot setting's name from ONLY its bound contract paragraph — while an \
         incidental mention survives elsewhere on the same real page — must still report a \
         documentation gap: {gaps:?}"
    );
}

/// The config-file surface has no real file to strip in memory — its
/// production observation (`vigil::config_file_recognizes_setting`, called
/// from inside `observe_entry_surfaces`) already builds and parses a
/// synthetic fragment rather than reading anything off disk, so there is no
/// "does it open the right file" question for this surface at all, unlike
/// the two above. What CAN be proven at this surface's own level is that
/// the real dispatch function genuinely distinguishes the pilot setting's
/// own key from one it does not recognize, rather than a fixture standing
/// in for it — done here by mutating only the entry's OWN `config_key`
/// field (the input `observe_entry_surfaces` actually reads), leaving its
/// `addon_option_key` and `documentation_page` untouched, so the other two
/// surfaces still observe normally through the real files and the real
/// entry's own name.
#[test]
fn observation_path_notices_a_config_key_the_real_dispatch_does_not_recognize() {
    let root = workspace_root();
    let addon_config_path = root.join("addons/vigil/config.yaml");
    let addon_config = fs::read_to_string(&addon_config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", addon_config_path.display()));

    let entries = vigil::declared_settings();
    let real_entry = pilot_entry(&entries);
    let probe_value = probe_value_for(real_entry.name);
    let claim_id = documentation_claim_id_for(real_entry.name);
    let doc_path = root.join(real_entry.surfaces.documentation_page);
    let doc_text = fs::read_to_string(&doc_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", doc_path.display()));

    assert!(
        vigil::config_file_recognizes_setting(real_entry.surfaces.config_key, probe_value),
        "sanity: the real dispatch must recognize the pilot setting's own declared key"
    );

    // A key the real dispatch has never been taught — proves the
    // production function itself gates on name, not just the fixture
    // `check_coverage` tests in settings.rs. `name`, `addon_option_key`,
    // and `documentation_page` stay real; only `config_key` is a decoy.
    let mut decoy_entry = real_entry;
    decoy_entry.surfaces.config_key = "__not_a_declared_setting";

    let mut observed = ObservedSurfaces::default();
    observe_entry_surfaces(
        &decoy_entry,
        &addon_config,
        &doc_text,
        claim_id,
        probe_value,
        &mut observed,
    );

    let gaps = check_coverage(&[real_entry], &observed);
    assert_eq!(
        gaps,
        vec![CoverageGap {
            setting_name: PILOT_SETTING_NAME,
            missing_surface: SurfaceKind::ConfigFile,
        }],
        "the real dispatch queried with a name it does not recognize must be the ONLY gap: \
         {gaps:?}"
    );
}
