use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::collections::BTreeSet;

use context_graph::owner_control::{OwnerReadCancellation, ReaderRelease};
use context_graph::{
    CgError, ConsumerDeclaration, ConsumerReader, ConsumerSchema, ConsumerTable, ControlHandler,
    Embedder, EmbedderConfig, Store, StoreConfig,
};

pub(crate) struct OpenStore {
    pub(crate) handle: Store,
    pub(crate) path: PathBuf,
    pub(crate) created: bool,
    pub(crate) trace: String,
}

/// What a startup open produced: the store, or a stop that arrived before it
/// could be taken.
///
/// The second is not a failure and must never be reported as one. Nothing is
/// wrong with the deployment — an operator asked this process to stop while it
/// was waiting for someone else's read to finish, and the only correct answer
/// to that is to stop.
pub(crate) enum StartupOpen {
    Opened(Box<OpenStore>, Option<Arc<dyn Embedder>>),
    ShutdownRequested,
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
    handler: ControlHandler,
    stop: &OwnerReadCancellation,
) -> Result<StartupOpen, StartupOpenFailure> {
    if !recognition.enabled {
        let created = !path.exists();
        prepare_store_parent(path).map_err(StartupOpenFailure::Store)?;
        let handle = match open_owned_waiting_for_readers(
            path,
            "could not open store through context-graph",
            stop,
            || attempt_store_open(path, Some(handler.clone())),
        )
        .map_err(StartupOpenFailure::Store)?
        {
            OwnedOpen::Opened(handle) => handle,
            OwnedOpen::ShutdownRequested => return Ok(StartupOpen::ShutdownRequested),
        };
        return Ok(StartupOpen::Opened(
            Box::new(finish_open(handle, created).map_err(StartupOpenFailure::Store)?),
            None,
        ));
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
    // ONE writable open carries both planes: the vision registration this
    // deployment recognizes through, and the handler its operator's commands
    // reach. A second open would be refused, and choosing between them would
    // cost the operator either recognition or the live command line.
    crate::recognition::prepare_recognition_store_parent(path)
        .map_err(StartupOpenFailure::Store)?;
    let handle = match open_owned_waiting_for_readers(
        path,
        "store open with vision embedder failed",
        stop,
        || {
            crate::recognition::attempt_recognition_store_open(
                path,
                &recognition.embedding_space_id,
                embedder.clone(),
                Some(handler.clone()),
            )
        },
    )
    .map_err(StartupOpenFailure::Store)?
    {
        OwnedOpen::Opened(handle) => handle,
        OwnedOpen::ShutdownRequested => return Ok(StartupOpen::ShutdownRequested),
    };
    let opened = finish_open(handle, created).map_err(StartupOpenFailure::Store)?;
    Ok(StartupOpen::Opened(Box::new(opened), Some(embedder)))
}

/// Render a store-open failure as a string, classifying a locked store by the
/// TYPED `CgError::StoreLocked` variant rather than by substring-matching
/// context-graph's Debug text (which context-graph deliberately stopped
/// carrying the engine's "database is locked … process" wording when it
/// introduced the typed variant, cg `dev` `99ea2d3`). Every vigil store-open
/// surface routes its error through this ONE classifier so the structured
/// locked message is restored everywhere, not just on the CLI read path — the
/// runtime startup open, the recognition-embedder open, and the CLI read all
/// share it. A locked store always surfaces "database is locked" — the phrase
/// every vigil surface classifies the condition by — plus the holder pid
/// whenever the layers below reported one (recovered from the typed fields,
/// never guessed from Debug shape). `holder_pid` is optional there, so a
/// holder that published no process record is rendered as exactly that: the
/// store is held, by a writer this layer cannot name. Nothing is invented for
/// the unknown case, because an operator sent after process zero kills the
/// wrong process. The `context` label carries the surface-specific wording for
/// every other error.
pub(crate) fn classify_store_open_error(context: &str, error: CgError) -> String {
    match error {
        CgError::StoreLocked {
            holder_pid: Some(holder_pid),
            path,
        } => format!(
            "database is locked by another process (holder pid {holder_pid}) at {}",
            path.display()
        ),
        CgError::StoreLocked {
            holder_pid: None,
            path,
        } => format!(
            "database is locked by another process, which published no process id, at {}",
            path.display()
        ),
        // Readers reading the store hold an open off only while they hydrate.
        // The store is healthy and the condition clears on its own, so the
        // rendering says who is holding it and never that the store failed to
        // open — the surfaces that classify by wording must not read a busy
        // moment as a storage fault.
        CgError::StoreHeldByReaders {
            observed_direct_readers,
            readers,
            path,
        } => crate::settings_projection::render_busy_with_readers(
            &path,
            observed_direct_readers,
            &readers,
        )
        .trim_end()
        .to_string(),
        other => format!("{context}: {other}"),
    }
}

/// Open the store and become the administrative owner of it: `handler` answers
/// every frame an operator's command sends at this store's path for as long as
/// the handle lives. There is no listener to start and no location to publish —
/// the channel is the store's own, attached at this one writable open.
///
/// What the open does with `handler` is context-graph's
/// `owner_control::owner_read_config`: it wraps the handler in the namespace
/// check and the ceilings that crate declares, and hands the result to the
/// engine as the writable open's owner-read configuration. Vigil supplies the
/// handler and nothing else — the namespace, the limits, the framing and every
/// refusal belong to the layers that own them, which is why there is no vigil
/// spelling of any of it to keep in step.
pub(crate) fn open_as_owner(path: &Path, handler: ControlHandler) -> Result<OpenStore, String> {
    open_inner(path, Some(handler))
}

pub(crate) fn open(path: &Path) -> Result<OpenStore, String> {
    open_inner(path, None)
}

/// Every Context Graph table the review reads have to reach to answer `vigil
/// why` and `vigil events` in full, and the whole of what this deployment
/// declares to read them.
///
/// The nested tables are here because the ANSWERS are nested: an observation
/// carries its evidence and each piece of evidence the retention status its
/// availability log last recorded, and a decision carries the intentions it
/// serves, the precedents it cites, its basis snapshots and its outcome. A
/// declaration naming only the headline table is told which table it still owes
/// rather than served a stripped row, so the list is the read set rather than a
/// convenience.
const REVIEW_READ_TABLES: &[&str] = &[
    // audit_query
    "audit_log",
    // get_context, list_contexts
    "contexts",
    // get_entity
    "entities",
    // get_intention
    "intentions",
    // get_decision, and what its answer carries
    "decisions",
    "edges",
    "decision_basis_snapshots",
    "outcomes",
    // get_observation, list_observations, and what their answers carry
    "observations",
    "evidence",
    "evidence_availability_log",
];

/// What a review read declares to Context Graph, built from context-graph's OWN
/// published specs.
///
/// Never a copied `CREATE TABLE` string. These are context-graph's tables, and
/// a copy of their text here would be a snapshot of a schema that goes on
/// changing upstream — the consumer door verifies a declaration against what
/// the store actually persisted and refuses a stale one by type, so a copy is a
/// drift bomb with a date on it rather than a shortcut.
fn review_read_declaration() -> ConsumerDeclaration {
    ConsumerDeclaration {
        schema: ConsumerSchema {
            namespace: crate::settings_store::VIGIL_TABLE_NAMESPACE.to_string(),
            tables: REVIEW_READ_TABLES
                .iter()
                .map(|wanted| {
                    let spec = context_graph::schema::TABLES
                        .iter()
                        .find(|spec| spec.name == *wanted)
                        .expect("context-graph publishes a spec for every table it owns");
                    ConsumerTable {
                        name: spec.name.to_string(),
                        ddl: spec.ddl.to_string(),
                    }
                })
                .collect(),
        },
        // No scope label, deliberately, for the reason APPENDIX 2 of this
        // round's brief settles for the settings reader and which holds here
        // too: a label on a reader is a request to be NARROWED, read narrowing
        // is the engine's `SCOPE_LABEL_READ` vocabulary, and a node that could
        // not see its own review history would be answering about a deployment
        // nobody has.
        scope_labels: BTreeSet::new(),
    }
}

/// Open this deployment's store for a REVIEW read — `vigil why`, `vigil events`
/// — creating nothing and taking no write lock.
///
/// One no-create read session over Context Graph's consumer reader, and the
/// typed graph reads ride it. Two operators asking at once are both served, the
/// store's bytes are the same after the answer as before it, and a store nobody
/// has ever started is refused by type rather than brought into existence to be
/// read. The refusal comes back already classified from the error THIS open
/// returned: two opens are two moments, a holder that lets go in between makes
/// the second one succeed, and the command would then answer about a store that
/// is not the one that refused it.
///
/// One condition is answered by opening the store WRITABLE, once, and it is the
/// only one: a deployment whose last runtime did not shut down cleanly leaves a
/// committed image the storage engine will not hand out until a writable
/// hydration has settled it. Nothing is damaged, the deployment is stopped so
/// nothing is contending for it, and the writable open is the only thing that
/// makes the store readable — so an operator whose node crashed still gets
/// their review history. The handle is taken and let go again, and the answer
/// is read through the reader as every other answer is, so two operators asking
/// at once still coexist afterwards. Every OTHER refusal stands as it is:
/// reaching for a writable handle on damage nobody has diagnosed is how a store
/// somebody could still have recovered by hand stops being recoverable at all.
pub(crate) fn open_for_review_read(path: &Path) -> Result<ConsumerReader, StoreOpenRefusal> {
    let refusal = match attempt_review_read(path) {
        Ok(reader) => return Ok(reader),
        Err(refusal) => refusal,
    };
    if !matches!(
        *refusal.settings_error,
        crate::settings_model::SettingsError::StoreNeedsWritableRecovery { .. }
    ) {
        return Err(refusal);
    }
    // Settle the image, then let the writable handle go: the answer itself is
    // read the way every review answer is read.
    drop(open_existing_for_edit(path)?);
    attempt_review_read(path)
}

fn attempt_review_read(path: &Path) -> Result<ConsumerReader, StoreOpenRefusal> {
    ConsumerReader::open(path, review_read_declaration()).map_err(|error| {
        let settings_error = crate::settings_store::map_consumer_open_error(path, error.clone());
        StoreOpenRefusal {
            class: crate::settings_degraded::classify_open_failure(&settings_error).unwrap_or(
                crate::settings_degraded::StoreOpenClass::Unreadable {
                    detail: settings_error.to_string(),
                },
            ),
            message: classify_store_open_error("could not read store through context-graph", error),
            settings_error: Box::new(settings_error),
        }
    })
}

/// Open this deployment's store for an EDIT — `vigil enroll`, `vigil forget` —
/// or refuse, creating nothing.
///
/// Context Graph's atomic open-existing-for-change door. An edit against a
/// deployment that has never started is refused by type rather than bringing
/// one into existence: a store a correction created is one no runtime ever
/// wrote, holding an enrollment at a path nobody chose, which the next `vigil
/// run` then opens as though a runtime had. It declares no consumer tables —
/// what an enrollment and a forget write are Context Graph's own — and the
/// refusal is classified from the error THIS open returned.
pub(crate) fn open_existing_for_edit(path: &Path) -> Result<Store, StoreOpenRefusal> {
    let config = StoreConfig {
        db_path: path.to_path_buf(),
        default_text_embedder: Some(EmbedderConfig::disabled()),
        ..StoreConfig::default()
    };
    Store::open_existing_for_change(config, Vec::new(), ConsumerDeclaration::default()).map_err(
        |error| {
            let settings_error =
                crate::settings_store::map_consumer_open_error(path, error.clone());
            StoreOpenRefusal {
                class: crate::settings_degraded::classify_open_failure(&settings_error).unwrap_or(
                    crate::settings_degraded::StoreOpenClass::Unreadable {
                        detail: settings_error.to_string(),
                    },
                ),
                message: classify_store_open_error(
                    "could not open store through context-graph",
                    error,
                ),
                settings_error: Box::new(settings_error),
            }
        },
    )
}

/// Why a store-open was refused, carried as a CLASSIFICATION rather than as
/// text a caller would have to interpret — plus the rendering, for the caller
/// that has nothing better to say than what happened.
pub(crate) struct StoreOpenRefusal {
    pub(crate) class: crate::settings_degraded::StoreOpenClass,
    pub(crate) message: String,
    /// The typed failure itself, kept beside the classification so a caller that
    /// has to tell a store which is NOT THERE from one that could not be read
    /// does it on the type rather than on the class — the two share the
    /// `Absent` class, and only one of them means "nothing has ever run here".
    ///
    /// Boxed because this value travels in the `Err` arm of every store open on
    /// this surface, and the typed failure carries a path, a reader roster and a
    /// refusal: leaving it inline widens the `Result` every caller returns for
    /// the sake of the case that almost never happens.
    pub(crate) settings_error: Box<crate::settings_model::SettingsError>,
}

fn open_inner(path: &Path, handler: Option<ControlHandler>) -> Result<OpenStore, String> {
    let created = !path.exists();
    prepare_store_parent(path)?;
    let handle = attempt_store_open(path, handler).map_err(|error| {
        classify_store_open_error("could not open store through context-graph", error)
    })?;
    finish_open(handle, created)
}

/// Create the store's parent directory, once, before any open attempt. Split
/// out because an open that has to be RETRIED must not redo the filesystem
/// work each time round.
fn prepare_store_parent(path: &Path) -> Result<(), String> {
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
    Ok(())
}

/// ONE attempt at the plain writable open, with context-graph's refusal still
/// typed so a caller that must wait one out can branch on the variant.
fn attempt_store_open(path: &Path, handler: Option<ControlHandler>) -> Result<Store, CgError> {
    let config = StoreConfig {
        db_path: path.to_path_buf(),
        default_text_embedder: Some(EmbedderConfig::disabled()),
        ..StoreConfig::default()
    };
    match handler {
        Some(handler) => Store::open_with_control_handler(config, handler),
        None => Store::open(config),
    }
}

/// The handle plus the two facts every caller of an open wants with it.
fn finish_open(handle: Store, created: bool) -> Result<OpenStore, String> {
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

/// The result of one owned open: the handle, or a stop that arrived first.
enum OwnedOpen {
    Opened(Store),
    ShutdownRequested,
}

/// The ending a stop gives this wait, said out loud wherever it is reached.
///
/// This is not a timeout: it gives the wait no outer limit and takes no view on
/// the store. Nothing here decides the store is unavailable and no unmanaged
/// run is started. The operator asked this process to stop, so it stops —
/// having taken nothing and written nothing.
fn abandoned_on_shutdown(path: &Path) -> OwnedOpen {
    println!(
        "store_open_abandoned_on_shutdown=true path={}",
        path.display()
    );
    OwnedOpen::ShutdownRequested
}

/// Open a store this process must OWN, retrying while direct readers are still
/// reading it.
///
/// The condition is momentary and self-clearing — the readers finish and the
/// next attempt takes the store — and the runtime retries for exactly as long
/// as the layer below keeps reporting it. Stopping on the first refusal is what
/// costs an operator their deployment: one `vigil stats` typed while Vigil is
/// coming up and the run goes unmanaged for the rest of its life, losing
/// recording, review history, corrections and settings changes, with nothing
/// done wrong.
///
/// The decision to retry is made on context-graph's typed
/// `CgError::StoreHeldByReaders` — never on the wording of a message, which is
/// the layer below's to change. Every other refusal is returned on the first
/// attempt exactly as before: a store held by a WRITER, a corrupt file, a
/// permission fault are none of them momentary, and retrying them would only
/// delay the operator's answer.
///
/// The wait itself is the KERNEL's, through the seam that owns it. Every direct
/// reader takes a SHARED advisory range hold on the companion beside the store
/// for exactly as long as it is LOADING the committed image — the one window a
/// would-be writer can collide with a reader at all — so context-graph's
/// `wait_for_reader_release` asks for that same hold EXCLUSIVELY, which is this
/// runtime's own question ("may I take this store") put to the only thing that
/// can answer it. The blocking acquisition sleeps in the kernel until the last
/// holder lets go, or dies, which the kernel treats as the same event: the wake
/// IS the release — not a moment later, and with no wakeups spent in between on
/// a box that may be running on a battery beside a camera. Vigil owns no
/// interval here: how long to pause before looking again was a number nobody
/// could defend, no operator could see, and it decided how promptly a
/// deployment came up.
///
/// Two things decide nothing in that verdict, and saying otherwise would
/// describe a weaker wait than this runtime actually gets. Reader breadcrumbs
/// are diagnosis — the notes that let an unobservable wait NAME who is holding
/// on, published in the default per-user runtime location — so a reader whose
/// breadcrumb never appeared is still a reader and still holds this wait, and a
/// diagnostic directory that cannot be read says nothing about whether the
/// store is free. And the owner channel's runtime root is not consulted at all:
/// the release verdict comes from the companion hold beside the store itself,
/// so a deployment whose channel root is elsewhere, unset or unreadable waits
/// exactly as correctly as any other.
///
/// The one thing that ends the wait early is the operator's own stop, and it is
/// read before the store on every attempt after the first refusal — a stop and
/// a reader letting go can land together, and a loop that looks at the store
/// first hands back a node that came up owning it after being stopped. The stop
/// also ends the kernel wait itself, through the token the signal cancels.
fn open_owned_waiting_for_readers(
    path: &Path,
    context: &str,
    stop: &OwnerReadCancellation,
    mut attempt: impl FnMut() -> Result<Store, CgError>,
) -> Result<OwnedOpen, String> {
    let mut waited = false;
    loop {
        // The stop is read BEFORE the store is, on every attempt after the
        // first refusal. A stop and a reader letting go can both land while
        // this is waiting; whichever this loop looks at first decides the
        // outcome, and looking at the store first means an operator who stopped
        // this node gets one that came up owning its store anyway.
        if waited && stop.is_cancelled() {
            return Ok(abandoned_on_shutdown(path));
        }
        match attempt() {
            Ok(handle) => {
                // And a stop that arrived while this attempt ran still wins
                // it: the handle is dropped rather than published, so startup
                // does not carry on past the operator's stop.
                if waited && stop.is_cancelled() {
                    drop(handle);
                    return Ok(abandoned_on_shutdown(path));
                }
                if waited {
                    println!("store_open_readers_cleared=true path={}", path.display());
                }
                return Ok(OwnedOpen::Opened(handle));
            }
            Err(CgError::StoreHeldByReaders { .. }) if stop.is_cancelled() => {
                return Ok(abandoned_on_shutdown(path));
            }
            Err(error @ CgError::StoreHeldByReaders { .. }) => {
                if !waited {
                    // Said once, and there is now only one turn to say it on:
                    // an operator wants to know who is holding their store, not
                    // to watch a counter. context-graph names the condition,
                    // the readers and the store, so this line adds the one fact
                    // it cannot know — that this runtime is waiting rather than
                    // giving up. It is also what a test waits on: the wait is
                    // observed by this line, never by a clock.
                    println!(
                        "store_open_waiting_for_readers=true path={} detail={error}",
                        path.display()
                    );
                    waited = true;
                }
                match context_graph::owner_control::wait_for_reader_release(path, stop) {
                    // The readers let go. Take the store.
                    ReaderRelease::Released => {}
                    ReaderRelease::Stopped => return Ok(abandoned_on_shutdown(path)),
                    // Nobody could see who is holding this store, so nothing
                    // here knows when to look again. One more attempt is made —
                    // the condition is momentary and may already have cleared —
                    // and if the store is still held, that is the answer the
                    // operator gets, naming what could not be observed. The
                    // alternative is a runtime looking again and again with
                    // nothing telling it when, which is the timer this replaced
                    // wearing a different name.
                    ReaderRelease::Unobservable(reason) => {
                        return match attempt() {
                            Ok(handle) => {
                                if stop.is_cancelled() {
                                    drop(handle);
                                    return Ok(abandoned_on_shutdown(path));
                                }
                                println!("store_open_readers_cleared=true path={}", path.display());
                                Ok(OwnedOpen::Opened(handle))
                            }
                            Err(CgError::StoreHeldByReaders { .. }) if stop.is_cancelled() => {
                                Ok(abandoned_on_shutdown(path))
                            }
                            Err(error) => Err(format!(
                                "{}: waiting for this store's readers to let go could not be \
                                 observed here ({reason}), so this runtime has nothing to wait on",
                                classify_store_open_error(context, error)
                            )),
                        };
                    }
                }
            }
            Err(error) => return Err(classify_store_open_error(context, error)),
        }
    }
}
