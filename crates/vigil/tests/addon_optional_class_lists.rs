//! The two class lists an operator never has to author.
//!
//! A person installs the add-on, leaves the behavior controls untouched, and
//! starts it. That only works if the packaged manifest declares both class
//! lists as genuinely optional AND ships no value for either: an omitted list
//! means Vigil owns the choice, a present one means a person made it, and a
//! packaged empty list destroys the difference between those two forever.
//!
//! Both halves are read off the shipped manifest itself, never off a copy of
//! it kept beside these assertions, so the file an installation actually
//! validates against is the file under test.

use std::fs;
use std::path::{Path, PathBuf};

const ADDON_CONFIG_PATH: &str = "addons/vigil/config.yaml";

/// The two lists this file is about: what the machine looks for, and what
/// recognition puts a name to.
const OPTIONAL_CLASS_LISTS: [&str; 2] = ["detector_classes", "recognition_covered_classes"];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn addon_config_text() -> String {
    let path = repo_root().join(ADDON_CONFIG_PATH);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Every line belonging to one top-level block of the manifest, comments and
/// blank lines dropped, indentation kept — enough to tell a key apart from the
/// element lines declared beneath it.
fn block_lines(text: &str, block: &str) -> Vec<String> {
    let mut inside = false;
    let mut lines = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("");
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            inside = line.trim().trim_end_matches(':') == block && line.trim().ends_with(':');
            continue;
        }
        if inside {
            lines.push(line.to_string());
        }
    }
    lines
}

/// The element declarations nested under one key of a block, in order.
fn nested_declarations(text: &str, block: &str, key: &str) -> Vec<String> {
    let mut found = false;
    let mut declarations = Vec::new();
    for line in block_lines(text, block) {
        let trimmed = line.trim();
        let is_top_key =
            line.starts_with("  ") && !line.starts_with("   ") && !trimmed.starts_with("- ");
        if is_top_key {
            found = trimmed.split_once(':').map(|(name, _)| name.trim() == key) == Some(true);
            continue;
        }
        if found && trimmed.starts_with("- ") {
            declarations.push(
                trimmed
                    .trim_start_matches("- ")
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        }
    }
    declarations
}

/// Whether one block names a key at all, in any shape — scalar, inline, or
/// with elements nested beneath it.
fn block_names_key(text: &str, block: &str, key: &str) -> bool {
    block_lines(text, block).into_iter().any(|line| {
        line.starts_with("  ")
            && !line.starts_with("   ")
            && line
                .trim()
                .split_once(':')
                .map(|(name, _)| name.trim() == key)
                == Some(true)
    })
}

#[test]
fn both_class_lists_declare_the_optional_list_element_grammar() {
    // Unfakeable because it reads each list's own element declaration out of
    // the shipped manifest: a list whose element is declared `str` is a list
    // every write must carry, which is what refuses a never-before-used
    // installation that carries neither. The optional marker on the element is
    // the platform's own spelling for "this list may be absent", so nothing
    // short of that declaration satisfies it.
    let text = addon_config_text();
    for key in OPTIONAL_CLASS_LISTS {
        let declarations = nested_declarations(&text, "schema", key);
        assert!(
            !declarations.is_empty(),
            "the schema must still declare {key} as a list of strings"
        );
        assert!(
            declarations.iter().all(|declaration| declaration == "str?"),
            "{key} must declare its element with the optional-list grammar `- str?`, so an \
             installation that never authored the list still validates; got {declarations:?}"
        );
    }
}

#[test]
fn neither_class_list_ships_a_packaged_value() {
    // Unfakeable in the direction the fix is most likely to be taken: an
    // operator-visible empty list would silence the validation refusal while
    // permanently erasing the difference between "nobody set this" and "a
    // person chose to look for nothing". So the key must be absent from the
    // packaged record entirely — an empty list is a value, and this fails on
    // it exactly as it fails on a populated one.
    let text = addon_config_text();
    for key in OPTIONAL_CLASS_LISTS {
        assert!(
            !block_names_key(&text, "options", key),
            "{key} must not appear in the packaged options record at all — a packaged value, \
             empty list included, is indistinguishable from one a person authored"
        );
    }
}

#[test]
fn an_installation_that_authored_neither_list_carries_no_required_key() {
    // The two halves above are each satisfiable alone in a way that still
    // refuses the install this test is about: an optional declaration with a
    // packaged empty value, or an absent value under a required declaration.
    // This asserts the pair together, in the shape a fresh installation
    // actually presents — a record naming only the keys the manifest ships,
    // validated against a schema that must accept it.
    let text = addon_config_text();
    let packaged: Vec<String> = block_lines(&text, "options")
        .into_iter()
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .filter_map(|line| {
            line.trim()
                .split_once(':')
                .map(|(name, _)| name.trim().to_string())
        })
        .collect();

    for key in OPTIONAL_CLASS_LISTS {
        assert!(
            !packaged.contains(&key.to_string()),
            "a fresh installation's record must not carry {key}"
        );
        let declarations = nested_declarations(&text, "schema", key);
        assert!(
            declarations
                .iter()
                .all(|declaration| declaration.ends_with('?')),
            "a record that omits {key} can only validate if every element declaration under it \
             is optional; got {declarations:?}"
        );
    }
}
