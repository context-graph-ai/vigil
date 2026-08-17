//! The authority the add-on asks Home Assistant for.
//!
//! Writing its own options back is the whole of what Vigil needs the
//! Supervisor for, and the ordinary role an add-on is granted already permits
//! it — measured against a live Supervisor with this container's own token,
//! which answered a complete self-options write successfully. Asking for the
//! broader role reaches every installed add-on's managed state, not only
//! Vigil's, so the manifest asks for the ordinary one and the packaged
//! documentation stops telling operators the ordinary one cannot do this.

use std::fs;
use std::path::{Path, PathBuf};

const ADDON_CONFIG_PATH: &str = "addons/vigil/config.yaml";
const ADDON_README_PATH: &str = "addons/vigil/README.md";

/// The role an add-on holds without being granted anything extra, and the one
/// a self-options write was proven to succeed under.
const ORDINARY_ROLE: &str = "default";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn top_level_scalar(text: &str, key: &str) -> Option<String> {
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("");
        let trimmed = line.trim();
        if trimmed.is_empty() || line.starts_with(' ') {
            continue;
        }
        if let Some((candidate, value)) = trimmed.split_once(':')
            && candidate == key
        {
            return Some(
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_ascii_lowercase(),
            );
        }
    }
    None
}

#[test]
fn the_manifest_asks_for_the_ordinary_role_a_self_options_write_needs() {
    // Unfakeable because it reads the shipped manifest's own grant: the role
    // is what an installation actually asks an operator to approve, and the
    // measured self-write succeeded at the ordinary one, so any broader
    // request is authority the feature does not need.
    let text = read(ADDON_CONFIG_PATH);
    assert_eq!(
        top_level_scalar(&text, "hassio_role").as_deref(),
        Some(ORDINARY_ROLE),
        "the manifest must ask for the ordinary Supervisor role, which a complete \
         self-options write was measured to succeed under"
    );
}

#[test]
fn the_packaged_documentation_does_not_claim_the_ordinary_role_cannot_self_write() {
    // The manifest and the words beside it are one statement to an operator.
    // A manifest lowered to the ordinary role while the packaged text still
    // says that role cannot write the add-on's own options leaves the reader
    // holding two contradictory answers and no way to tell which is current.
    let text = read(ADDON_README_PATH).to_ascii_lowercase();
    for claim in [
        "cannot self-write",
        "cannot write an add-on's own options",
        "cannot write its own options",
    ] {
        assert!(
            !text.contains(claim),
            "the packaged documentation must not claim the ordinary role cannot write the \
             add-on's own options; a live write at that role succeeded. Found: {claim}"
        );
    }
    assert!(
        !text.contains("hassio_role: manager"),
        "the packaged documentation must not describe the broader role as the one this \
         add-on requests"
    );
    assert!(
        text.contains(ORDINARY_ROLE),
        "the packaged documentation must name the role the manifest actually asks for"
    );
}
