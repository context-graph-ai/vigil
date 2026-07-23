//! Proves that Home Assistant discovery/transport vocabulary stays out of
//! core: `crates/vigil/src` may hold camera-domain facts and a neutral
//! integration seam, but never a Home Assistant discovery topic, config
//! topic, or entity/device concept that belongs to the adapter crate
//! (`vigil-ha`) instead.
//!
//! What this proves: none of [`HOME_ASSISTANT_NOUNS`] appears as code
//! (an identifier) or a string-literal value anywhere under
//! `crates/vigil/src`. What it does NOT prove: it says nothing about
//! `vigil-ha` itself (which legitimately carries every one of these terms),
//! and a noun outside this exact list can still leak undetected — the list
//! is reviewed, not exhaustive.
//!
//! Deliberately absent from the noun list: `entity_id`. That is a
//! context-graph ontology field name used legitimately throughout core's
//! recognition, correction, and live-read paths; including it would make
//! this guard fail on correct code.
//!
//! Comments are excluded from the scan (a doc comment explaining *why* core
//! has no Home Assistant concept, such as this file's own header, must not
//! trip the guard it documents); identifiers and string-literal contents
//! are scanned, via the shared lexer already used by
//! `cli_secret_flag_surface.rs`, `environment_read_surface.rs`, and
//! `settings_registry_declare_boundary.rs`.

use std::fs;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{crates_root, lex, rust_sources};

/// Home Assistant discovery/transport vocabulary. Every entry below is a
/// term this crate's Home Assistant MQTT discovery payloads or topic
/// naming actually used before the adapter split (see
/// `crates/vigil-ha/src/ha_discovery.rs`) — never a generic word that
/// could show up in unrelated, legitimate core code.
const HOME_ASSISTANT_NOUNS: &[&str] = &[
    "homeassistant/",
    "home_assistant",
    "ha_discovery",
    "ha_mqtt_tasks",
    "discovery_payload",
    "device_class",
    "default_entity_id",
    "via_device",
    "payload_available",
    "payload_not_available",
];

/// Every noun from `HOME_ASSISTANT_NOUNS` found in `source`, outside
/// comments — as code (an identifier or keyword substring) or inside a
/// string literal's decoded value.
fn scan_for_home_assistant_nouns(source: &str) -> Vec<&'static str> {
    let lexed = lex(source);
    let masked_code: String = lexed.masked.iter().collect::<String>().to_ascii_lowercase();
    let mut hits = Vec::new();
    for noun in HOME_ASSISTANT_NOUNS {
        let in_code = masked_code.contains(noun);
        let in_string_literal = lexed
            .strings
            .iter()
            .any(|(_, _, content)| content.to_ascii_lowercase().contains(noun));
        if in_code || in_string_literal {
            hits.push(*noun);
        }
    }
    hits
}

#[test]
fn core_sources_carry_no_home_assistant_vocabulary() {
    let src_root = crates_root().join("vigil").join("src");
    let mut violations = Vec::new();
    for path in rust_sources(&src_root) {
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        for noun in scan_for_home_assistant_nouns(&source) {
            violations.push(format!("{}: contains `{noun}`", path.display()));
        }
    }
    assert!(
        violations.is_empty(),
        "Home Assistant vocabulary leaked into core sources (belongs in the vigil-ha adapter crate instead):\n{}",
        violations.join("\n")
    );
}

#[test]
fn scan_detects_a_planted_leak_in_a_real_core_file() {
    // Unfakeable because the canary is planted into a REAL core source
    // file's actual text (not a synthetic string), through the same
    // read-scan path the guard above uses, proving the scan is not
    // vacuously passing over unread or empty content.
    let real_file = crates_root().join("vigil").join("src").join("health.rs");
    let mut source = fs::read_to_string(&real_file)
        .unwrap_or_else(|error| panic!("read {real_file:?}: {error}"));
    source.push_str("\n// planted-leak canary: homeassistant/sensor/canary/config\n");
    let hits = scan_for_home_assistant_nouns(&source);
    assert!(
        hits.is_empty(),
        "canary was planted inside a comment and must not be detected there, got {hits:?}"
    );

    let mut code_source = fs::read_to_string(&real_file)
        .unwrap_or_else(|error| panic!("read {real_file:?}: {error}"));
    code_source.push_str(
        "\nconst VIGIL_TEST_PLANTED_LEAK_CANARY: &str = \"homeassistant/sensor/canary/config\";\n",
    );
    let hits = scan_for_home_assistant_nouns(&code_source);
    assert!(
        hits.contains(&"homeassistant/"),
        "planted leak inside a real core file's live code must be detected, got {hits:?}"
    );
}
