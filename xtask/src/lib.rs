// The library half of the xtask crate: the only part any OTHER crate may
// depend on (the `xtask` binary itself is release/CI tooling, never a
// dependency). Today this exists for exactly one reason — so a `vigil`
// guard test that needs the SAME doc-claim paragraph-boundary parsing the
// test-estate registry already freezes a hash of can call that parsing
// directly, rather than re-implement it a second time and drift from it
// (see `test_estate::adjacent_paragraph` / `adjacent_paragraph_line_range`,
// consumed by `crates/vigil/tests/settings_surface_coverage.rs`).

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
