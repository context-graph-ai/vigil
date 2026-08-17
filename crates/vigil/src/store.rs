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

/// Why a startup open did not produce a store.
///
/// The two are different facts about different things, and a caller that
/// flattened them told an operator their store was unreadable when the store
/// was never opened at all: the component the runtime loads BEFORE it opens
/// anything is what failed.
pub(crate) enum StartupOpenFailure {
    /// The store itself could not be opened.
    Store(String),
    /// A component the runtime loads before the store failed to load. The
    /// store was never reached.
    Component {
        component: &'static str,
        detail: String,
    },
}

impl StartupOpenFailure {
    /// The failure as an operator reads it, cause named either way.
    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Store(detail) => detail.clone(),
            Self::Component { component, detail } => {
                format!("{component} failed to load: {detail}")
            }
        }
    }
}

/// Open the store, registering the real vision embedder when recognition is
/// configured. A configured weights dir that fails to load is a LOUD startup
/// failure — recognition never degrades silently. Returns the embedder handle
/// for the hot path.
pub(crate) fn open_with_recognition(
    path: &Path,
    recognition: &crate::recognition::RecognitionConfig,
) -> Result<(OpenStore, Option<Arc<dyn Embedder>>), StartupOpenFailure> {
    if !recognition.enabled {
        return Ok((open(path).map_err(StartupOpenFailure::Store)?, None));
    }
    let weights_dir =
        recognition
            .weights_dir
            .clone()
            .ok_or_else(|| StartupOpenFailure::Component {
                component: crate::settings_degraded::RECOGNITION_EMBEDDER_COMPONENT,
                detail: "recognition is on with no weights directory configured".to_string(),
            })?;
    let embedder: Arc<dyn Embedder> = Arc::new(
        cg_vision_embedder::SiglipVisionEmbedder::load(cg_vision_embedder::SiglipEmbedderConfig {
            weights_dir,
            embedding_space_id: recognition.embedding_space_id.clone(),
            model_name: "siglip".to_string(),
            model_version: "2-base".to_string(),
        })
        .map_err(|error| StartupOpenFailure::Component {
            component: crate::settings_degraded::RECOGNITION_EMBEDDER_COMPONENT,
            detail: error.to_string(),
        })?,
    );
    let created = !path.exists();
    let handle = crate::recognition::open_store_with_embedder(
        path,
        &recognition.embedding_space_id,
        embedder.clone(),
    )
    .map_err(StartupOpenFailure::Store)?;
    let trace = handle.last_query_trace().map_err(|error| {
        StartupOpenFailure::Store(format!("could not read store trace: {error}"))
    })?;
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

/// Render a store-open failure as a string, classifying a locked store by the
/// TYPED `CgError::StoreLocked` variant rather than by substring-matching
/// context-graph's Debug text (which context-graph deliberately stopped
/// carrying the engine's "database is locked … process" wording when it
/// introduced the typed variant, cg `dev` `99ea2d3`). Every vigil store-open
/// surface routes its error through this ONE classifier so the structured
/// locked message is restored everywhere, not just on the CLI read path — the
/// runtime startup open, the recognition-embedder open, and the CLI read all
/// share it. A locked store always surfaces "database is locked" + the holder
/// pid (recovered from the typed fields, never guessed from Debug shape); the
/// `context` label carries the surface-specific wording for every other error.
pub(crate) fn classify_store_open_error(context: &str, error: CgError) -> String {
    match error {
        CgError::StoreLocked { holder_pid, path } => format!(
            "database is locked by another process (holder pid {holder_pid}) at {}",
            path.display()
        ),
        other => format!("{context}: {other}"),
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
    let handle = Store::open(config).map_err(|error| {
        classify_store_open_error("could not open store through context-graph", error)
    })?;
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
