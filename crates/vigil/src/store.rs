use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use context_graph::{CgError, Embedder, EmbedderConfig, Store, StoreConfig};

pub(crate) struct OpenStore {
    pub(crate) handle: Store,
    pub(crate) path: PathBuf,
    pub(crate) created: bool,
    pub(crate) trace: String,
}

/// Open the store, registering the real vision embedder when recognition is
/// configured. A configured weights dir that fails to load is a LOUD startup
/// failure — recognition never degrades silently. Returns the embedder handle
/// for the hot path.
pub(crate) fn open_with_recognition(
    path: &Path,
    recognition: &crate::recognition::RecognitionConfig,
) -> Result<(OpenStore, Option<Arc<dyn Embedder>>), String> {
    if !recognition.enabled {
        return Ok((open(path)?, None));
    }
    let weights_dir = recognition
        .weights_dir
        .clone()
        .ok_or_else(|| "recognition enabled without a weights dir".to_string())?;
    let embedder: Arc<dyn Embedder> = Arc::new(
        cg_vision_embedder::SiglipVisionEmbedder::load(cg_vision_embedder::SiglipEmbedderConfig {
            weights_dir,
            embedding_space_id: recognition.embedding_space_id.clone(),
            model_name: "siglip".to_string(),
            model_version: "2-base".to_string(),
        })
        .map_err(|error| format!("recognition embedder failed to load: {error}"))?,
    );
    let created = !path.exists();
    let handle = crate::recognition::open_store_with_embedder(
        path,
        &recognition.embedding_space_id,
        embedder.clone(),
    )?;
    let trace = handle
        .last_query_trace()
        .map_err(|error| format!("could not read store trace: {error}"))?;
    Ok((
        OpenStore {
            path: handle.db_path().to_path_buf(),
            handle,
            created,
            trace,
        },
        Some(embedder),
    ))
}

/// Render a store-open failure as a CLI string, classifying a locked store by
/// the TYPED `CgError::StoreLocked` variant rather than by substring-matching
/// context-graph's Debug text (which context-graph deliberately stopped
/// carrying the engine's "database is locked … process" wording when it
/// introduced the typed variant). The busy-read path (`lib.rs`
/// `is_database_locked_error`) keys off "database is locked" + the holder pid,
/// so a locked store must surface both — recovered here from the typed fields,
/// never guessed from Debug shape.
fn open_error_message(error: CgError) -> String {
    match error {
        CgError::StoreLocked { holder_pid, path } => format!(
            "database is locked by another process (holder pid {holder_pid}) at {}",
            path.display()
        ),
        other => format!("could not open store through context-graph: {other}"),
    }
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
    let handle = Store::open(config).map_err(open_error_message)?;
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
