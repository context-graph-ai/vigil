//! What a reflected change is allowed to touch.
//!
//! An operator changes one Vigil setting and is entitled to see exactly that
//! one setting change on the add-on options page. Everything else in the
//! record — above all the camera list, whose entries are structured objects
//! and not settings at all — must come back the shape it went out in, because
//! a write is a full replace and a record that arrives in the wrong shape is
//! refused outright, leaving the page frozen on a value that is no longer
//! true.
//!
//! When a write IS refused, the refusal has to say something an operator can
//! act on, and it must say it without printing camera credentials: the
//! refusal body carries the record that was rejected, and that record carries
//! camera URLs with passwords in them.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use vigil::settings_model::SettingValue;
use vigil::settings_reflection::{
    ContainerSupervisorClient, ReflectionFailure, SupervisorOptionsClient,
};

const TOKEN: &str = "the-token-this-container-was-issued";

/// An ordinary declared behavior key, so nothing here depends on a key the
/// packaged schema does not carry.
const REFLECTED_SETTING: &str = "detector_sample_frames";

/// The password an operator's camera record carries. Long and unmistakable so
/// a leak cannot be confused with any other text in a refusal.
const CAMERA_PASSWORD: &str = "camera-secret-pass-9f2c1d";

/// What a real installation's record looks like: a structured camera list
/// beside ordinary scalar settings.
///
/// The camera here carries its credential ONLY inside the address, which is
/// how the manifest lets an operator write one — the separate username and
/// password fields are optional, and a person who put the whole thing in the
/// address has set no password field at all. A fixture that also fills the
/// password field cannot tell a redaction that understands addresses from one
/// that only knows named fields: scrubbing the field value happens to scrub
/// the address too, and the leak stays invisible. So there is one secret and
/// it lives in one place.
fn a_record_with_a_real_camera() -> Map<String, Value> {
    let mut options = Map::new();
    options.insert("store_path".to_string(), json!("/data/store.contextgraph"));
    options.insert("site_name".to_string(), json!("home farm"));
    options.insert(REFLECTED_SETTING.to_string(), json!(7));
    options.insert(
        "cameras".to_string(),
        json!([{
            "name": "lower gate",
            "rtsp_url": format!("rtsp://operator:{CAMERA_PASSWORD}@127.0.0.1:1/analysis"),
        }]),
    );
    options
}

// ── A stand-in for the Supervisor's own surface ────────────────────────────

struct SupervisorDouble {
    base_url: String,
    stored: Arc<Mutex<Map<String, Value>>>,
    last_write_body: Arc<Mutex<Option<String>>>,
}

impl SupervisorDouble {
    fn start(options: Map<String, Value>, write_status: u16) -> Self {
        Self::start_with(options, 200, write_status)
    }

    /// The Supervisor refusing the READ. Its answer echoes the record it holds,
    /// which is where the camera address lives.
    fn start_refusing_reads(options: Map<String, Value>) -> Self {
        Self::start_with(options, 502, 200)
    }

    fn start_with(options: Map<String, Value>, read_status: u16, write_status: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind the stand-in");
        let port = listener.local_addr().expect("stand-in address").port();
        let stored = Arc::new(Mutex::new(options));
        let last_write_body = Arc::new(Mutex::new(None));
        let served_stored = Arc::clone(&stored);
        let served_body = Arc::clone(&last_write_body);
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                let Ok(stream) = connection else { break };
                serve_one(
                    stream,
                    &served_stored,
                    &served_body,
                    read_status,
                    write_status,
                );
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            stored,
            last_write_body,
        }
    }

    fn client(&self) -> ContainerSupervisorClient {
        ContainerSupervisorClient::for_endpoint(self.base_url.clone(), TOKEN)
    }

    fn stored_options(&self) -> Map<String, Value> {
        self.stored.lock().expect("stored options").clone()
    }

    fn last_write_body(&self) -> Option<String> {
        self.last_write_body.lock().expect("write body").clone()
    }
}

fn serve_one(
    mut stream: TcpStream,
    stored: &Arc<Mutex<Map<String, Value>>>,
    last_write_body: &Arc<Mutex<Option<String>>>,
    read_status: u16,
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
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
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

    let response = if method == "GET" {
        let current = stored.lock().expect("stored options").clone();
        if read_status == 200 {
            let mut data = current.clone();
            data.insert("options".to_string(), Value::Object(current));
            ok_response(&json!({"result": "ok", "data": data}))
        } else {
            // A refused read answers with its own explanation, and that
            // explanation quotes the record it was holding.
            refusal_response(
                read_status,
                &format!(
                    "unable to render the add-on record: {}",
                    Value::Object(current)
                ),
            )
        }
    } else if path.contains("restart") {
        ok_response(&json!({"result": "ok", "data": {}}))
    } else {
        *last_write_body.lock().expect("write body") = Some(body.clone());
        if write_status == 200 {
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
            // What a real refusal carries: the reason, quoting the record it
            // refused — camera addresses and all.
            refusal_response(
                write_status,
                &format!("expected dict for dictionary value @ data['cameras'][0]. Got {body}"),
            )
        }
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// The shape a Supervisor refusal arrives in: its own explanation, quoting
/// whatever it was looking at.
fn refusal_response(status: u16, message: &str) -> String {
    let body = json!({"result": "error", "message": message}).to_string();
    format!(
        "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn ok_response(body: &Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

// ── The contracts ──────────────────────────────────────────────────────────

#[test]
fn reflecting_one_setting_returns_the_camera_list_in_the_shape_it_arrived_in() {
    // Unfakeable because the camera object under test is the one the stand-in
    // held before the round trip, compared field for field afterwards: a
    // client that flattened it into text, dropped a field, or reordered it
    // into something else fails on content, and a client that never wrote
    // leaves the setting it was supposed to change at its old value.
    let supervisor = SupervisorDouble::start(a_record_with_a_real_camera(), 200);
    let client = supervisor.client();

    let before = supervisor.stored_options();
    let expected_cameras = before
        .get("cameras")
        .cloned()
        .expect("the record's cameras");

    let record = client.read_options().expect("read the add-on record");
    let updated = record.merge(REFLECTED_SETTING, &SettingValue::Int(9));
    client
        .write_options(&updated)
        .expect("the complete record must be acceptable to the Supervisor");

    let after = supervisor.stored_options();
    assert_eq!(
        after.get("cameras"),
        Some(&expected_cameras),
        "reflecting one setting must return the camera list byte-identical; a camera entry \
         serialized into text is a list of strings where the schema declares objects, which \
         is refused outright"
    );
    assert_eq!(
        after.get(REFLECTED_SETTING),
        Some(&json!(9)),
        "the one setting the operator changed must be the one that changed"
    );
    assert_eq!(
        after.get("site_name"),
        before.get("site_name"),
        "no unrelated setting may change shape or value"
    );
}

#[test]
fn a_posted_camera_entry_is_an_object_never_text() {
    // The half above proves the outcome; this proves the wire shape, so a
    // stand-in that happened to reassemble text back into an object could not
    // make the pair pass. The posted body is read directly.
    let supervisor = SupervisorDouble::start(a_record_with_a_real_camera(), 200);
    let client = supervisor.client();
    let record = client.read_options().expect("read the add-on record");
    let updated = record.merge(REFLECTED_SETTING, &SettingValue::Int(9));
    let _ = client.write_options(&updated);

    let body = supervisor
        .last_write_body()
        .expect("the client must have posted a record");
    let parsed: Value = serde_json::from_str(&body).expect("the posted record must be JSON");
    let options = parsed
        .get("options")
        .and_then(Value::as_object)
        .unwrap_or_else(|| parsed.as_object().expect("the posted record is an object"));
    let cameras = options
        .get("cameras")
        .and_then(Value::as_array)
        .expect("the posted record must still carry a camera list");
    let first = cameras.first().expect("the camera the operator configured");
    assert!(
        first.is_object(),
        "each posted camera entry must be an object, never text; got {first}"
    );
    assert_eq!(
        first.get("name"),
        Some(&json!("lower gate")),
        "the posted camera must keep its own fields"
    );
}

// ── What a camera address is allowed to look like once Vigil says it out loud ──
//
// A refusal has to stay useful: an operator needs to know which camera the
// Supervisor objected to, which means the address's scheme, host, port and path
// belong in the message. Nothing else does. Everything a camera address can
// carry BESIDES those — the userinfo before the `@`, and the whole of the query
// after the `?` — is where credentials live, and which part of a query is a
// credential is not knowable: `?usr=&pwd=` is one vendor's convention,
// `?user=&password=` another's, and a camera that names its secret `?k=` is
// doing nothing wrong. A list of credential-looking key names can only ever
// enumerate the conventions somebody has already seen, and the ones it has not
// seen leak in full.
//
// So the rule carries no judgement about which part is sensitive: a surfaced
// address is scheme, host, port and path, and the rest is gone. The stored
// record is untouched by any of this — the camera the operator wrote is written
// back exactly as it arrived, which the round-trip contracts above hold.

/// One address an operator can legitimately write, what a message about it may
/// still say, and the text that must never appear.
struct SurfacedAddress {
    what_it_is: &'static str,
    url: String,
    visible: String,
    hidden: Vec<String>,
}

/// The one address that carries every character a reduction might mistake for
/// the end of the credential. Named rather than positional so the multi-camera
/// contract below cannot quietly stop covering it when the list is reordered.
const MARKER_RICH_CREDENTIAL: &str =
    "an apostrophe, a brace and a quotation mark inside one query credential";

/// What operators actually call their cameras: ordinary single words. This is
/// load-bearing, not decoration. A record on the wire is compact JSON with no
/// whitespace in it anywhere, so a name like `camera 0` smuggles in the only
/// space in the body and hides any reduction that runs to the next space —
/// such a reduction would stop harmlessly at the fixture's own name and eat
/// every camera after it on a real record. The names here carry no whitespace,
/// so the message a real operator gets is the message this contract judges.
const ORDINARY_CAMERA_NAMES: [&str; 3] = ["front", "gate", "barn"];

fn ordinary_camera_name(index: usize) -> String {
    match ORDINARY_CAMERA_NAMES.get(index) {
        Some(name) => (*name).to_string(),
        None => format!("cam{index}"),
    }
}

fn adversarial_addresses() -> Vec<SurfacedAddress> {
    vec![
        SurfacedAddress {
            what_it_is: MARKER_RICH_CREDENTIAL,
            // Every character an operator might type into a password that also
            // means something to the text around it: an apostrophe, a closing
            // brace, and a quotation mark — which the record renders as `\"`
            // once it is written into a JSON string value. A reduction that
            // stops at any of them leaves the rest of the credential standing;
            // one that runs past the string it lives in eats the cameras that
            // come after it.
            url: format!("rtsp://192.0.2.1:1/m?pwd=pa'}}\"{CAMERA_PASSWORD}"),
            visible: "rtsp://192.0.2.1:1/m".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "pwd=".to_string()],
        },
        SurfacedAddress {
            what_it_is: "a marker character inside a query credential",
            url: format!("rtsp://192.0.2.2:1/a?pwd=p@{CAMERA_PASSWORD}"),
            visible: "rtsp://192.0.2.2:1/a".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "pwd=".to_string()],
        },
        SurfacedAddress {
            what_it_is: "a marker character inside one query value with a credential in the next",
            url: format!("rtsp://192.0.2.3:1/i?u=admin@site&pwd={CAMERA_PASSWORD}"),
            visible: "rtsp://192.0.2.3:1/i".to_string(),
            // Every forbidden token here names part of the ADDRESS. A bare
            // word like `site` would also appear inside `site_name`, an
            // ordinary setting the refusal quotes for good reason, so
            // forbidding it would make this case unsatisfiable without
            // throwing away the useful half of the message. The teeth are
            // unchanged: `site&pwd=` is exactly what surfaced when the query
            // tail survived as a host, and the credential and its key are
            // forbidden outright.
            hidden: vec![
                CAMERA_PASSWORD.to_string(),
                "u=admin@site".to_string(),
                "site&pwd=".to_string(),
                "pwd=".to_string(),
            ],
        },
        SurfacedAddress {
            what_it_is: "an apostrophe inside a query credential",
            url: format!("rtsp://192.0.2.4:1/a?pwd=pa'{CAMERA_PASSWORD}"),
            visible: "rtsp://192.0.2.4:1/a".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "pwd=".to_string()],
        },
        SurfacedAddress {
            what_it_is: "a closing brace inside a query credential",
            url: format!("rtsp://192.0.2.5:1/a?pwd=pa}}{CAMERA_PASSWORD}"),
            visible: "rtsp://192.0.2.5:1/a".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "pwd=".to_string()],
        },
        SurfacedAddress {
            what_it_is: "a quotation mark inside a query credential",
            url: format!("rtsp://192.0.2.6:1/a?pwd=pa\"{CAMERA_PASSWORD}"),
            visible: "rtsp://192.0.2.6:1/a".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "pwd=".to_string()],
        },
        SurfacedAddress {
            what_it_is: "a closing bracket inside the credential",
            url: format!("rtsp://operator:pa]{CAMERA_PASSWORD}@192.0.2.7:1/g"),
            visible: "rtsp://192.0.2.7:1/g".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "operator:pa".to_string()],
        },
        SurfacedAddress {
            what_it_is: "a backslash inside the credential",
            url: format!("rtsp://operator:pa\\{CAMERA_PASSWORD}@192.0.2.8:1/h"),
            visible: "rtsp://192.0.2.8:1/h".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "operator:pa".to_string()],
        },
        SurfacedAddress {
            what_it_is: "a vendor abbreviating its query keys",
            url: format!("rtsp://192.0.2.9:1/a?usr=operator&pwd={CAMERA_PASSWORD}"),
            visible: "rtsp://192.0.2.9:1/a".to_string(),
            hidden: vec![
                CAMERA_PASSWORD.to_string(),
                "usr=".to_string(),
                "pwd=".to_string(),
                "operator".to_string(),
            ],
        },
        SurfacedAddress {
            what_it_is: "a query key that looks like nothing in particular",
            url: format!("rtsp://192.0.2.10:1/b?k={CAMERA_PASSWORD}"),
            visible: "rtsp://192.0.2.10:1/b".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "k=".to_string()],
        },
        SurfacedAddress {
            what_it_is: "the spelled-out query keys",
            url: format!("rtsp://192.0.2.11:1/c?user=admin&password={CAMERA_PASSWORD}"),
            visible: "rtsp://192.0.2.11:1/c".to_string(),
            hidden: vec![
                CAMERA_PASSWORD.to_string(),
                "admin".to_string(),
                "user=".to_string(),
                "password=".to_string(),
            ],
        },
        SurfacedAddress {
            what_it_is: "an encoded marker inside the userinfo",
            url: format!("rtsp://operator%40site:{CAMERA_PASSWORD}@192.0.2.12:1/d"),
            visible: "rtsp://192.0.2.12:1/d".to_string(),
            hidden: vec![
                CAMERA_PASSWORD.to_string(),
                "operator%40site".to_string(),
                "%40".to_string(),
            ],
        },
        // A password containing whitespace is not here, and that is a
        // decision rather than an oversight: a run of text is bounded by
        // whitespace before any of this looks at it, so a credential written
        // with a space in it is already two runs by the time the reduction
        // sees it, and no rule about addresses can rejoin them. What it costs
        // differs by position — before the host it loses the prefix, and after
        // the query marker it loses the TAIL, which is the larger half. It
        // stays accepted because the split happens above this rule, not
        // because the loss is small.
        SurfacedAddress {
            what_it_is: "a delimiter inside the credential",
            url: format!("rtsp://operator:pa,{CAMERA_PASSWORD}@192.0.2.13:1/e"),
            visible: "rtsp://192.0.2.13:1/e".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "operator:pa".to_string()],
        },
        // The two below are the same credential in the same position, written
        // once without a scheme and once with one. What separates them is which
        // `://` the reading takes for the scheme marker: an operator who omits
        // the scheme — the manifest accepts it, and a camera address is a
        // free-form field — and whose password happens to contain `://` puts
        // that marker inside the credential itself, so everything before it is
        // read as a scheme and printed. `operator:pa` names no scheme; a scheme
        // is letters, digits and `+`/`-`/`.`, and nothing that carries a `:` or
        // an `@` inside it can be one. The scheme-bearing form comes first as the control:
        // its own `rtsp://` is found first, so the marker inside the credential
        // never gets read as one, and it must stay reduced exactly as it is.
        SurfacedAddress {
            what_it_is: "a scheme marker inside the credential of an address that names a scheme",
            url: format!("rtsp://operator:pa://ss{CAMERA_PASSWORD}@198.51.100.1:1/l"),
            visible: "rtsp://198.51.100.1:1/l".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "operator:pa".to_string()],
        },
        SurfacedAddress {
            what_it_is: "a scheme marker inside the credential of a schemeless address",
            url: format!("operator:pa://ss{CAMERA_PASSWORD}@198.51.100.2:1/k"),
            visible: "198.51.100.2:1/k".to_string(),
            hidden: vec![CAMERA_PASSWORD.to_string(), "operator:pa".to_string()],
        },
        SurfacedAddress {
            what_it_is: "a credential in the userinfo and another in the query",
            url: format!("rtsp://operator:{CAMERA_PASSWORD}@192.0.2.14:1/f?pwd={CAMERA_PASSWORD}"),
            visible: "rtsp://192.0.2.14:1/f".to_string(),
            hidden: vec![
                CAMERA_PASSWORD.to_string(),
                "operator".to_string(),
                "pwd=".to_string(),
            ],
        },
    ]
}

fn a_record_with(addresses: &[&SurfacedAddress]) -> Map<String, Value> {
    let mut options = a_record_with_a_real_camera();
    options.insert(
        "cameras".to_string(),
        Value::Array(
            addresses
                .iter()
                .enumerate()
                .map(|(index, address)| {
                    json!({ "name": ordinary_camera_name(index), "rtsp_url": address.url })
                })
                .collect(),
        ),
    );
    options
}

#[test]
fn a_surfaced_address_keeps_its_scheme_host_and_path_and_nothing_else() {
    // Every shape below is legitimate operator input against a free-form field,
    // and each one is driven through both paths that surface the Supervisor's
    // own words. A redaction built from a list of credential-looking key names
    // passes some of these and fails the rest — which is the point: the ones it
    // fails are the conventions nobody put on the list.
    for address in adversarial_addresses() {
        for (path, reason) in refusals_for(a_record_with(&[&address])) {
            let context = format!("{path}, {}", address.what_it_is);
            assert!(
                reason.contains(&address.visible),
                "{context}: a refusal must still say which camera it is about, which is the \
                 address without its credentials — expected {}; got {reason}",
                address.visible
            );
            for hidden in &address.hidden {
                assert!(
                    !reason.contains(hidden.as_str()),
                    "{context}: `{hidden}` must not survive into a message or a log; got {reason}"
                );
            }
            assert!(
                !reason.contains('?'),
                "{context}: no part of a query may survive, because which part is a credential \
                 is not knowable; got {reason}"
            );
            assert!(
                !reason.contains('@'),
                "{context}: nothing before the host may survive; got {reason}"
            );
        }
    }
}

#[test]
fn several_addresses_in_one_message_are_each_reduced() {
    // A record carries every camera, so one message can quote several
    // addresses, and the message is compact JSON with no whitespace in it. Both
    // ways of getting this wrong cost the operator the same thing — a refusal
    // they cannot act on — so both are held here at once. Hiding too little
    // leaves a later camera's credential standing behind the first one. Hiding
    // too much eats the remainder of the body from the first credential
    // onwards, so the operator is told about camera one and never learns that
    // cameras two and three were in the record at all.
    //
    // The first address selected is the one whose credential carries an
    // apostrophe, a brace and a quotation mark, so a reduction that stops at
    // any character a credential may legitimately contain is caught here rather
    // than passing on a fixture that happens not to contain it.
    let addresses = adversarial_addresses();
    let mut selected: Vec<&SurfacedAddress> = addresses
        .iter()
        .filter(|address| address.what_it_is == MARKER_RICH_CREDENTIAL)
        .collect();
    assert_eq!(
        selected.len(),
        1,
        "the marker-rich credential must still be in the list this contract selects from"
    );
    selected.extend(
        addresses
            .iter()
            .filter(|address| address.what_it_is != MARKER_RICH_CREDENTIAL)
            .take(2),
    );

    for (path, reason) in refusals_for(a_record_with(&selected)) {
        for (index, address) in selected.iter().enumerate() {
            let context = format!("{path}, {}", address.what_it_is);

            // Direction one: the whole record is still described. Every camera
            // in it is named, and named by the entry the operator wrote, not by
            // a word that happens to occur elsewhere in the body.
            let named = format!("\"name\":\"{}\"", ordinary_camera_name(index));
            assert!(
                reason.contains(&named),
                "{context}: a refusal about a record with {} cameras must still name all of \
                 them — {named} is missing, so the operator is never told this camera was in \
                 the record; got {reason}",
                selected.len()
            );
            assert!(
                reason.contains(&address.visible),
                "{context}: every camera the message is about must still be nameable — expected \
                 {}; got {reason}",
                address.visible
            );

            // Direction two: nothing a credential is written in survives, no
            // matter which camera in the list carries it.
            for hidden in &address.hidden {
                assert!(
                    !reason.contains(hidden.as_str()),
                    "{context}: `{hidden}` survived alongside {} others; got {reason}",
                    selected.len() - 1
                );
            }
        }
        assert!(
            !reason.contains(CAMERA_PASSWORD),
            "{path}: no camera's credential may survive a message that quotes several of them; \
             got {reason}"
        );
    }
}

#[test]
fn a_refusal_still_says_what_was_refused() {
    // The other half of the promise, and the reason redaction cannot simply
    // drop the whole message: an operator reading a refusal has to be able to
    // act on it. Both paths keep the Supervisor's own explanation.
    let address = adversarial_addresses().remove(0);
    let refusals = refusals_for(a_record_with(&[&address]));
    let write = &refusals[0].1;
    let read = &refusals[1].1;
    assert!(
        write.contains("cameras"),
        "a refused write must carry the Supervisor's own explanation of what it refused, not \
         only a status code; got {write}"
    );
    assert!(
        read.contains("add-on record") || read.contains("cameras"),
        "a refused read must carry its own explanation too; got {read}"
    );
}

/// The reason surfaced by each refusal path, for one record. Both paths are
/// driven, because a redaction fixed on one of them and not the other leaves
/// the credential reachable through the one that was missed.
fn refusals_for(record: Map<String, Value>) -> Vec<(&'static str, String)> {
    let mut refusals = Vec::new();

    let write_side = SupervisorDouble::start(record.clone(), 400);
    let client = write_side.client();
    let held = client.read_options().expect("read the add-on record");
    let updated = held.merge(REFLECTED_SETTING, &SettingValue::Int(9));
    let failure = client
        .write_options(&updated)
        .expect_err("this stand-in refuses the write");
    refusals.push(("refused write", reason_of(&failure)));

    let read_side = SupervisorDouble::start_refusing_reads(record);
    let failure = read_side
        .client()
        .read_options()
        .expect_err("this stand-in refuses the read");
    refusals.push(("refused read", reason_of(&failure)));

    refusals
}

fn reason_of(failure: &ReflectionFailure) -> String {
    match failure {
        ReflectionFailure::SupervisorRejected { reason } => reason.clone(),
        other => panic!("a refusal must be classified as a rejection; got {other:?}"),
    }
}

// ── What may be read as a scheme, and what may not ────────────────────────────
//
// A run's leading `://` is the scheme marker only when the text before it could
// actually be a scheme, and a scheme begins with a letter — the character set
// alone is not enough. An operator may omit the scheme, so a password holding a
// `://` puts that marker inside the credential; whatever precedes it is then
// kept and printed as the camera's address. The three contracts below fix where
// that line falls: a prefix that cannot be a scheme is part of the credential
// and goes, and a prefix that can be one stays, which is a hole named further
// down rather than a defect.

/// Drive one address whose credential carries a `://` through both refusal
/// paths, and hold that the host and path still name the camera while the given
/// text — the credential prefix ahead of that marker — does not survive.
fn a_credential_prefix_never_surfaces(what_it_is: &'static str, url: String, visible: &str) {
    let address = SurfacedAddress {
        what_it_is,
        url,
        visible: visible.to_string(),
        hidden: Vec::new(),
    };
    let prefix = address
        .url
        .split_once("://")
        .expect("this shape is written with a scheme marker inside its credential")
        .0
        .to_string();
    for (path, reason) in refusals_for(a_record_with(&[&address])) {
        let context = format!("{path}, {what_it_is}");
        assert!(
            reason.contains(&address.visible),
            "{context}: a refusal must still say which camera it is about — expected {}; got \
             {reason}",
            address.visible
        );
        assert!(
            !reason.contains(&format!("{prefix}://")),
            "{context}: `{prefix}` cannot be a scheme, so it is the first characters of the \
             operator's password and must not be printed as the camera's address; got {reason}"
        );
        assert!(
            !reason.contains(CAMERA_PASSWORD),
            "{context}: no part of the credential itself may survive; got {reason}"
        );
    }
}

#[test]
fn a_digit_leading_prefix_is_no_scheme_and_is_read_as_the_credential_it_is() {
    // No camera scheme begins with a digit, and none can: a scheme starts with
    // a letter. So `123` before the marker is the operator's password starting
    // with three digits, and keeping it prints the front of the secret in the
    // one place a refusal is guaranteed to be read.
    a_credential_prefix_never_surfaces(
        "a digit-leading credential written before the scheme marker",
        format!("123://ss{CAMERA_PASSWORD}@198.51.100.3:1/a"),
        "198.51.100.3:1/a",
    );
}

#[test]
fn a_dot_leading_prefix_is_no_scheme_and_is_read_as_the_credential_it_is() {
    // The same line from the other side: `.` is a character a scheme may
    // contain but may not begin with, so a prefix made only of dots is no
    // scheme either, and it is likewise the head of a password.
    a_credential_prefix_never_surfaces(
        "a dot-leading credential written before the scheme marker",
        format!("...://ss{CAMERA_PASSWORD}@198.51.100.4:1/i"),
        "198.51.100.4:1/i",
    );
}

// ── What this contract deliberately does not cover ────────────────────────────
//
// Every case below is a hole in the rule, not a defect under it, and each is
// asserted green so the decision is visible to whoever reads this next instead
// of being rediscovered as a surprise. If one of the tradeoffs is ever
// reversed, these tests are what fails and says why.

#[test]
fn a_secret_written_into_the_path_survives_because_the_path_names_the_camera() {
    // The rule keeps the path on purpose: the path is how an operator knows
    // WHICH camera a refusal is about, and dropping it would leave a message
    // that names a host and nothing else on a node watching several streams off
    // one recorder. A credential written into a path segment therefore
    // survives. The tradeoff is deliberate and one-directional — usefulness
    // bought at the cost of this one shape — and reversing it means giving up
    // naming the camera, which is the other half of the promise.
    let address = SurfacedAddress {
        what_it_is: "a secret in the path",
        url: format!("rtsp://192.0.2.15:1/{CAMERA_PASSWORD}"),
        visible: format!("rtsp://192.0.2.15:1/{CAMERA_PASSWORD}"),
        hidden: Vec::new(),
    };
    for (path, reason) in refusals_for(a_record_with(&[&address])) {
        assert!(
            reason.contains(&address.visible),
            "{path}: the path is kept so the camera can be named; got {reason}"
        );
    }
}

#[test]
fn a_letter_leading_credential_prefix_survives_because_it_is_written_exactly_like_a_scheme() {
    // The two contracts above drop a credential prefix that cannot be a scheme.
    // This one is the residue they cannot reach: `pa` before a `://` is spelled
    // the way every real scheme is spelled, so a password beginning `pa://` and
    // an address whose scheme is `pa` are the same characters in the same order,
    // and no reading of the text can separate them. Hiding the prefix to close
    // this would strip `rtsp` off every ordinary address, which is the scheme an
    // operator needs in order to recognise the camera at all — so the shape is
    // accepted rather than fixed.
    //
    // What it costs is bounded and worth stating plainly: only the part of the
    // password BEFORE the `://` is surfaced, because everything from there to
    // the `@` is reduced away like any other userinfo. The secret after the
    // marker never appears — the second assertion holds that bound, and if the
    // exposure ever widened past the prefix it is what fails.
    let address = SurfacedAddress {
        what_it_is: "a credential whose leading segment is spelled like a scheme",
        url: format!("pa://ss{CAMERA_PASSWORD}@198.51.100.5:1/g"),
        visible: "pa://198.51.100.5:1/g".to_string(),
        hidden: Vec::new(),
    };
    for (path, reason) in refusals_for(a_record_with(&[&address])) {
        assert!(
            reason.contains(&address.visible),
            "{path}: a scheme-shaped prefix is kept, because keeping it is the same rule that \
             keeps `rtsp`; got {reason}"
        );
        assert!(
            !reason.contains(CAMERA_PASSWORD),
            "{path}: the exposure is bounded to the segment before the marker — the credential \
             itself must still be gone; got {reason}"
        );
    }
}

#[test]
fn an_encoded_delimiter_is_outside_a_rule_written_about_literal_ones() {
    // The reduction reads an address by its literal delimiters. A percent-
    // encoded one is not a delimiter to that reading, so an address whose query
    // marker is written `%3F` is a path as far as this rule is concerned and
    // survives whole. Closing it means decoding before reducing, which changes
    // what a "run of text" is for every other shape too — a wider change than
    // this contract makes, and one nobody has yet needed.
    let address = SurfacedAddress {
        what_it_is: "an encoded query marker",
        url: format!("rtsp://192.0.2.16:1/c%3Fpwd%3D{CAMERA_PASSWORD}"),
        visible: format!("rtsp://192.0.2.16:1/c%3Fpwd%3D{CAMERA_PASSWORD}"),
        hidden: Vec::new(),
    };
    for (path, reason) in refusals_for(a_record_with(&[&address])) {
        assert!(
            reason.contains(&address.visible),
            "{path}: an encoded delimiter is read as ordinary path text; got {reason}"
        );
    }
}
