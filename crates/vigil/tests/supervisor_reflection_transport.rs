//! Reflection actually reaching the Supervisor.
//!
//! The promise is that the page a Home Assistant user trusts stops lying the
//! moment a value lands: a pushed or locally-set value is mirrored into the
//! add-on options, from inside Vigil's own container and with the token that
//! container was issued. Today the production client refuses every one of its
//! three operations, nothing in a real run calls the reflection path at all,
//! and the record of what Vigil last wrote out is never persisted — so the
//! promise is decorative.
//!
//! These tests drive the PRODUCTION client against a local stand-in for the
//! Supervisor's own surface: a real HTTP server on loopback that answers the way
//! the Supervisor answers — options round-tripped faithfully, a write treated as
//! a full replace, a restart as its own separate act. The stand-in returns what
//! the real one returns; it never returns a shape rigged to make a client look
//! right.
//!
//! What the write endpoint is called is the implementation's to choose, so the
//! assertions are about the CONTRACT the measured Supervisor behavior fixes: the
//! container's own bearer token, a read before the write, a posted record
//! carrying every key rather than only the one that changed, and a restart that
//! happens only when the stored restart policy says so.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use vigil::settings_model::{Scope, ScopeTarget, SettingValue, Surface};
use vigil::settings_reflection::{
    ContainerSupervisorClient, EchoLedger, OptionsRecord, ReflectionFailure, ReflectionOutcome,
    SupervisorOptionsClient, TokenSource,
};
use vigil::settings_store::SettingsStore;

const NODE: &str = "node-a";
const TOKEN: &str = "the-token-this-container-was-issued";

/// The setting the reflection tests move. An ordinary declared behavior key
/// with an add-on schema target, so nothing here depends on a key the schema
/// does not carry.
const REFLECTED_SETTING: &str = "detector_sample_frames";

/// The setting the restart-policy test moves. The stored policy governs the
/// restart that makes the container's own options file agree, and that restart
/// exists so a value the process only reads at startup takes effect — so the
/// setting that demonstrates it has to be one of those. `detector_sample_frames`
/// above is brought into force live and is never restarted for.
const RESTART_GOVERNED_SETTING: &str = "decode_probe_deadline_secs";

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: NODE.to_string(),
        site: NODE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

// ── The stand-in for the Supervisor's own surface ──────────────────────────

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

impl RecordedRequest {
    /// The options a write carried, accepting either shape a client may post:
    /// the record itself, or the record wrapped under an `options` member. What
    /// is under test is that the COMPLETE record travelled, not which of the two
    /// envelopes carried it.
    fn posted_options(&self) -> Map<String, Value> {
        let parsed: Value = serde_json::from_str(&self.body).unwrap_or_else(|error| {
            panic!("a write must carry JSON; got {:?}: {error}", self.body)
        });
        let object = parsed
            .as_object()
            .unwrap_or_else(|| panic!("a write must carry a JSON object; got {parsed}"));
        match object.get("options").and_then(Value::as_object) {
            Some(wrapped) => wrapped.clone(),
            None => object.clone(),
        }
    }
}

struct SupervisorDouble {
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stored: Arc<Mutex<Map<String, Value>>>,
}

impl SupervisorDouble {
    /// Answers with `options` as the add-on's current record, accepts a write as
    /// a full replace of it, and accepts a restart as its own request.
    fn start(options: Map<String, Value>) -> Self {
        Self::start_with_write_status(options, 200)
    }

    /// The same, with the write rejected — the Supervisor being unreachable or
    /// refusing is a real failure mode and its own contract.
    fn start_rejecting_writes(options: Map<String, Value>) -> Self {
        Self::start_with_write_status(options, 502)
    }

    fn start_with_write_status(options: Map<String, Value>, write_status: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind the supervisor stand-in");
        let port = listener.local_addr().expect("stand-in address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stored = Arc::new(Mutex::new(options));
        let served_requests = Arc::clone(&requests);
        let served_stored = Arc::clone(&stored);
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                let Ok(stream) = connection else { break };
                serve_one(stream, &served_requests, &served_stored, write_status);
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
            stored,
        }
    }

    fn client(&self) -> ContainerSupervisorClient {
        ContainerSupervisorClient::for_endpoint(self.base_url.clone(), TOKEN)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("request log").clone()
    }

    fn writes(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.method != "GET" && !request.path.contains("restart"))
            .collect()
    }

    fn restarts(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.path.contains("restart"))
            .collect()
    }

    fn stored_options(&self) -> Map<String, Value> {
        self.stored.lock().expect("stored options").clone()
    }
}

fn serve_one(
    mut stream: TcpStream,
    requests: &Arc<Mutex<Vec<RecordedRequest>>>,
    stored: &Arc<Mutex<Map<String, Value>>>,
    write_status: u16,
) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone the connection"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }
    let body = String::from_utf8_lossy(&body).to_string();

    requests.lock().expect("request log").push(RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        authorization: headers.get("authorization").cloned(),
        body: body.clone(),
    });

    let restart = path.contains("restart");
    let response = if method == "GET" {
        let current = stored.lock().expect("stored options").clone();
        // The Supervisor renders the add-on's record inside its own envelope.
        // Both the record itself and an `options` member carry it, so a client
        // reading either finds the same content and neither shape is the one
        // this stand-in happens to reward.
        let mut data = current.clone();
        data.insert("options".to_string(), Value::Object(current));
        ok_response(&json!({"result": "ok", "data": data}))
    } else if restart {
        ok_response(&json!({"result": "ok", "data": {}}))
    } else if write_status == 200 {
        // A write is a full replace validated against the posted content alone.
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if let Some(object) = parsed.as_object() {
            let replacement = match object.get("options").and_then(Value::as_object) {
                Some(wrapped) => wrapped.clone(),
                None => object.clone(),
            };
            *stored.lock().expect("stored options") = replacement;
        }
        ok_response(&json!({"result": "ok", "data": {}}))
    } else {
        let body =
            json!({"result": "error", "message": "the supervisor refused the write"}).to_string();
        format!(
            "HTTP/1.1 {write_status} Bad Gateway\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn ok_response(body: &Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

/// What a Home Assistant install's record actually looks like: several keys the
/// user set, only one of which any single write is about.
fn a_users_record() -> Map<String, Value> {
    let mut options = Map::new();
    options.insert("site_name".to_string(), json!("home farm"));
    options.insert("camera_name".to_string(), json!("lower gate"));
    options.insert("rtsp_url".to_string(), json!("rtsp://127.0.0.1:1/analysis"));
    options.insert("store_path".to_string(), json!("/data/store.contextgraph"));
    options.insert("detector_sample_frames".to_string(), json!(7));
    options.insert("recognition_threshold".to_string(), json!(0.9));
    options
}

fn open_store(directory: &tempfile::TempDir) -> SettingsStore {
    SettingsStore::open(directory.path()).expect("open the node-side settings store")
}

// ── The production client against the Supervisor's own surface ─────────────

/// Unfakeable because the record the stand-in holds is not one the client could
/// invent: a read that returned an empty or made-up record fails on content, and
/// a client that never issued a request leaves the stand-in's log empty.
#[test]
fn the_production_client_reads_the_add_on_record_from_the_supervisor() {
    let supervisor = SupervisorDouble::start(a_users_record());
    let client = supervisor.client();

    let record = client
        .read_options()
        .expect("reading the add-on's own options is what a reflection write starts from");

    assert!(
        record
            .entries
            .iter()
            .any(|(key, value)| key == "site_name" && value == &SettingValue::text("home farm")),
        "the client reads the record the add-on actually holds: {record:?}"
    );
    let requests = supervisor.requests();
    assert_eq!(
        requests.len(),
        1,
        "one read, issued against the supervisor rather than answered from nothing: {requests:?}"
    );
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str()),
        "carrying the token this container was issued — a write that only succeeds with another \
         add-on's token proves nothing about this promise: {requests:?}"
    );
    assert_eq!(
        client.token_source(),
        TokenSource::OwnContainer,
        "and the client says so"
    );
}

/// Unfakeable because the record deliberately holds keys the write is not about:
/// a partial post is either rejected outright for dropping a required key or
/// silently erases the optional ones, so the surviving content is the whole
/// proof. The value that changed is asserted alongside, so a client that posted
/// the record back unchanged fails too.
#[test]
fn a_reflection_write_posts_the_complete_record_not_only_the_key_that_changed() {
    let supervisor = SupervisorDouble::start(a_users_record());
    let client = supervisor.client();
    let mut ledger = EchoLedger::default();

    let outcome = vigil::settings_reflection::reflect(
        &client,
        &mut ledger,
        REFLECTED_SETTING,
        &SettingValue::Int(2),
        false,
    )
    .expect("reflecting an ordinary declared value");
    assert!(
        !matches!(outcome, ReflectionOutcome::NotAchieved(_)),
        "the write has to succeed against a supervisor that accepts it: {outcome:?}"
    );

    let writes = supervisor.writes();
    assert_eq!(writes.len(), 1, "exactly one write: {writes:?}");
    let posted = writes[0].posted_options();
    for key in a_users_record().keys() {
        assert!(
            posted.contains_key(key),
            "every key the user set travels in the write, because writing options is a full \
             replace: {key} is missing from {posted:?}"
        );
    }
    assert_eq!(
        posted.get(REFLECTED_SETTING),
        Some(&json!(2)),
        "and the one key the reflection is about carries the new value: {posted:?}"
    );
    assert!(
        supervisor
            .requests()
            .iter()
            .position(|request| request.method == "GET")
            .is_some_and(|read| read == 0),
        "the write is preceded by a read, because a full replace that did not start from the \
         current record is data loss on settings nobody touched: {:?}",
        supervisor.requests()
    );
    assert_eq!(
        supervisor.stored_options().get(REFLECTED_SETTING),
        Some(&json!(2)),
        "and the page the user trusts now shows what runs"
    );
}

/// Unfakeable because the restart policy is stored, not passed: the same call is
/// made twice against two stores that differ only in that value, and the
/// stand-in's own request log is what says whether a restart was asked for. A
/// build that took the policy from its caller, or that always restarted, cannot
/// produce both answers.
#[test]
fn the_stored_restart_policy_decides_whether_a_mirror_is_followed_by_a_restart() {
    for (policy, expected_restarts) in [(false, 0usize), (true, 1usize)] {
        let directory = tempfile::tempdir().expect("temporary data directory");
        let store = open_store(&directory);
        store
            .set_local(
                vigil::settings_reflection::RESTART_ON_REFLECT_SETTING,
                Surface::VigilSettings,
                Scope::node(NODE),
                SettingValue::Bool(policy),
            )
            .expect("the restart policy is an ordinary setting an operator stores");

        let supervisor = SupervisorDouble::start(a_users_record());
        let client = supervisor.client();
        let mut ledger = EchoLedger::default();

        vigil::settings_reflection::reflect_effective(
            &store,
            &target(),
            &client,
            &mut ledger,
            RESTART_GOVERNED_SETTING,
            &SettingValue::Int(2),
        )
        .expect("reflecting through the stored restart policy");

        assert_eq!(
            supervisor.writes().len(),
            1,
            "the mirror happens either way — saving options never restarts the add-on and never \
             reaches the running container, so it costs nothing and is unconditional"
        );
        assert_eq!(
            supervisor.restarts().len(),
            expected_restarts,
            "with the stored policy at {policy}, the restart that makes the container's own \
             options file agree is asked for exactly {expected_restarts} time(s): {:?}",
            supervisor.requests()
        );
    }
}

/// Unfakeable because the change is made through the real `vigil settings`
/// answer path against a real store: nothing in the test writes the record or
/// calls reflection itself, so an unmirrored local change leaves the stand-in's
/// log empty.
#[test]
fn a_local_settings_change_mirrors_onto_the_add_on_options() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let supervisor = SupervisorDouble::start(a_users_record());
    let client = supervisor.client();

    let answer = vigil::settings_command::answer_with_reflection(
        directory.path(),
        &format!("set {REFLECTED_SETTING} 2"),
        &client,
    );
    assert!(
        !vigil::settings_command::answer_failed(&answer),
        "the local change lands: {answer}"
    );

    let writes = supervisor.writes();
    assert_eq!(
        writes.len(),
        1,
        "a local change reflects immediately, exactly as a pushed value does — the page the \
         operator trusts stops disagreeing with the store the moment they change it, and nothing \
         is interrupted: {:?}",
        supervisor.requests()
    );
    assert_eq!(
        writes[0].posted_options().get(REFLECTED_SETTING),
        Some(&json!(2)),
        "carrying what they set: {writes:?}"
    );
}

/// Unfakeable because the ledger is reopened from the deployment directory by a
/// second, independent handle: a ledger that only ever lived in the memory of
/// the process that wrote it comes back empty, and an empty ledger re-authors
/// the user's own options as if a human had typed them on every boot — the
/// runaway the ledger exists to prevent.
#[test]
fn what_was_written_out_survives_as_the_deployments_own_echo_record() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let supervisor = SupervisorDouble::start(a_users_record());
    let client = supervisor.client();

    let answer = vigil::settings_command::answer_with_reflection(
        directory.path(),
        &format!("set {REFLECTED_SETTING} 2"),
        &client,
    );
    assert!(
        !vigil::settings_command::answer_failed(&answer),
        "the local change lands: {answer}"
    );

    let reopened = EchoLedger::open(directory.path()).expect("reopen this deployment's ledger");
    let entry = reopened
        .entries()
        .iter()
        .find(|entry| entry.surface == Surface::AddonOptions && entry.setting == REFLECTED_SETTING)
        .unwrap_or_else(|| {
            panic!(
                "the deployment records what it last wrote out, so the next start can tell its own \
                 echo from a human edit; nothing was recorded: {:?}",
                reopened.entries()
            )
        });
    assert!(
        entry
            .last_write_out
            .entries
            .iter()
            .any(|(key, value)| key == REFLECTED_SETTING && value == &SettingValue::Int(2)),
        "carrying the complete posted content, not just the key that changed: {entry:?}"
    );
    assert_eq!(
        entry.last_write_out.entries.len(),
        a_users_record().len(),
        "which is the whole record, because a write is a full replace: {entry:?}"
    );
}

/// Unfakeable because the store is read after the failure: a build that treated
/// an unreachable supervisor as a reason to roll the value back would answer
/// with the old number here. A failed reflection is never a reason to discard,
/// alter, or un-apply the value.
#[test]
fn a_supervisor_that_refuses_the_write_never_un_applies_the_stored_value() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open_store(&directory);
    store
        .set_local(
            REFLECTED_SETTING,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Int(2),
        )
        .expect("the operator's value lands in the store first");

    let supervisor = SupervisorDouble::start_rejecting_writes(a_users_record());
    let client = supervisor.client();
    let mut ledger = EchoLedger::default();

    let outcome = vigil::settings_reflection::reflect_effective(
        &store,
        &target(),
        &client,
        &mut ledger,
        REFLECTED_SETTING,
        &SettingValue::Int(2),
    )
    .expect("a refused mirror is an outcome, not an error the caller has to handle");

    assert!(
        matches!(
            outcome,
            ReflectionOutcome::NotAchieved(ReflectionFailure::SupervisorRejected { .. })
        ),
        "the failure is reported with its reason rather than swallowed: {outcome:?}"
    );
    let effective = store
        .resolve(REFLECTED_SETTING, &target())
        .expect("resolve the value after the refused mirror");
    assert_eq!(
        effective.requested,
        SettingValue::Int(2),
        "and the value the operator set is still what this node asks for: the store is the \
         authority and mirroring is a courtesy to the surface: {effective:?}"
    );
}

/// The production client is what a deployment builds and what these tests
/// drive. Naming that here keeps a future "test client" from quietly becoming
/// the only thing that works while the shipped one still refuses.
#[test]
fn the_client_a_deployment_builds_is_the_client_these_proofs_drive() {
    let supervisor = SupervisorDouble::start(a_users_record());
    let client: Box<dyn SupervisorOptionsClient> = Box::new(supervisor.client());
    assert_eq!(client.token_source(), TokenSource::OwnContainer);
    assert!(
        client.read_options().is_ok(),
        "the shipped client reads its own add-on's options; a client whose every operation refuses \
         cannot deliver the reflection promise at all"
    );
    let record = OptionsRecord {
        entries: vec![(REFLECTED_SETTING.to_string(), SettingValue::Int(2))],
    };
    assert!(
        client.write_options(&record).is_ok(),
        "and writes them, from inside its own container with its own credentials"
    );
    assert!(
        client.restart_addon().is_ok(),
        "and asks for the restart that makes the container's own options file agree"
    );
}
