//! Per-source-kind, fallible, canonicalising `CameraId` construction.
//!
//! `CameraId::new` accepts any string, so nothing stops two independently
//! authored source lanes from inventing mutually incompatible identity
//! schemes for the same physical camera. These constructors make the
//! durable-identity rule structural: one constructor per source kind, each
//! taking what genuinely identifies that kind (durable hardware identity for
//! USB/CSI, endpoint URL for RTSP/MJPEG); fallible, so an input that cannot
//! yield a durable identity is an error, never a silently accepted bad
//! identity; canonicalising, so inputs differing only incidentally (case,
//! trailing detail, default port) produce the same identity, while genuinely
//! different cameras stay distinct; and scoped to the owning node, so two
//! nodes' cameras can never collide.
//!
//! Canonical endpoint identity keeps scheme, case-folded host, port (an
//! absent port and the scheme's own default port collapse together), and
//! path; only userinfo (credentials) is stripped structurally. Path is
//! significant, not incidental: two MJPEG cameras behind one NVR at
//! `http://host/cam1` and `http://host/cam2` are the ordinary deployment,
//! not an edge case, and a derivation that discards the path silently drops
//! one of them.
//!
//! A real NVR also distinguishes channels by query (e.g.
//! `?channel=1&subtype=0`), so the query keeps its distinguishing power too
//! — but the raw query text is never retained in the identity at all. The
//! query is sorted (so parameter order is incidental) and reduced to an
//! 8-hex-character content digest, rendered as `?q=<digest>`; an absent or
//! empty query contributes no digest component. Copying a query verbatim
//! into the identity is a credential-disclosure route (a rotating
//! query-carried token would otherwise leak, in full, into every surface a
//! `CameraId` reaches: `as_str`, its derived `Debug`, mounts, and telemetry
//! keys), so the digest keeps the distinguishing power a real NVR needs
//! without ever making that disclosure possible.
//!
//! Role congruence — a camera declared with both an analysis stream
//! (`rtsp_url`) and a separate live stream (`live_rtsp_url`) being ONE
//! camera — is an ENTRY-level property, not a URL-level one: an entry's
//! identity derives from its analysis endpoint alone, and `live_rtsp_url`
//! never participates (see `crates/vigil/tests/camera_config_schema.rs` for
//! the entry-level tests, including cross-entry duplicate-camera
//! detection). It is not reconstructed here by collapsing two distinct URLs
//! (same host/port, different path) onto one identity — that was the prior
//! design, and it bought role congruence by deleting the very information
//! that distinguishes two real, differently-pathed cameras on the same
//! host, which silently drops a configured camera rather than merely
//! showing a phantom one.

use vigil::camera_track::{CameraId, CameraIdError, CameraQuerySalt};

// ---------------------------------------------------------------------
// USB
// ---------------------------------------------------------------------

#[test]
fn from_usb_is_stable_across_repeated_calls_with_the_same_inputs() {
    let first = CameraId::from_usb("node-a", "vendor-04b4:product-00f9:serial-0000A1B2")
        .expect("a durable vendor:product:serial value must be accepted");
    let second = CameraId::from_usb("node-a", "vendor-04b4:product-00f9:serial-0000A1B2")
        .expect("a durable vendor:product:serial value must be accepted");
    assert_eq!(
        first, second,
        "the same node and the same durable hardware identity must resolve to the same CameraId"
    );
}

#[test]
fn from_usb_refuses_a_transient_dev_video_path() {
    let result = CameraId::from_usb("node-a", "/dev/video0");
    assert_eq!(
        result,
        Err(CameraIdError::NotDurable {
            field: "usb_device"
        }),
        "a /dev/videoN-style path is not durable across restarts and must be refused, not accepted as a bad identity"
    );
}

#[test]
fn from_usb_refuses_an_empty_hardware_identity() {
    let result = CameraId::from_usb("node-a", "");
    assert_eq!(
        result,
        Err(CameraIdError::NotDurable {
            field: "usb_device"
        }),
        "an empty value carries no durable identity at all and must be refused"
    );
}

#[test]
fn from_usb_trims_incidental_surrounding_whitespace() {
    let unpadded = CameraId::from_usb("node-a", "vendor-04b4:product-00f9:serial-0000A1B2")
        .expect("a durable value must be accepted");
    let padded = CameraId::from_usb("node-a", "  vendor-04b4:product-00f9:serial-0000A1B2  \n")
        .expect("surrounding whitespace is incidental and must not cause a refusal");
    assert_eq!(
        unpadded, padded,
        "surrounding whitespace around a durable hardware identity is incidental and must not change the resolved identity"
    );
    let different_serial = CameraId::from_usb("node-a", "vendor-04b4:product-00f9:serial-0000A1B3")
        .expect("a durable value must be accepted");
    assert_ne!(
        unpadded, different_serial,
        "a companion check that a genuinely different serial still resolves to a different identity, so this test cannot be satisfied by a stub that folds every usb_device onto one constant"
    );
}

#[test]
fn from_usb_preserves_case_because_hardware_serials_can_be_case_sensitive() {
    let lower = CameraId::from_usb("node-a", "vendor-04b4:product-00f9:serial-abcxyz")
        .expect("a durable value must be accepted");
    let upper = CameraId::from_usb("node-a", "vendor-04b4:product-00f9:serial-ABCXYZ")
        .expect("a durable value must be accepted");
    assert_ne!(
        lower, upper,
        "a USB serial can be genuinely case-sensitive alphanumeric text (unlike a DNS host name), so case must NOT be folded away for a hardware identity — over-normalising here would collapse two potentially distinct devices"
    );
}

#[test]
fn from_usb_refuses_an_empty_node_id() {
    let result = CameraId::from_usb("", "vendor-04b4:product-00f9:serial-0000A1B2");
    assert_eq!(
        result,
        Err(CameraIdError::EmptyNode),
        "an empty owning-node identity carries no cross-node scoping and must be refused"
    );
}

#[test]
fn from_usb_includes_owning_node_so_two_nodes_never_collide() {
    let node_a = CameraId::from_usb("node-a", "vendor-04b4:product-00f9:serial-0000A1B2")
        .expect("a durable value must be accepted");
    let node_b = CameraId::from_usb("node-b", "vendor-04b4:product-00f9:serial-0000A1B2")
        .expect("a durable value must be accepted");
    assert_ne!(
        node_a, node_b,
        "the identical hardware identity declared on two different nodes must resolve to two different CameraIds, or two independently owned devices would collide into one camera"
    );
}

// ---------------------------------------------------------------------
// CSI
// ---------------------------------------------------------------------

#[test]
fn from_csi_is_stable_across_repeated_calls_with_the_same_inputs() {
    let first = CameraId::from_csi("node-a", "csi0-imx219-module-7F3A")
        .expect("a durable module identity must be accepted");
    let second = CameraId::from_csi("node-a", "csi0-imx219-module-7F3A")
        .expect("a durable module identity must be accepted");
    assert_eq!(
        first, second,
        "the same node and the same durable module identity must resolve to the same CameraId"
    );
}

#[test]
fn from_csi_refuses_a_transient_dev_path() {
    let result = CameraId::from_csi("node-a", "/dev/media0");
    assert_eq!(
        result,
        Err(CameraIdError::NotDurable {
            field: "csi_module"
        }),
        "a /dev/-style path is not durable across restarts and must be refused"
    );
}

#[test]
fn from_csi_refuses_an_empty_node_id() {
    let result = CameraId::from_csi("", "csi0-imx219-module-7F3A");
    assert_eq!(result, Err(CameraIdError::EmptyNode));
}

#[test]
fn from_csi_includes_owning_node_so_two_nodes_never_collide() {
    let node_a = CameraId::from_csi("node-a", "csi0-imx219-module-7F3A")
        .expect("a durable value must be accepted");
    let node_b = CameraId::from_csi("node-b", "csi0-imx219-module-7F3A")
        .expect("a durable value must be accepted");
    assert_ne!(
        node_a, node_b,
        "the identical module identity declared on two different nodes must not collide into one camera"
    );
}

// ---------------------------------------------------------------------
// RTSP
// ---------------------------------------------------------------------

#[test]
fn from_rtsp_url_canonicalizes_host_case() {
    let salt = CameraQuerySalt::generate();
    let mixed_case = CameraId::from_rtsp_url("node-a", "rtsp://Camera.LOCAL:554/mainstream", &salt)
        .expect("a valid rtsp:// URL must be accepted");
    let lower_case = CameraId::from_rtsp_url("node-a", "rtsp://camera.local:554/mainstream", &salt)
        .expect("a valid rtsp:// URL must be accepted");
    assert_eq!(
        mixed_case, lower_case,
        "a DNS host name is case-insensitive, so host case is incidental and must not change the resolved identity"
    );
    let different_host =
        CameraId::from_rtsp_url("node-a", "rtsp://other-camera.local:554/mainstream", &salt)
            .expect("a valid rtsp:// URL must be accepted");
    assert_ne!(
        mixed_case, different_host,
        "a companion check that a genuinely different host still resolves to a different identity"
    );
}

#[test]
fn from_rtsp_url_canonicalizes_the_default_rtsp_port() {
    let salt = CameraQuerySalt::generate();
    let no_port = CameraId::from_rtsp_url("node-a", "rtsp://camera.local/mainstream", &salt)
        .expect("an rtsp:// URL with no explicit port must be accepted");
    let default_port =
        CameraId::from_rtsp_url("node-a", "rtsp://camera.local:554/mainstream", &salt)
            .expect("an rtsp:// URL with the explicit default port must be accepted");
    assert_eq!(
        no_port, default_port,
        "an absent port and the RTSP default port (554) name the same endpoint and are incidental to identity"
    );
    let non_default_port =
        CameraId::from_rtsp_url("node-a", "rtsp://camera.local:8554/mainstream", &salt)
            .expect("an rtsp:// URL with a non-default explicit port must be accepted");
    assert_ne!(
        no_port, non_default_port,
        "a genuinely different, non-default port names a different endpoint and must resolve to a different identity"
    );
}

/// INVERTED (was `from_rtsp_url_strips_credentials_and_query_tokens`):
/// the old test asserted a plain, query-less URL and a credentialed URL
/// carrying a DIFFERENT query (`?token=...`) resolved to the SAME identity
/// — which only held because query was discarded entirely. The ruling
/// keeps query as significant, so that premise is now false: this test
/// still pins that userinfo (credentials) is stripped structurally, but
/// requires the SAME query on both sides to prove it, and adds a companion
/// check that a differing query — not just a differing host — now produces
/// a genuinely different identity.
#[test]
fn from_rtsp_url_strips_credentials_via_userinfo_but_keeps_the_query_as_significant() {
    let salt = CameraQuerySalt::generate();
    let plain_with_query = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://camera.local:554/mainstream?channel=1",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    let credentialed_same_query = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://vigil:hunter2secret@camera.local:554/mainstream?channel=1",
        &salt,
    )
    .expect("a credentialed rtsp:// URL must still be accepted");
    assert_eq!(
        plain_with_query, credentialed_same_query,
        "the same endpoint and the same query, declared with or without embedded credentials, must resolve to the same identity — the credential (never the query) is what is stripped"
    );
    assert!(
        !credentialed_same_query.as_str().contains("hunter2secret"),
        "the password must never survive into the identity string: got {:?}",
        credentialed_same_query.as_str()
    );

    // The property this test exists to invert: a query that genuinely
    // differs (a real NVR's channel selector) must now produce a DIFFERENT
    // identity, not the same one — dropping the query the way the old
    // implementation did would silently fold two distinct channels on the
    // same host/port/path into one camera.
    let different_query = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://camera.local:554/mainstream?channel=2",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    assert_ne!(
        plain_with_query, different_query,
        "a genuinely different query (e.g. a different NVR channel selector) must resolve to a different identity now that query is kept as significant"
    );

    // Companion distinctness check: a genuinely different host must not
    // collide either.
    let different_endpoint = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://a-different-camera.local:554/mainstream?channel=1",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    assert_ne!(plain_with_query, different_endpoint);
}

/// New: the query is reduced to a short content digest (16 hex characters of
/// the sorted canonical query, prefixed `q=`), never copied into the
/// identity verbatim. Disclosure of a query-carried token must become
/// structurally impossible — no code path can print a value the type does
/// not contain — while the digest still distinguishes genuinely different
/// queries from one another (see the companion distinguishing-power test
/// below).
#[test]
fn from_rtsp_url_reduces_the_query_to_a_short_hex_digest_never_verbatim_text() {
    let salt = CameraQuerySalt::generate();
    let identity = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://camera.local:554/mainstream?channel=1&subtype=0",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    let rendered = identity.as_str();
    assert!(
        !rendered.contains("channel=1") && !rendered.contains("subtype=0"),
        "the raw query text must never be copied verbatim into the identity, got: {rendered:?}"
    );
    let digest_marker = "?q=";
    let digest_start = rendered.find(digest_marker).unwrap_or_else(|| {
        panic!("expected a '{digest_marker}' digest marker in the identity, got: {rendered:?}")
    }) + digest_marker.len();
    let digest = &rendered[digest_start..];
    assert_eq!(
        digest.len(),
        16,
        "the query digest must be exactly 16 hex characters, got {digest:?} within {rendered:?}"
    );
    assert!(
        digest
            .chars()
            .all(|character| character.is_ascii_hexdigit()),
        "the query digest must be hex-only, got {digest:?} within {rendered:?}"
    );
}

/// New: the digest must not destroy the query's distinguishing power — two
/// different NVR channel selectors are two different cameras, and reducing
/// the query to a digest is only acceptable if it still tells them apart.
#[test]
fn from_rtsp_url_query_digest_still_distinguishes_different_channel_selectors() {
    let salt = CameraQuerySalt::generate();
    let channel_one =
        CameraId::from_rtsp_url("node-a", "rtsp://nvr.local:554/mainstream?channel=1", &salt)
            .expect("a valid rtsp:// URL must be accepted");
    let channel_two =
        CameraId::from_rtsp_url("node-a", "rtsp://nvr.local:554/mainstream?channel=2", &salt)
            .expect("a valid rtsp:// URL must be accepted");
    assert_ne!(
        channel_one, channel_two,
        "reducing the query to a digest must not destroy its distinguishing power — two different NVR channel selectors must still resolve to two different camera identities"
    );
}

/// New: query parameter order is incidental — `?a=1&b=2` and `?b=2&a=1`
/// describe the same camera — so the query must be sorted before it is
/// digested, or the identical parameter set arriving in a different order
/// would spuriously look like two different cameras.
#[test]
fn from_rtsp_url_query_digest_is_insensitive_to_parameter_order() {
    let salt = CameraQuerySalt::generate();
    let a_then_b = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://camera.local:554/mainstream?a=1&b=2",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    let b_then_a = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://camera.local:554/mainstream?b=2&a=1",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    assert_eq!(
        a_then_b, b_then_a,
        "the same set of query parameters in a different order must resolve to the same identity — the query must be sorted before it is digested, not digested in arrival order"
    );
}

/// New: an absent query and an explicitly empty query must keep resolving
/// to the same identity (the uniform rendering already in place), and
/// neither may contribute a digest component at all — there is nothing to
/// digest, so no `q=` marker may appear.
#[test]
fn from_rtsp_url_absent_and_empty_query_contribute_no_digest_component() {
    let salt = CameraQuerySalt::generate();
    let no_query = CameraId::from_rtsp_url("node-a", "rtsp://camera.local:554/mainstream", &salt)
        .expect("a valid rtsp:// URL must be accepted");
    let empty_query =
        CameraId::from_rtsp_url("node-a", "rtsp://camera.local:554/mainstream?", &salt)
            .expect("a valid rtsp:// URL with an explicitly empty query must be accepted");
    assert_eq!(
        no_query, empty_query,
        "a query-less URL and one with an explicitly empty query must resolve to the same identity, exactly as before the query was digested"
    );
    assert!(
        !no_query.as_str().contains("q="),
        "an absent/empty query must contribute no digest component at all, got: {:?}",
        no_query.as_str()
    );
}

/// New: the documented consequence of keeping the query as significant — a
/// rotating token embedded in the URL query rotates the camera's identity,
/// via the digest it feeds, never via the raw token surviving into the
/// identity. This is NOT a bug to be worked around: the supported way to
/// authenticate a camera is the entry's `username`/`password` fields, which
/// never enter the identity at all (see the userinfo-stripping test above).
/// Embedding a credential/token in the query instead is documented guidance
/// against, not a supported pattern this constructor is expected to
/// special-case.
///
/// RE-EXPRESSED (was asserting rotation the same way, but predates the
/// digest ruling): still proves rotation strictly by inequality between two
/// computed identities, never by naming an expected rendered string — doing
/// that would re-embed a token-like literal in an expected identity string,
/// the exact disclosure this ruling closes, only in a smaller, digest-sized
/// form. Adds an explicit check that neither identity's rendered form ever
/// carries the raw token text, proving the digest genuinely replaces it
/// rather than merely truncating it.
#[test]
fn from_rtsp_url_query_carried_token_rotates_identity_as_documented_guidance() {
    let salt = CameraQuerySalt::generate();
    let before_rotation = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://camera.local:554/mainstream?token=abc123leak",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    let after_rotation = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://camera.local:554/mainstream?token=def456fresh",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    assert_ne!(
        before_rotation, after_rotation,
        "a rotating query-carried token is documented to rotate the camera's identity — this is the expected, documented consequence of keeping the query significant, not a defect; operators must authenticate via the entry's username/password fields instead"
    );
    assert!(
        !before_rotation.as_str().contains("abc123leak")
            && !after_rotation.as_str().contains("def456fresh"),
        "the raw query-carried token must never survive into the identity string in any form, got {:?} and {:?}",
        before_rotation.as_str(),
        after_rotation.as_str()
    );
}

/// New: disclosure must be structurally impossible across every surface a
/// `CameraId` reaches, not merely `as_str` — including its derived `Debug`,
/// which mounts, telemetry, and panic/log messages can all format directly.
/// A digest that only replaced the query in `as_str` while some other path
/// still rendered the raw query would leave the credential-disclosure route
/// this ruling exists to close.
#[test]
fn from_rtsp_url_identity_never_leaks_the_raw_query_through_debug_either() {
    let salt = CameraQuerySalt::generate();
    let identity = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://camera.local:554/mainstream?token=abc123leak",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    let debug_text = format!("{identity:?}");
    assert!(
        !debug_text.contains("abc123leak") && !debug_text.contains("token="),
        "the raw query text must not survive even through the derived Debug impl, got: {debug_text:?}"
    );
}

/// New: the exact defect this ruling exists to fix — two channels/streams on
/// one host/port that differ only by path (e.g. an NVR serving `/ch1` and
/// `/ch2`) must resolve to two distinct camera identities. The prior
/// implementation discarded the path, so this scenario silently collapsed a
/// second, real, configured camera into the first one's identity.
#[test]
fn from_rtsp_url_keeps_the_path_as_significant_so_two_channels_on_one_host_stay_distinct() {
    let salt = CameraQuerySalt::generate();
    let channel_one = CameraId::from_rtsp_url("node-a", "rtsp://nvr.local:554/ch1", &salt)
        .expect("a valid rtsp:// URL must be accepted");
    let channel_two = CameraId::from_rtsp_url("node-a", "rtsp://nvr.local:554/ch2", &salt)
        .expect("a valid rtsp:// URL must be accepted");
    assert_ne!(
        channel_one, channel_two,
        "two distinct paths on the identical host/port are two distinct cameras (an NVR serving multiple channels), and discarding the path would silently drop one of them"
    );

    // Companion stability check: the SAME path resolves to the SAME
    // identity, so this cannot be satisfied by a stub that makes every call
    // return a fresh, never-equal identity.
    let channel_one_again = CameraId::from_rtsp_url("node-a", "rtsp://nvr.local:554/ch1", &salt)
        .expect("a valid rtsp:// URL must be accepted");
    assert_eq!(channel_one, channel_one_again);
}

#[test]
fn from_rtsp_url_treats_rtsp_and_rtsps_as_distinct_endpoints() {
    let salt = CameraQuerySalt::generate();
    let plaintext = CameraId::from_rtsp_url("node-a", "rtsp://camera.local:554/mainstream", &salt)
        .expect("a valid rtsp:// URL must be accepted");
    let encrypted = CameraId::from_rtsp_url("node-a", "rtsps://camera.local:554/mainstream", &salt)
        .expect("a valid rtsps:// URL must be accepted");
    assert_ne!(
        plaintext, encrypted,
        "rtsp and rtsps are materially different transports (one is TLS-wrapped) sharing a host/port, so the scheme must NOT be normalised away"
    );
}

#[test]
fn from_rtsp_url_refuses_a_non_rtsp_scheme() {
    let salt = CameraQuerySalt::generate();
    let result = CameraId::from_rtsp_url("node-a", "http://camera.local/mainstream", &salt);
    assert_eq!(
        result,
        Err(CameraIdError::InvalidUrl { field: "rtsp_url" }),
        "an rtsp_url value must parse as rtsp:// or rtsps://, never a different scheme"
    );
}

#[test]
fn from_rtsp_url_refuses_an_unparsable_value() {
    let salt = CameraQuerySalt::generate();
    let result = CameraId::from_rtsp_url("node-a", "not a url at all", &salt);
    assert_eq!(result, Err(CameraIdError::InvalidUrl { field: "rtsp_url" }));
}

#[test]
fn from_rtsp_url_refuses_an_empty_node_id() {
    let salt = CameraQuerySalt::generate();
    let result = CameraId::from_rtsp_url("", "rtsp://camera.local:554/mainstream", &salt);
    assert_eq!(result, Err(CameraIdError::EmptyNode));
}

/// INVERTED (was `analysis_and_live_roles_of_the_same_camera_resolve_to_the_same_identity`):
/// the old test asserted that a camera's `rtsp_url` (analysis, `/substream`)
/// and its `live_rtsp_url` (live, `/mainstream`) — same host/port, different
/// path — resolved to the SAME `CameraId`, deriving role congruence from a
/// naive per-URL comparison that folded the path away. That was the
/// mechanism this ruling reverses: it bought role congruence by deleting
/// the path, which is exactly the information that distinguishes two real,
/// differently-pathed cameras behind one NVR. Role congruence is restored
/// as an ENTRY-level property instead (see
/// `crates/vigil/tests/camera_config_schema.rs`, where a single entry's
/// identity is derived from its `rtsp_url` alone and `live_rtsp_url` never
/// participates) — at the raw-URL level pinned here, two distinct paths on
/// the same host/port are now two distinct camera ids, full stop.
#[test]
fn distinct_paths_on_the_same_host_and_port_resolve_to_distinct_camera_ids() {
    let salt = CameraQuerySalt::generate();
    let analysis_role_url =
        CameraId::from_rtsp_url("node-a", "rtsp://camera.local:554/substream", &salt)
            .expect("the analysis-role rtsp_url must be accepted");
    let live_role_url =
        CameraId::from_rtsp_url("node-a", "rtsp://camera.local:554/mainstream", &salt)
            .expect("the live-role live_rtsp_url must be accepted");
    assert_ne!(
        analysis_role_url, live_role_url,
        "two distinct paths on the identical host/port must now resolve to two DISTINCT CameraIds — path is significant, not incidental; entry-level role congruence (config.rs deriving one entry's identity from its analysis endpoint alone) is what keeps a real analysis+live camera as one camera, not this URL-level comparison"
    );

    // Companion checks retained from the pre-inversion test: a genuinely
    // different camera (different host, same path suffix) must not
    // collide, and the same URL declared on a different owning node must
    // not collide either.
    let different_camera_same_path = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://a-different-camera.local:554/substream",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    assert_ne!(
        analysis_role_url, different_camera_same_path,
        "two different cameras sharing a path suffix must not collide"
    );
    let different_node_same_urls =
        CameraId::from_rtsp_url("node-b", "rtsp://camera.local:554/substream", &salt)
            .expect("a valid rtsp:// URL must be accepted");
    assert_ne!(
        analysis_role_url, different_node_same_urls,
        "the same camera URL declared on a different owning node must not collide"
    );
}

// ---------------------------------------------------------------------
// MJPEG
// ---------------------------------------------------------------------

#[test]
fn from_mjpeg_url_canonicalizes_host_case_and_the_default_http_port() {
    let salt = CameraQuerySalt::generate();
    let mixed_case = CameraId::from_mjpeg_url("node-a", "http://ESP32CAM.LOCAL/stream", &salt)
        .expect("a valid http:// URL must be accepted");
    let explicit_default_port =
        CameraId::from_mjpeg_url("node-a", "http://esp32cam.local:80/stream", &salt)
            .expect("a valid http:// URL with the explicit default port must be accepted");
    assert_eq!(
        mixed_case, explicit_default_port,
        "host case and an absent-vs-explicit default HTTP port (80) are both incidental and must not change the resolved identity"
    );
    let different_host = CameraId::from_mjpeg_url("node-a", "http://other-cam.local/stream", &salt)
        .expect("a valid http:// URL must be accepted");
    assert_ne!(mixed_case, different_host);
}

/// INVERTED (was `from_mjpeg_url_strips_credentials_and_query_tokens`), same
/// reasoning as the RTSP sibling inversion above: a plain, query-less URL
/// and a credentialed URL carrying a DIFFERENT query no longer resolve to
/// the same identity, because query is now significant. Only userinfo
/// (credentials) is stripped structurally.
#[test]
fn from_mjpeg_url_strips_credentials_via_userinfo_but_keeps_the_query_as_significant() {
    let salt = CameraQuerySalt::generate();
    let plain_with_query =
        CameraId::from_mjpeg_url("node-a", "http://esp32cam.local/stream?channel=1", &salt)
            .expect("a valid http:// URL must be accepted");
    let credentialed_same_query = CameraId::from_mjpeg_url(
        "node-a",
        "http://vigil:hunter2secret@esp32cam.local/stream?channel=1",
        &salt,
    )
    .expect("a credentialed http:// URL must still be accepted");
    assert_eq!(plain_with_query, credentialed_same_query);
    assert!(!credentialed_same_query.as_str().contains("hunter2secret"));

    let different_query =
        CameraId::from_mjpeg_url("node-a", "http://esp32cam.local/stream?channel=2", &salt)
            .expect("a valid http:// URL must be accepted");
    assert_ne!(
        plain_with_query, different_query,
        "a genuinely different query must resolve to a different identity now that query is kept as significant"
    );

    // Companion distinctness check, same reasoning as the RTSP sibling test:
    // stops this test being satisfiable by a constant-identity stub.
    let different_endpoint = CameraId::from_mjpeg_url(
        "node-a",
        "http://a-different-cam.local/stream?channel=1",
        &salt,
    )
    .expect("a valid http:// URL must be accepted");
    assert_ne!(plain_with_query, different_endpoint);
}

/// New, mirroring the RTSP sibling: the query is reduced to a short content
/// digest (16 hex characters of the sorted canonical query, prefixed `q=`),
/// never copied into the identity verbatim.
#[test]
fn from_mjpeg_url_reduces_the_query_to_a_short_hex_digest_never_verbatim_text() {
    let salt = CameraQuerySalt::generate();
    let identity = CameraId::from_mjpeg_url(
        "node-a",
        "http://esp32cam.local/stream?channel=1&subtype=0",
        &salt,
    )
    .expect("a valid http:// URL must be accepted");
    let rendered = identity.as_str();
    assert!(
        !rendered.contains("channel=1") && !rendered.contains("subtype=0"),
        "the raw query text must never be copied verbatim into the identity, got: {rendered:?}"
    );
    let digest_marker = "?q=";
    let digest_start = rendered.find(digest_marker).unwrap_or_else(|| {
        panic!("expected a '{digest_marker}' digest marker in the identity, got: {rendered:?}")
    }) + digest_marker.len();
    let digest = &rendered[digest_start..];
    assert_eq!(
        digest.len(),
        16,
        "the query digest must be exactly 16 hex characters, got {digest:?} within {rendered:?}"
    );
    assert!(
        digest
            .chars()
            .all(|character| character.is_ascii_hexdigit()),
        "the query digest must be hex-only, got {digest:?} within {rendered:?}"
    );
}

/// New, mirroring the RTSP sibling: the digest must not destroy the query's
/// distinguishing power.
#[test]
fn from_mjpeg_url_query_digest_still_distinguishes_different_channel_selectors() {
    let salt = CameraQuerySalt::generate();
    let channel_one =
        CameraId::from_mjpeg_url("node-a", "http://hub.local/stream?channel=1", &salt)
            .expect("a valid http:// URL must be accepted");
    let channel_two =
        CameraId::from_mjpeg_url("node-a", "http://hub.local/stream?channel=2", &salt)
            .expect("a valid http:// URL must be accepted");
    assert_ne!(
        channel_one, channel_two,
        "reducing the query to a digest must not destroy its distinguishing power — two different channel selectors must still resolve to two different camera identities"
    );
}

/// New, mirroring the RTSP sibling: query parameter order is incidental, so
/// the query must be sorted before it is digested.
#[test]
fn from_mjpeg_url_query_digest_is_insensitive_to_parameter_order() {
    let salt = CameraQuerySalt::generate();
    let a_then_b =
        CameraId::from_mjpeg_url("node-a", "http://esp32cam.local/stream?a=1&b=2", &salt)
            .expect("a valid http:// URL must be accepted");
    let b_then_a =
        CameraId::from_mjpeg_url("node-a", "http://esp32cam.local/stream?b=2&a=1", &salt)
            .expect("a valid http:// URL must be accepted");
    assert_eq!(
        a_then_b, b_then_a,
        "the same set of query parameters in a different order must resolve to the same identity — the query must be sorted before it is digested, not digested in arrival order"
    );
}

/// New, mirroring the RTSP sibling: an absent query and an explicitly empty
/// query must keep resolving to the same identity and contribute no digest
/// component at all.
#[test]
fn from_mjpeg_url_absent_and_empty_query_contribute_no_digest_component() {
    let salt = CameraQuerySalt::generate();
    let no_query = CameraId::from_mjpeg_url("node-a", "http://esp32cam.local/stream", &salt)
        .expect("a valid http:// URL must be accepted");
    let empty_query = CameraId::from_mjpeg_url("node-a", "http://esp32cam.local/stream?", &salt)
        .expect("a valid http:// URL with an explicitly empty query must be accepted");
    assert_eq!(
        no_query, empty_query,
        "a query-less URL and one with an explicitly empty query must resolve to the same identity, exactly as before the query was digested"
    );
    assert!(
        !no_query.as_str().contains("q="),
        "an absent/empty query must contribute no digest component at all, got: {:?}",
        no_query.as_str()
    );
}

/// New, mirroring the RTSP sibling: the documented consequence of keeping
/// the query significant is that a rotating query-carried token rotates
/// identity — via the digest it feeds, never via the raw token surviving
/// into the identity. Supported authentication is the entry's
/// `username`/`password` fields, never the URL query.
///
/// RE-EXPRESSED (was asserting rotation the same way, but predates the
/// digest ruling): still proves rotation strictly by inequality, never by
/// naming an expected rendered string, and adds an explicit check that
/// neither identity's rendered form ever carries the raw token text.
#[test]
fn from_mjpeg_url_query_carried_token_rotates_identity_as_documented_guidance() {
    let salt = CameraQuerySalt::generate();
    let before_rotation = CameraId::from_mjpeg_url(
        "node-a",
        "http://esp32cam.local/stream?token=abc123leak",
        &salt,
    )
    .expect("a valid http:// URL must be accepted");
    let after_rotation = CameraId::from_mjpeg_url(
        "node-a",
        "http://esp32cam.local/stream?token=def456fresh",
        &salt,
    )
    .expect("a valid http:// URL must be accepted");
    assert_ne!(
        before_rotation, after_rotation,
        "a rotating query-carried token is documented to rotate the camera's identity — operators must authenticate via the entry's username/password fields instead"
    );
    assert!(
        !before_rotation.as_str().contains("abc123leak")
            && !after_rotation.as_str().contains("def456fresh"),
        "the raw query-carried token must never survive into the identity string in any form, got {:?} and {:?}",
        before_rotation.as_str(),
        after_rotation.as_str()
    );
}

/// New, mirroring the RTSP sibling: disclosure must be structurally
/// impossible across every surface a `CameraId` reaches, not merely
/// `as_str` — including its derived `Debug`.
#[test]
fn from_mjpeg_url_identity_never_leaks_the_raw_query_through_debug_either() {
    let salt = CameraQuerySalt::generate();
    let identity = CameraId::from_mjpeg_url(
        "node-a",
        "http://esp32cam.local/stream?token=abc123leak",
        &salt,
    )
    .expect("a valid http:// URL must be accepted");
    let debug_text = format!("{identity:?}");
    assert!(
        !debug_text.contains("abc123leak") && !debug_text.contains("token="),
        "the raw query text must not survive even through the derived Debug impl, got: {debug_text:?}"
    );
}

/// New: the exact motivating defect for this ruling, stated in its own
/// terms — two MJPEG cameras behind one NVR/hub at different paths (e.g.
/// `http://host/cam1` and `http://host/cam2`) are the ordinary deployment,
/// not an edge case, and must resolve to two distinct camera identities.
#[test]
fn from_mjpeg_url_keeps_the_path_as_significant_so_two_cameras_behind_one_nvr_stay_distinct() {
    let salt = CameraQuerySalt::generate();
    let first_camera = CameraId::from_mjpeg_url("node-a", "http://hub.local/cam1", &salt)
        .expect("a valid http:// URL must be accepted");
    let second_camera = CameraId::from_mjpeg_url("node-a", "http://hub.local/cam2", &salt)
        .expect("a valid http:// URL must be accepted");
    assert_ne!(
        first_camera, second_camera,
        "two MJPEG cameras behind one hub, differing only by path, must resolve to two distinct camera identities — collapsing them silently drops a real, configured camera"
    );

    let first_camera_again = CameraId::from_mjpeg_url("node-a", "http://hub.local/cam1", &salt)
        .expect("a valid http:// URL must be accepted");
    assert_eq!(first_camera, first_camera_again);
}

#[test]
fn from_mjpeg_url_treats_http_and_https_as_distinct_endpoints() {
    let salt = CameraQuerySalt::generate();
    let plaintext = CameraId::from_mjpeg_url("node-a", "http://esp32cam.local/stream", &salt)
        .expect("a valid http:// URL must be accepted");
    let encrypted = CameraId::from_mjpeg_url("node-a", "https://esp32cam.local/stream", &salt)
        .expect("a valid https:// URL must be accepted");
    assert_ne!(
        plaintext, encrypted,
        "http and https are materially different transports sharing a host, so the scheme must NOT be normalised away"
    );
}

#[test]
fn from_mjpeg_url_refuses_a_non_http_scheme() {
    let salt = CameraQuerySalt::generate();
    let result = CameraId::from_mjpeg_url("node-a", "rtsp://esp32cam.local/stream", &salt);
    assert_eq!(
        result,
        Err(CameraIdError::InvalidUrl { field: "mjpeg_url" })
    );
}

#[test]
fn from_mjpeg_url_refuses_an_empty_node_id() {
    let salt = CameraQuerySalt::generate();
    let result = CameraId::from_mjpeg_url("", "http://esp32cam.local/stream", &salt);
    assert_eq!(result, Err(CameraIdError::EmptyNode));
}

// ---------------------------------------------------------------------
// Cross-kind
// ---------------------------------------------------------------------

#[test]
fn the_same_raw_text_across_different_source_kinds_never_collides() {
    let usb = CameraId::from_usb("node-a", "shared-raw-text-123")
        .expect("a durable value must be accepted");
    let csi = CameraId::from_csi("node-a", "shared-raw-text-123")
        .expect("a durable value must be accepted");
    assert_ne!(
        usb, csi,
        "USB and CSI identities built from identical raw text on the identical node must still be distinguished by source kind"
    );
}

// ---------------------------------------------------------------------
// Query canonicalisation collision (the ambiguous-rejoin defect)
// ---------------------------------------------------------------------
//
// `query_pairs()` already *decodes* percent-encoding on read. The prior
// canonicalisation rejoined the decoded pairs as bare `key=value&key=value`
// text with no re-encoding, so a decoded `&`/`=` inside one pair's value
// could be replayed as the delimiter for what looks like a second pair.
// Concretely: `?a=1%26b%3D2` decodes to the SINGLE pair `a` = `"1&b=2"`,
// while `?a=1&b=2` decodes to the TWO pairs `a`=`"1"`, `b`=`"2"` — genuinely
// different queries describing genuinely different endpoints — but the old
// rejoin produced the identical text `"a=1&b=2"` for both, so they resolved
// to the same `CameraId`: exactly the silent camera-merge this digest
// exists to prevent, reintroduced by the canonicalisation step itself.
//
// A do-nothing "fix" is ruled out by construction here: a stub that returns
// a constant digest, or one that ignores the query, is already caught by
// the pre-existing `..._still_distinguishes_different_channel_selectors`
// and `..._query_digest_is_insensitive_to_parameter_order` tests above (a
// constant digest fails the former; an ignored query fails the latter) —
// this pair adds the ambiguous-rejoin case neither of those exercises.

/// RED pair (rtsp): one pair whose value itself contains an encoded `&`
/// and `=` must NOT collide with two genuinely separate pairs.
#[test]
fn from_rtsp_url_one_encoded_pair_does_not_collide_with_two_plain_pairs() {
    let salt = CameraQuerySalt::generate();
    let one_pair_value_contains_encoded_delimiters = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://camera.local:554/mainstream?a=1%26b%3D2",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    let two_plain_pairs = CameraId::from_rtsp_url(
        "node-a",
        "rtsp://camera.local:554/mainstream?a=1&b=2",
        &salt,
    )
    .expect("a valid rtsp:// URL must be accepted");
    assert_ne!(
        one_pair_value_contains_encoded_delimiters, two_plain_pairs,
        "one query pair whose value contains an encoded '&' and '=' must not canonicalise to the same identity as two genuinely separate plain pairs — that is the exact silent camera-merge this digest exists to prevent"
    );
}

/// RED pair (mjpeg): the same collision, mirrored on the MJPEG constructor.
#[test]
fn from_mjpeg_url_one_encoded_pair_does_not_collide_with_two_plain_pairs() {
    let salt = CameraQuerySalt::generate();
    let one_pair_value_contains_encoded_delimiters =
        CameraId::from_mjpeg_url("node-a", "http://hub.local/stream?a=1%26b%3D2", &salt)
            .expect("a valid http:// URL must be accepted");
    let two_plain_pairs =
        CameraId::from_mjpeg_url("node-a", "http://hub.local/stream?a=1&b=2", &salt)
            .expect("a valid http:// URL must be accepted");
    assert_ne!(
        one_pair_value_contains_encoded_delimiters, two_plain_pairs,
        "one query pair whose value contains an encoded '&' and '=' must not canonicalise to the same identity as two genuinely separate plain pairs"
    );
}

/// Property-style companion: several genuinely distinct DECODED pair-sets
/// — built from inputs whose raw, on-the-wire query text is deliberately
/// varied (percent-encoded delimiters, an encoded '%' itself, an extra
/// pair, a value/key swap) — must resolve to pairwise-distinct identities.
/// This is the general form of the RED pair above: it is not satisfiable
/// by a fix that only special-cases the one literal example.
#[test]
fn from_rtsp_url_distinct_decoded_pair_sets_produce_pairwise_distinct_identities() {
    let salt = CameraQuerySalt::generate();
    let base = "rtsp://camera.local:554/mainstream";
    let variants = [
        // decodes to one pair: a = "1&b=2"
        "?a=1%26b%3D2",
        // decodes to two pairs: a=1, b=2
        "?a=1&b=2",
        // decodes to one pair: a = "1%b=2" (a literal '%' in the value)
        "?a=1%25b%3D2",
        // decodes to two pairs with a third thrown in: a=1, b=2, c=3
        "?a=1&b=2&c=3",
        // decodes to one pair with the key/value roles swapped relative
        // to the first variant: b = "2&a=1"
        "?b=2%26a%3D1",
    ];
    let identities: Vec<CameraId> = variants
        .iter()
        .map(|query| {
            CameraId::from_rtsp_url("node-a", &format!("{base}{query}"), &salt)
                .unwrap_or_else(|error| panic!("{query} must be a valid rtsp:// URL: {error}"))
        })
        .collect();
    for (i, left) in identities.iter().enumerate() {
        for (j, right) in identities.iter().enumerate() {
            if i == j {
                continue;
            }
            assert_ne!(
                left, right,
                "variant {:?} and variant {:?} decode to genuinely different pair-sets and must resolve to different identities, got the same identity for both",
                variants[i], variants[j]
            );
        }
    }
}

// ---------------------------------------------------------------------
// Query digest salt (keyed digest, never an unkeyed fingerprint)
// ---------------------------------------------------------------------

/// The same query, digested under two different salts, must resolve to two
/// different identities — proof the digest is genuinely keyed (an
/// attacker who can only observe the identity, never the salt, cannot
/// reproduce it). Proven strictly by inequality: no expected identity
/// string is named anywhere, so this cannot regress into embedding a
/// digest-shaped literal.
#[test]
fn from_rtsp_url_query_digest_differs_under_a_different_salt() {
    let salt_one = CameraQuerySalt::generate();
    let salt_two = CameraQuerySalt::generate();
    let url = "rtsp://camera.local:554/mainstream?channel=1";
    let under_salt_one = CameraId::from_rtsp_url("node-a", url, &salt_one)
        .expect("a valid rtsp:// URL must be accepted");
    let under_salt_two = CameraId::from_rtsp_url("node-a", url, &salt_two)
        .expect("a valid rtsp:// URL must be accepted");
    assert_ne!(
        under_salt_one, under_salt_two,
        "the identical query digested under two different salts must resolve to two different identities, or the digest would not genuinely be keyed by the salt"
    );

    // Companion stability check: the SAME salt reused for the SAME query
    // must still resolve to the SAME identity, so this cannot be satisfied
    // by a stub that makes every call return a fresh, never-equal identity.
    let under_salt_one_again = CameraId::from_rtsp_url("node-a", url, &salt_one)
        .expect("a valid rtsp:// URL must be accepted");
    assert_eq!(under_salt_one, under_salt_one_again);
}

/// A query-less URL never invokes the digest at all, so it must resolve to
/// the SAME identity regardless of which salt is in play — the salt only
/// ever affects a URL that actually carries a query.
#[test]
fn from_rtsp_url_query_less_identity_is_unaffected_by_the_salt() {
    let salt_one = CameraQuerySalt::generate();
    let salt_two = CameraQuerySalt::generate();
    let url = "rtsp://camera.local:554/mainstream";
    let under_salt_one =
        CameraId::from_rtsp_url("node-a", url, &salt_one).expect("a valid rtsp:// URL");
    let under_salt_two =
        CameraId::from_rtsp_url("node-a", url, &salt_two).expect("a valid rtsp:// URL");
    assert_eq!(
        under_salt_one, under_salt_two,
        "a query-less URL contributes no digest component, so it must not vary with the salt"
    );
}

/// `CameraQuerySalt::load_or_generate` persists a salt beside other node
/// state and returns the SAME salt (via its effect on the digest) across
/// repeated loads of the same `data_dir` — the file is written once, not
/// regenerated on every call.
#[test]
fn camera_query_salt_load_or_generate_is_stable_across_repeated_loads() {
    let data_dir = tempfile::tempdir().expect("create a temp data dir");
    let first_load =
        CameraQuerySalt::load_or_generate(data_dir.path()).expect("first load must succeed");
    let second_load =
        CameraQuerySalt::load_or_generate(data_dir.path()).expect("second load must succeed");
    let url = "rtsp://camera.local:554/mainstream?channel=1";
    let identity_after_first_load = CameraId::from_rtsp_url("node-a", url, &first_load)
        .expect("a valid rtsp:// URL must be accepted");
    let identity_after_second_load = CameraId::from_rtsp_url("node-a", url, &second_load)
        .expect("a valid rtsp:// URL must be accepted");
    assert_eq!(
        identity_after_first_load, identity_after_second_load,
        "loading the same data_dir twice must yield the same persisted salt, proven by the identical resulting identity — a fresh random salt on every load would make the persisted file pointless"
    );
}

/// The documented consequence: wiping the persisted salt file re-keys a
/// query-distinguished camera's identity the next time it resolves.
/// Proven by inequality against the pre-wipe identity, never by naming a
/// rendered digest.
#[test]
fn camera_query_salt_wiping_the_file_rekeys_query_distinguished_identity() {
    let data_dir = tempfile::tempdir().expect("create a temp data dir");
    let before_wipe =
        CameraQuerySalt::load_or_generate(data_dir.path()).expect("first load must succeed");
    let url = "rtsp://camera.local:554/mainstream?channel=1";
    let identity_before_wipe = CameraId::from_rtsp_url("node-a", url, &before_wipe)
        .expect("a valid rtsp:// URL must be accepted");

    std::fs::remove_file(data_dir.path().join("camera-query-salt"))
        .expect("the salt file must exist after the first load_or_generate call");

    let after_wipe =
        CameraQuerySalt::load_or_generate(data_dir.path()).expect("post-wipe load must succeed");
    let identity_after_wipe = CameraId::from_rtsp_url("node-a", url, &after_wipe)
        .expect("a valid rtsp:// URL must be accepted");

    assert_ne!(
        identity_before_wipe, identity_after_wipe,
        "wiping the persisted salt file must re-key a query-distinguished camera's identity on next resolution — the documented, accepted consequence of node-state loss, not a bug"
    );
}
