use std::fs;
use std::path::{Path, PathBuf};

use context_graph::{EmbedderConfig, Store, StoreConfig};

pub(crate) struct OpenStore {
    pub(crate) handle: Store,
    pub(crate) path: PathBuf,
    pub(crate) created: bool,
    pub(crate) trace: String,
}

pub(crate) fn open(path: &Path) -> Result<OpenStore, String> {
    let created = !path.exists();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create store parent {}: {error}",
                parent.display()
            )
        })?;
    }
    let config = StoreConfig {
        db_path: path.to_path_buf(),
        default_text_embedder: Some(EmbedderConfig::disabled()),
        ..StoreConfig::default()
    };
    let handle = Store::open(config)
        .map_err(|error| format!("could not open store through context-graph: {error}"))?;
    let trace = handle
        .last_query_trace()
        .map_err(|error| format!("could not read store trace: {error}"))?;
    Ok(OpenStore {
        path: handle.db_path().to_path_buf(),
        handle,
        created,
        trace,
    })
}
