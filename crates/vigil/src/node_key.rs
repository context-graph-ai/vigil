//! The name this deployment records its settings under.
//!
//! Every settings record is keyed on `(setting, surface, scope level, scope
//! target)`, and the travelling tables resolve a collision by keeping the
//! latest write. So the scope target is the only place the key names WHICH
//! node wrote a row: two nodes that record under one target are one row to
//! every reader, and the newest write silently replaces the other node's.
//!
//! The deployment directory's own name cannot carry that. The Home Assistant
//! add-on installs with a fixed data directory on every node, so a target
//! derived from it is byte-identical across an entire fleet — and the first
//! record to collide would be this node's service identity, which names its
//! Home Assistant device.
//!
//! So the target is a key generated once, on this node, at the first start
//! that can persist anything, and then read back forever. It is generated
//! before anything is written to the store, and nothing about it is derived
//! from a value the store holds — including the service identity, whose own
//! record is written AT this key and therefore cannot be its source.
//!
//! The key is an identifier, not a name an operator reads or types: no
//! surface renders it, and nothing about a node's behavior changes with its
//! value.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Where the key lives inside a deployment directory. Under the data
/// directory, beside the store and the identity cache, because that is the one
/// path a deployment guarantees is writable.
pub fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("node-key")
}

/// The key already recorded for this deployment, if one is. Reads only: a
/// caller that must leave a never-started deployment exactly as it found it
/// asks this and takes its own fallback when the answer is `None`.
pub fn recorded(data_dir: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(key_path(data_dir)).ok()?;
    let key = contents.trim().to_string();
    if key.is_empty() { None } else { Some(key) }
}

/// The key this deployment records under, generating and persisting one if
/// this is the first start that ever could.
///
/// Generation is exclusive-create, so two processes starting at once cannot
/// each keep their own key: the one that loses the create reads back the one
/// that won.
///
/// A deployment directory that cannot be written falls back to the directory's
/// own name. That is the pre-existing behavior and it is safe exactly where it
/// happens: a deployment that cannot persist a key cannot persist a record
/// either, so there is nothing at that target for another node to overwrite.
pub fn scope_name(data_dir: &Path) -> String {
    if let Some(key) = recorded(data_dir) {
        return key;
    }
    let generated = uuid::Uuid::new_v4().to_string();
    let path = key_path(data_dir);
    let _ = std::fs::create_dir_all(data_dir);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            if file
                .write_all(format!("{generated}\n").as_bytes())
                .and_then(|()| file.sync_all())
                .is_ok()
            {
                return generated;
            }
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if let Some(key) = recorded(data_dir) {
                return key;
            }
        }
        Err(_) => {}
    }
    // Nothing could be persisted. Answer the way this deployment answered
    // before a key existed, so a read of an unwritable directory still
    // resolves rather than failing.
    fallback_name(data_dir)
}

/// The name a deployment that has no recorded key resolves at.
pub fn fallback_name(data_dir: &Path) -> String {
    data_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "vigil".to_string())
}
