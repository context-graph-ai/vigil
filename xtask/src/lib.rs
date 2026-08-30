// The library half of the xtask crate: the only part any OTHER crate may
// depend on (the `xtask` binary itself is release/CI tooling, never a
// dependency). Today this exists for exactly one reason — so a `vigil`
// guard test that needs the SAME doc-claim paragraph-boundary parsing the
// test-estate registry already freezes a hash of can call that parsing
// directly, rather than re-implement it a second time and drift from it
// (see `test_estate::adjacent_paragraph` / `adjacent_paragraph_line_range`,
// consumed by `crates/vigil/tests/settings_surface_coverage.rs`).

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub mod test_estate;

/// The workspace root, shared by every xtask subcommand — the library's
/// own test-estate checks and the binary's release/fixture tooling alike —
/// so there is one function computing it, not two copies that could
/// diverge.
pub fn repo_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "xtask has no workspace parent".to_string())
}

/// The Cargo build/target directory, resolved the way Cargo itself resolves
/// it: `CARGO_TARGET_DIR` when set (absolute, or taken relative to `root`
/// when it is not), falling back to `<root>/target`. Every xtask site that
/// stages into or reads out of the *actual Cargo build output directory*
/// (as opposed to a location xtask itself owns and always addresses via the
/// same literal, such as the CI closeout/archive-handoff staging paths)
/// must resolve it through this one helper rather than hardcoding
/// `root.join("target/...")` — a lane run with `CARGO_TARGET_DIR` pointed
/// elsewhere (see `.execution-lane/`) builds into that directory, not
/// `<root>/target`, and a hardcoded join silently looks in the wrong place.
// Reads CARGO_TARGET_DIR, which Cargo itself defines and resolves this way;
// it is Cargo's build contract, not an operator settings surface the
// settings-declaration guard covers.
#[allow(clippy::disallowed_methods)]
pub fn target_dir(root: &Path) -> PathBuf {
    resolve_target_dir(root, env::var_os("CARGO_TARGET_DIR"))
}

fn resolve_target_dir(root: &Path, cargo_target_dir: Option<OsString>) -> PathBuf {
    match cargo_target_dir {
        Some(value) if !value.is_empty() => {
            let candidate = PathBuf::from(value);
            if candidate.is_absolute() {
                candidate
            } else {
                root.join(candidate)
            }
        }
        _ => root.join("target"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_dir_falls_back_to_root_target_when_unset() {
        let root = Path::new("/workspace/vigil");
        assert_eq!(resolve_target_dir(root, None), root.join("target"));
        assert_eq!(
            resolve_target_dir(root, Some(OsString::new())),
            root.join("target"),
            "an empty CARGO_TARGET_DIR must not be treated as set"
        );
    }

    #[test]
    fn target_dir_honors_an_absolute_cargo_target_dir() {
        let root = Path::new("/workspace/vigil");
        assert_eq!(
            resolve_target_dir(root, Some(OsString::from("/lane/.execution-lane/target"))),
            PathBuf::from("/lane/.execution-lane/target"),
            "an absolute CARGO_TARGET_DIR must be used as-is, never joined onto root"
        );
    }

    #[test]
    fn target_dir_resolves_a_relative_cargo_target_dir_against_root() {
        let root = Path::new("/workspace/vigil");
        assert_eq!(
            resolve_target_dir(root, Some(OsString::from("build-out"))),
            root.join("build-out"),
            "Cargo resolves a relative CARGO_TARGET_DIR relative to the workspace root"
        );
    }
}
