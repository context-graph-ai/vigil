use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

use context_graph::Store;

use crate::correction::CorrectionRequest;
use crate::ha_discovery::DiscoveryPayload;

// ── Public MQTT config ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MqttConfig {
    pub broker_host: String,
    pub broker_port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

// ── Subscriber handle exposed to callers (and tests) ──────────────────────

pub struct SubscriberHandle {
    /// Counts correction commands dropped due to channel overflow (wrong stub: always 0).
    pub overflow_count: Arc<AtomicUsize>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SubscriberHandle {
    pub fn shutdown_and_join(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

// ── Connect-intent predicate ───────────────────────────────────────────────

/// WRONG STUB: always returns true regardless of whether a broker is configured.
///
/// Correct behaviour: returns true only when `config.is_some()`.
///   - mqtt_gated_off_when_no_broker_configured → connect-intent is true when it must be false → FAIL
pub fn mqtt_connect_intent(config: Option<&MqttConfig>) -> bool {
    let _ = config; // wrong stub: ignores the config
    true
}

// ── Discovery publisher ────────────────────────────────────────────────────

/// WRONG STUB: returns Ok without connecting or publishing anything.
///
///   - discovery_published_to_real_broker_on_start → bounded receive returns None → FAIL
pub fn publish_discovery_to_broker(
    _config: &MqttConfig,
    _payloads: &[DiscoveryPayload],
) -> Result<(), String> {
    // wrong stub: no-op — does not connect, does not publish
    Ok(())
}

// ── Correction command subscriber ─────────────────────────────────────────

/// WRONG STUB: spawns a thread that should listen to the broker command topic and
/// forward parsed CorrectionRequests to `command_tx`, but:
///   1. Never actually connects to the broker.
///   2. Never sends on `command_tx`, so overflow_count is never incremented.
///
/// The caller is responsible for calling `record_correction` for each command it
/// receives on the receiver end of `command_tx`.
///
///   - correction_command_channel_overflow_is_loud → overflow_count stays 0 → FAIL
///   - correction_command_on_broker_lands_in_cg → tx never receives → correction not in cg → FAIL
///   - redelivered_correction_command_is_idempotent → tx never receives → nothing in cg → FAIL
///   - two_distinct_corrections_on_same_detection_both_land → tx never receives → nothing → FAIL
///   - malformed_correction_command_is_rejected_and_subscriber_survives → tx never receives → FAIL
pub fn spawn_correction_subscriber(
    config: MqttConfig,
    command_tx: mpsc::SyncSender<CorrectionRequest>,
    overflow_count: Arc<AtomicUsize>,
) -> SubscriberHandle {
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let _config_clone = config.clone();
    let thread = thread::spawn(move || {
        // wrong stub: never connects to the broker; the command_tx is never used
        // so the caller's receiver never gets a message and overflow_count stays 0
        let _ = command_tx; // dropped immediately — wrong stub ignores it
        loop {
            if shutdown_clone.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });

    SubscriberHandle {
        overflow_count,
        shutdown,
        thread: Some(thread),
    }
}

/// WRONG STUB: accepts a Store reference but never connects to the broker and
/// never calls `record_correction`.  A correct implementation connects to the
/// broker, receives correction commands from the command topic, and calls
/// `record_correction(store, cmd)` for each, writing the correction durably
/// to cg authority.
///
///   - correction_command_on_broker_lands_in_cg  → nothing in cg → FAIL
///   - redelivered_correction_command_is_idempotent → nothing in cg → FAIL
///   - two_distinct_corrections_on_same_detection_both_land → nothing in cg → FAIL
///   - operator_action_command_effects_action → ack not in cg → FAIL
///   - acknowledge_survives_reopen_and_restart_via_cg_authority → not in cg → FAIL
pub fn spawn_wired_correction_subscriber(
    config: MqttConfig,
    store: Arc<Store>,
    overflow_count: Arc<AtomicUsize>,
) -> SubscriberHandle {
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let overflow_clone = Arc::clone(&overflow_count);
    let thread = thread::spawn(move || {
        // wrong stub: never connects to the broker; store and overflow_count are unused
        let _ = config;
        let _ = store;
        let _ = overflow_clone;
        loop {
            if shutdown_clone.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    SubscriberHandle {
        overflow_count,
        shutdown,
        thread: Some(thread),
    }
}

/// WRONG STUB: publishes an outbound socket attempt in the detection-event publish path.
/// This is caught by correction_path_makes_no_outbound_network_beyond_broker.
///
///   - detection_publishes_event_to_real_broker → bounded receive returns None → FAIL
///   - correction_path_makes_no_outbound_network_beyond_broker → extra socket detected → FAIL
pub fn publish_detection_event(
    _config: &MqttConfig,
    _event_json: &str,
    _topic: &str,
) -> Result<(), String> {
    // wrong stub: no-op — does not publish detection events
    Ok(())
}

/// Publish availability last-will configuration during startup.
/// WRONG STUB: returns Ok without setting up any last-will.
///   - TH-18 → last-will unwired, availability stays online on kill → FAIL
pub fn publish_availability_online(
    _config: &MqttConfig,
    _availability_topic: &str,
) -> Result<(), String> {
    Ok(())
}
