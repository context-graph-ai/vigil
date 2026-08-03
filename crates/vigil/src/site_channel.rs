//! The seam between Vigil's own runtime and an integration adapter.
//!
//! Vigil's detection record and owner-command handling are both complete
//! without any implementation of the traits below. A composition root may
//! attach an adapter that publishes the same facts Vigil already tracks
//! through its own surfaces, and that forwards an owner command back in
//! through the same product action (`record_correction`) Vigil's own
//! surfaces use. Nothing here names a specific integration; an adapter
//! chooses its own transport, vocabulary, and wire shape.
//!
//! An adapter never holds a store handle, an enable/disable flag, or a data
//! directory — [`SiteControl`] is the one door onto Vigil's own camera state
//! and detection evidence, implemented by core and handed to the adapter as
//! a trait object.

use std::sync::Arc;

use crate::correction::{CorrectionRequest, read_latest_detection_image};
use crate::health::HealthState;
use crate::secret::Secret;

/// A remote endpoint Vigil may connect out to. Carries only what any network
/// client needs — no vocabulary from any particular integration.
///
/// `Debug` is hand-written, not derived: `password` is a secret on this
/// surface and must never print raw (the `Secret` wrapper handles that
/// itself), while `username` is a DIAGNOSTIC, not a secret (owner ruling)
/// and prints plainly — knowing which account a connection is configured
/// to use is exactly the kind of thing an operator needs from a log line.
/// Redacting a config type that merely WRAPS this one (e.g.
/// `RuntimeConfig.mqtt: Option<ConnectionEndpoint>`) would be defeated if
/// the inner type's own derive still printed the password raw — the
/// redaction has to live at the type that owns the field, not only at
/// every outer type that happens to embed it.
#[derive(Clone)]
pub struct ConnectionEndpoint {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<Secret>,
}

impl std::fmt::Debug for ConnectionEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionEndpoint")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password)
            .finish()
    }
}

/// One resolved camera, as an integration needs to announce it.
#[derive(Debug, Clone)]
pub struct CameraAnnouncement {
    pub id: String,
    pub label: String,
}

/// The resolved site facts an integration announces once, before listening
/// for owner commands.
#[derive(Debug, Clone)]
pub struct SiteAnnouncement {
    pub service_name: String,
    pub service_id: String,
    pub cameras: Vec<CameraAnnouncement>,
}

/// One detection fact, ready to leave Vigil through an integration. Carries
/// only fields Vigil itself already tracks — no transport or vocabulary
/// concept from any particular integration.
#[derive(Debug, Clone)]
pub struct DetectionFact {
    pub observation_id: String,
    pub camera_id: String,
    pub camera_name: String,
    pub object_class: String,
    pub confidence: f64,
    pub timestamp_ms: i64,
    pub evidence_ref: String,
    pub snapshot_ref: String,
    /// The recognized entity's name when the detection matched an enrolled
    /// subject; `None` keeps the fact an honest "unknown <class>".
    pub entity_name: Option<String>,
    pub match_score: Option<f64>,
}

/// Why a correction submission did not queue. Never a store or write
/// failure — the correction write itself always goes through
/// `record_correction`; this is purely about the bounded hand-off channel
/// between an adapter's listener thread and Vigil's own correction writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitCorrectionError {
    /// The bounded queue is full; the caller should count and log this, not
    /// treat it as Vigil rejecting the correction.
    QueueFull,
    /// The queue's reader is gone; the caller should stop submitting
    /// further corrections.
    Disconnected,
}

/// The actions and reads a site-integration adapter needs against Vigil's
/// own camera enable state and detection evidence. Vigil owns the actual
/// storage — the store, on-disk disable markers, per-camera enable flags —
/// implemented once by core and handed to an adapter as a trait object; an
/// adapter never holds any of that directly.
pub trait SiteControl: Send + Sync {
    /// The current enabled/disabled state of every camera Vigil knows
    /// about, as `(camera id, enabled)`, for an adapter's own initial or
    /// reconnect state publish.
    fn camera_enabled_states(&self) -> Vec<(String, bool)>;

    /// Enable or disable one camera by id. Returns `false` for an id Vigil
    /// does not recognize as one of its own cameras — a true no-op: an
    /// unrecognized id is never used to touch the filesystem.
    fn set_camera_enabled(&self, camera_id: &str, enabled: bool) -> bool;

    /// The most recent detection evidence frame for one camera, if any.
    fn latest_detection_image(&self, camera_id: &str) -> Option<Vec<u8>>;

    /// Submit an owner-issued correction through the same product action
    /// Vigil's own surfaces use (`record_correction`), via a bounded
    /// hand-off to Vigil's own correction-writer thread.
    fn submit_correction(&self, request: CorrectionRequest) -> Result<(), SubmitCorrectionError>;
}

/// An outbound channel to publish detection facts through. Built once at
/// boot (before any camera thread starts) and held for the runtime's
/// lifetime; cheaply cloneable via the `Arc` it is held behind.
pub trait DetectionChannel: Send + Sync {
    /// Publish one detection fact. A failure to deliver is the adapter's to
    /// log; it never drops or blocks Vigil's own recording of the
    /// detection.
    fn publish_detection(&self, event: DetectionFact);

    /// Signal the background connection to stop and block until it has
    /// joined. Safe to call once other holders of the same `Arc` (e.g. a
    /// camera thread) have already dropped their clones.
    fn shutdown_and_join(&self);
}

/// Listens for owner-issued commands (corrections, enable/disable, snapshot
/// requests) once the full camera roster is known, forwarding parsed
/// corrections into the channel it was given. Owns whatever background
/// connection it needs.
pub trait CommandListener: Send {
    /// Signal the listener to stop and block until its background work has
    /// joined.
    fn shutdown_and_join(self: Box<Self>);
}

/// Builds an integration's outbound channel and command listener from
/// Vigil's own resolved facts. Implemented once by an adapter; supplied by
/// the composition root that assembles the running binary.
pub trait SiteChannelFactory: Send + Sync {
    /// Connect the outbound detection channel, given the endpoint Vigil
    /// resolved from configuration and the stable service id used to
    /// namespace whatever the adapter publishes. Called once, before any
    /// camera thread starts.
    fn connect(
        &self,
        endpoint: &ConnectionEndpoint,
        service_id: &str,
        health: HealthState,
    ) -> Option<Arc<dyn DetectionChannel>>;

    /// Announce the resolved site and start listening for owner commands.
    /// `control` is the only door onto Vigil's own camera state and
    /// detection evidence; it carries no store handle, no flag, no path.
    /// Called once, after every camera's enable/disable flag has been
    /// resolved.
    fn listen(
        &self,
        endpoint: &ConnectionEndpoint,
        site: SiteAnnouncement,
        health: HealthState,
        control: Arc<dyn SiteControl>,
    ) -> Option<Box<dyn CommandListener>>;
}

/// The absence of any integration: never connects, never listens. An
/// explicit choice a caller passes to [`crate::run_cli_with_site_channel`]
/// — for example core's own tests and tooling, which never need a real
/// adapter wired in — never a default `run_cli_with_site_channel` supplies
/// on its own.
pub struct NoSiteChannel;

impl SiteChannelFactory for NoSiteChannel {
    fn connect(
        &self,
        _endpoint: &ConnectionEndpoint,
        _service_id: &str,
        _health: HealthState,
    ) -> Option<Arc<dyn DetectionChannel>> {
        None
    }

    fn listen(
        &self,
        _endpoint: &ConnectionEndpoint,
        _site: SiteAnnouncement,
        _health: HealthState,
        _control: Arc<dyn SiteControl>,
    ) -> Option<Box<dyn CommandListener>> {
        None
    }
}

/// Core's own implementation of [`SiteControl`], backed by the real store,
/// on-disk disable markers, and per-camera enable flags. Never exposed
/// outside core — an adapter only ever receives it as `Arc<dyn
/// SiteControl>`.
pub(crate) struct RuntimeSiteControl {
    pub(crate) store: context_graph::Store,
    pub(crate) data_dir: std::path::PathBuf,
    pub(crate) camera_flags: std::collections::BTreeMap<String, Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) commands: std::sync::mpsc::SyncSender<CorrectionRequest>,
}

impl SiteControl for RuntimeSiteControl {
    fn camera_enabled_states(&self) -> Vec<(String, bool)> {
        self.camera_flags
            .iter()
            .map(|(id, flag)| (id.clone(), flag.load(std::sync::atomic::Ordering::SeqCst)))
            .collect()
    }

    fn set_camera_enabled(&self, camera_id: &str, enabled: bool) -> bool {
        // Resolve first: an id Vigil does not recognize as one of its own
        // cameras never reaches the filesystem, so an untrusted caller
        // cannot use an unresolved id (including one containing a `..`
        // segment) to write or probe a path under `camera-disabled/`.
        let Some(flag) = self.camera_flags.get(camera_id) else {
            return false;
        };
        let disabled_dir = self.data_dir.join("camera-disabled");
        if enabled {
            let _ = std::fs::remove_file(disabled_dir.join(camera_id));
        } else {
            let _ = std::fs::create_dir_all(&disabled_dir);
            let _ = std::fs::write(disabled_dir.join(camera_id), b"");
        }
        flag.store(enabled, std::sync::atomic::Ordering::SeqCst);
        true
    }

    fn latest_detection_image(&self, camera_id: &str) -> Option<Vec<u8>> {
        read_latest_detection_image(&self.store, camera_id, &self.data_dir)
    }

    fn submit_correction(&self, request: CorrectionRequest) -> Result<(), SubmitCorrectionError> {
        self.commands
            .try_send(request)
            .map_err(|error| match error {
                std::sync::mpsc::TrySendError::Full(_) => SubmitCorrectionError::QueueFull,
                std::sync::mpsc::TrySendError::Disconnected(_) => {
                    SubmitCorrectionError::Disconnected
                }
            })
    }
}

/// Build a [`SiteControl`] backed by a real store, on-disk disable markers,
/// and per-camera enable flags. This is the one place `RuntimeSiteControl`
/// is constructed — `runtime.rs` is the only caller. Never exposed outside
/// core: an adapter or its tests get `SiteControl` only as the trait object
/// `runtime.rs` hands them, never by constructing this directly against a
/// real store — that is what a `SiteControl` test double is for.
pub(crate) fn store_backed_site_control(
    store: context_graph::Store,
    data_dir: std::path::PathBuf,
    camera_flags: std::collections::BTreeMap<String, Arc<std::sync::atomic::AtomicBool>>,
    commands: std::sync::mpsc::SyncSender<CorrectionRequest>,
) -> Arc<dyn SiteControl> {
    Arc::new(RuntimeSiteControl {
        store,
        data_dir,
        camera_flags,
        commands,
    })
}

#[cfg(test)]
mod tests {
    //! `SiteControl::set_camera_enabled` must resolve the camera id BEFORE
    //! touching disk. A command naming a camera id Vigil does not recognize
    //! (as could arrive from an untrusted integration payload) must be a
    //! true no-op: no directory created, no file written, no path
    //! constructed from the untrusted id at all — so a `..`-segment id
    //! cannot be used to write or probe outside `camera-disabled/`.
    //!
    //! Lives here (not as an integration test under `tests/`) because
    //! `store_backed_site_control` is `pub(crate)` — only reachable from
    //! inside this crate.

    use super::*;

    fn open_store_at(path: &std::path::Path) -> context_graph::Store {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create store parent dir");
        }
        context_graph::Store::open(context_graph::StoreConfig {
            db_path: path.to_path_buf(),
            default_text_embedder: Some(context_graph::EmbedderConfig::disabled()),
            ..context_graph::StoreConfig::default()
        })
        .expect("open store")
    }

    fn control_with_one_camera(
        data_dir: &std::path::Path,
    ) -> (
        Arc<dyn SiteControl>,
        std::sync::mpsc::Receiver<CorrectionRequest>,
    ) {
        let store = open_store_at(&data_dir.join("store.contextgraph"));
        let mut camera_flags = std::collections::BTreeMap::new();
        camera_flags.insert(
            "front-gate".to_string(),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        (
            store_backed_site_control(store, data_dir.to_path_buf(), camera_flags, tx),
            rx,
        )
    }

    #[test]
    fn unrecognized_camera_id_is_a_no_op_and_never_touches_disk() {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let (control, _rx) = control_with_one_camera(data_dir.path());

        let changed = control.set_camera_enabled("no-such-camera", false);

        assert!(
            !changed,
            "an id Vigil does not recognize must be reported as a no-op"
        );
        assert!(
            !data_dir.path().join("camera-disabled").exists(),
            "an unrecognized camera id must never cause `camera-disabled/` to be created"
        );
    }

    #[test]
    fn unrecognized_camera_id_containing_a_traversal_segment_never_escapes_the_disabled_dir() {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let (control, _rx) = control_with_one_camera(data_dir.path());

        let changed = control.set_camera_enabled("../escaped-marker", false);

        assert!(
            !changed,
            "a `..`-bearing unrecognized id must still be reported as a no-op"
        );
        assert!(
            !data_dir.path().join("camera-disabled").exists(),
            "a `..`-bearing unrecognized id must never cause any write under or \
             beside `camera-disabled/`"
        );
        assert!(
            !data_dir
                .path()
                .parent()
                .is_some_and(|parent| parent.join("escaped-marker").exists()),
            "a `..`-bearing unrecognized id must not escape the data directory"
        );
    }

    #[test]
    fn recognized_camera_id_still_writes_the_disabled_marker() {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let (control, _rx) = control_with_one_camera(data_dir.path());

        let changed = control.set_camera_enabled("front-gate", false);

        assert!(
            changed,
            "a camera Vigil owns must report the change as applied"
        );
        assert!(
            data_dir
                .path()
                .join("camera-disabled")
                .join("front-gate")
                .is_file(),
            "disabling a recognized camera must still write its disabled marker"
        );
    }
}
