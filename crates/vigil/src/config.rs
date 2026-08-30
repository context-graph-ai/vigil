use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::camera_track::{CameraId, CameraQuerySalt};
use crate::secret::Secret;
use crate::settings::{SettingHandle, SettingSpec, SettingSurfaces, SettingsRegistry};
use crate::site_channel::ConnectionEndpoint;

/// `Debug`-format an `Option<String>` URL-shaped field through
/// [`redact_url_userinfo`], so a hand-written `Debug` impl never has to
/// remember to redact at each field individually — it just routes every
/// URL-shaped `Option<String>` field through this one function. Renders
/// exactly like the ordinary `Option<String>` Debug output
/// (`Some("...")` / `None`) it stands in for, just with the PASSWORD (and
/// any query/fragment) removed — the username stays visible, via
/// [`PreformattedDebug`].
fn debug_redacted_url(value: &Option<String>) -> PreformattedDebug {
    PreformattedDebug(match value {
        Some(url) => format!(
            "Some({:?})",
            redact_url_userinfo(url, UrlRedactionPolicy::Display)
        ),
        None => "None".to_string(),
    })
}

/// `Debug`-format an `Option<String>` username field PLAINLY — the real
/// value, not a placeholder. Owner ruling: a username is a DIAGNOSTIC, not
/// a secret, on every surface in this codebase (the same rule
/// [`redact_url_userinfo`] applies to a URL's userinfo — it keeps the
/// username and redacts only the password); a separate `username` config
/// field follows the identical rule. This function exists (rather than
/// just using the field directly) so every hand-written `Debug` impl below
/// prints usernames through one single, named, greppable point instead of
/// several ad hoc ones that could silently drift apart.
fn debug_visible_username(value: &Option<String>) -> PreformattedDebug {
    PreformattedDebug(format!("{value:?}"))
}

/// `Debug`-format an `Option<String>` fabric enrollment ticket as fully
/// redacted. A fabric ticket is an enrollment credential — printing it
/// raw while the URL/username fields on the SAME hand-written `Debug`
/// impl are redacted would make the impl look safe while still
/// disclosing a credential, which is worse than the derive it replaced.
fn debug_redacted_ticket(value: &Option<String>) -> PreformattedDebug {
    PreformattedDebug(match value {
        Some(_) => "Some(\"<redacted>\")".to_string(),
        None => "None".to_string(),
    })
}

/// Renders as exactly the string it wraps, with no added quoting — the
/// splice point that lets [`debug_redacted_url`]'s pre-formatted, already
/// redacted rendering sit inside an ordinary `Formatter::debug_struct`
/// field call.
struct PreformattedDebug(String);

impl fmt::Debug for PreformattedDebug {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The four supported camera source kinds (native RTSP, USB/UVC, MIPI
/// CSI-2, HTTP/MJPEG). USB/CSI/MJPEG identity is durable-hardware/endpoint
/// keyed; the entry's `name` never participates in identity — it is
/// display-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CameraSourceKind {
    Rtsp,
    Usb,
    Csi,
    Mjpeg,
}

impl CameraSourceKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CameraSourceKind::Rtsp => "rtsp",
            CameraSourceKind::Usb => "usb",
            CameraSourceKind::Csi => "csi",
            CameraSourceKind::Mjpeg => "mjpeg",
        }
    }
}

impl std::fmt::Display for CameraSourceKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why one source-kind field failed validation. Four distinct outcomes —
/// never coerced into each other, and an explicit value is never silently
/// defaulted away. All four are real, live outcomes of
/// [`resolve_camera_source_kind`] against the REAL `config::load` path
/// (exercised end to end by `tests/camera_config_schema.rs`): `Omitted`
/// and `Empty` are kept distinct because "never touched the field" and
/// "typed an empty value" are different operator mistakes; `Invalid`
/// (fails [`is_valid_rtsp_url`]/[`is_valid_mjpeg_url`]/[`is_durable_hardware_identity`]) and
/// `Unavailable` (parses fine, but no artifact today carries that kind)
/// are kept distinct because they call for different fixes — fix a typo
/// versus install a different artifact — and conflating them would send
/// an operator who merely mistyped a URL off to install different
/// hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldOutcome {
    /// The field was never supplied by any config source.
    Omitted,
    /// The field was supplied as an empty string.
    Empty,
    /// The field was supplied but is not a valid value for its kind (e.g.
    /// an unparsable URL, a malformed hardware identity).
    Invalid,
    /// The field names a real-shaped value this artifact cannot currently
    /// reach or carry (e.g. `unsupported_by_this_artifact`).
    Unavailable,
}

/// Which single [`FieldOutcome`] applies to one field's value, given
/// whether it was supplied, whether it validates for its declared kind, and
/// whether the running artifact can carry that kind at all. A pure
/// classification — no I/O — called from [`resolve_camera_source_kind`]
/// with real validators, so this is not a helper proving something about
/// itself: the real loader produces all four outcomes through this exact
/// function.
///
/// Returns `None` for a field that is supplied, valid, AND reachable: that
/// is not a failure outcome at all, so there is genuinely nothing to
/// report — treating "fully usable" as a fifth [`FieldOutcome`] variant
/// would force every caller to handle a case that is not an outcome of
/// anything going wrong.
pub(crate) fn classify_field_outcome(
    value: Option<&str>,
    is_valid_for_kind: bool,
    artifact_supports_kind: bool,
) -> Option<FieldOutcome> {
    match value {
        None => Some(FieldOutcome::Omitted),
        Some("") => Some(FieldOutcome::Empty),
        Some(_) if !is_valid_for_kind => Some(FieldOutcome::Invalid),
        Some(_) if !artifact_supports_kind => Some(FieldOutcome::Unavailable),
        Some(_) => None,
    }
}

/// An immutable declaration of which [`CameraSourceKind`]s a running
/// artifact can carry, constructed once and passed into configuration
/// validation as an explicit value — never populated through global
/// mutable registration. That is the deliberate shape: four adapter lanes
/// (native RTSP today; USB, CSI, and MJPEG as their capture/encode paths
/// land) will each need to declare their own kind supported, and a global
/// mutable registry would make "did lane X register before validation ran"
/// an initialization-order / first-writer-wins hazard. An immutable value
/// built once and threaded through has no such ordering to get wrong, and
/// a test can construct one declaring any set of kinds supported without
/// touching any process-global state.
#[derive(Debug, Clone, Default)]
pub(crate) struct SourceKindCapabilityRegistry {
    supported: Vec<CameraSourceKind>,
}

impl SourceKindCapabilityRegistry {
    pub(crate) fn new(supported: impl IntoIterator<Item = CameraSourceKind>) -> Self {
        Self {
            supported: supported.into_iter().collect(),
        }
    }

    pub(crate) fn supports(&self, kind: CameraSourceKind) -> bool {
        self.supported.contains(&kind)
    }
}

/// The real artifact's own capability declaration: only native RTSP has a
/// wired capture/encode path today (every adapter kind is honestly
/// rejected at configuration load, `unsupported_by_this_artifact`, because
/// no capture/encode path for it exists yet — the shared encoder seam in
/// `crate::encode` has no producer wired to it). This is the value
/// [`load`] passes into [`resolve_camera_source_kind`].
pub(crate) fn artifact_source_kind_capabilities() -> SourceKindCapabilityRegistry {
    SourceKindCapabilityRegistry::new([CameraSourceKind::Rtsp])
}

/// Which surface a redacted URL is destined for. The two policies differ
/// ONLY in how much of the userinfo survives:
///
/// - [`UrlRedactionPolicy::Display`] — logs, `Debug` output, and
///   parse-error text. Owner ruling: a username is a DIAGNOSTIC, not a
///   secret, and stays visible on every display surface; only the
///   password (and query/fragment) are secrets there.
/// - [`UrlRedactionPolicy::Persistence`] — anything written into the
///   memory graph (runtime-memory/provenance). The username is NOT a
///   secret, but it is still an identity value the owner ruled must not
///   be persisted alongside the camera's durable record: the ENTIRE
///   userinfo, username and password both, is stripped.
///
/// Both policies always drop the query/fragment (a common place for a
/// token, e.g. `?token=...`), independent of whether userinfo was present
/// at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UrlRedactionPolicy {
    /// Keep the username, redact only the password. Logs, `Debug`, and
    /// parse-error text — never anything persisted into the memory graph.
    Display,
    /// Strip the whole userinfo, username and password both. Anything
    /// persisted into the memory graph (runtime-memory/provenance) —
    /// never a log line or `Debug` impl.
    Persistence,
}

/// Redact a URL's userinfo, under an explicit [`UrlRedactionPolicy`],
/// before the URL becomes displayable text OR persisted state anywhere on
/// the config surface — an error message, log line, receipt, identity, or
/// a memory-graph property. This is the single way a URL-shaped config
/// value becomes displayable/persisted text on this path: "secrets never
/// enter argv, logs, receipts, provenance, identity, or locally served
/// URLs" is a binding product line, and a password embedded in a URL
/// string (the common `scheme://user:pass@host/...` shape — an RTSP
/// camera URL and an ESP32-CAM class `mjpeg_url` both carry credentials
/// exactly this way) is exactly as much a secret as an explicit
/// `password` field — the `Secret` wrapper protects only the latter, so a
/// caller that echoes a raw URL string bypasses that protection entirely
/// unless it goes through this first.
///
/// Under [`UrlRedactionPolicy::Display`]: keeps scheme, host, port, path,
/// AND username — what makes a refusal or log line actionable, including
/// which account a camera is configured to use — and drops only the
/// password AND the query/fragment:
/// `rtsp://vigil:hunter2@camera.local:554/substream` renders as
/// `rtsp://vigil:<redacted>@camera.local:554/substream`. A username with
/// no password (`rtsp://vigil@camera.local/substream`) passes through
/// UNCHANGED — there is no password to redact, and the username is not a
/// secret on this surface.
///
/// Under [`UrlRedactionPolicy::Persistence`]: the same example renders as
/// `rtsp://camera.local:554/substream` — no `user:` / `user:<redacted>:`
/// prefix at all, whether or not a password was present.
///
/// Under EITHER policy, the query and fragment are ALWAYS stripped,
/// independent of whether userinfo was present at all:
/// `rtsp://host/stream?token=abc123` must never survive into an error,
/// `Debug` rendering, or persisted property, even though that URL has no
/// `user:pass@` shape at all. A value with no userinfo and no
/// query/fragment passes through byte-for-byte unchanged under either
/// policy (including a non-URL identity like a USB/CSI hardware string,
/// which never has an `@`, `?`, or `#`).
///
/// Handles three authority shapes, not only a fully-schemed URL: a
/// standard `scheme://user:pass@host/...` value; a scheme-relative
/// `//user:pass@host/...` value (no scheme, but still `//`-prefixed); and
/// a bare `user:pass@host/...` authority with neither — this last shape
/// is exactly the bypass a naive "only handle `scheme://`" redaction
/// leaves open, since a value entering this function should never be
/// ASSUMED well-formed (this function is the redaction backstop; the real
/// gate is that [`config::load`](load) refuses a URL-shaped field that
/// does not parse as a real, correctly-schemed URL BEFORE anything about
/// it is ever echoed at all — see [`is_valid_rtsp_url`]). In every shape,
/// the userinfo boundary is found as the LAST `@` within the AUTHORITY
/// portion only — the substring up to the first `/`, `?`, or `#` after
/// any scheme/`//` prefix — never the first or last `@` in the whole
/// string, either of which a query can contain legitimately (an email
/// address in a query value, for instance) and so can fool a naive
/// whole-string split; the point is moot for the query itself now that it
/// is unconditionally dropped, but the same bounded scan also protects
/// the PATH, which is kept. Within the userinfo itself, when the policy
/// keeps a username, the password boundary is the FIRST `:` (the standard
/// `user:pass` split) — a username is never assumed free of `:` in
/// general URL syntax, but the first colon is always where the password
/// begins.
pub(crate) fn redact_url_userinfo(value: &str, policy: UrlRedactionPolicy) -> String {
    let (prefix, rest) = if let Some((scheme, after_scheme)) = value.split_once("://") {
        (format!("{scheme}://"), after_scheme)
    } else if let Some(after_slashes) = value.strip_prefix("//") {
        ("//".to_string(), after_slashes)
    } else {
        (String::new(), value)
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path_and_beyond) = rest.split_at(authority_end);
    let path_end = path_and_beyond
        .find(['?', '#'])
        .unwrap_or(path_and_beyond.len());
    let path = &path_and_beyond[..path_end];
    match authority.rsplit_once('@') {
        Some((userinfo, host)) => match policy {
            // Persistence: the whole userinfo is dropped, username and
            // password both — no `@` survives into the reassembled value.
            UrlRedactionPolicy::Persistence => format!("{prefix}{host}{path}"),
            UrlRedactionPolicy::Display => {
                let visible_userinfo = match userinfo.split_once(':') {
                    // A password is present: keep the username, redact only
                    // the password.
                    Some((username, _password)) => format!("{username}:<redacted>"),
                    // Username with no password: nothing to redact.
                    None => userinfo.to_string(),
                };
                format!("{prefix}{visible_userinfo}@{host}{path}")
            }
        },
        // No userinfo: the reassembled `prefix + authority + path` is
        // always byte-identical to the original `value` whenever nothing
        // was actually stripped (no scheme/`//` prefix, no query/
        // fragment) — e.g. a non-URL identity like a USB/CSI hardware
        // string, which has neither. When a query/fragment WAS present,
        // this reassembly is exactly what drops it.
        None => format!("{prefix}{authority}{path}"),
    }
}

/// A fixed, arbitrary node scope used ONLY to drive [`CameraId`]
/// construction for validation purposes (see [`is_valid_rtsp_url`],
/// [`is_valid_mjpeg_url`], [`is_durable_hardware_identity`] below) — never
/// stored, never compared, never surfaced. The identity these functions
/// build is thrown away; only whether construction succeeded matters.
const ENDPOINT_VALIDATION_NODE: &str = "config-validation";

/// Whether `value` is a real, valid `rtsp://`/`rtsps://` endpoint — by
/// construction, the SAME question [`CameraId::from_rtsp_url`] itself
/// answers, because this calls it directly rather than re-deriving a
/// parallel scheme-only check. A prior scheme-only validator accepted a
/// hostless opaque URL like `rtsp:camera/stream` (a valid scheme, but no
/// `//host` for `url::Url::parse` to expose): [`CameraId::from_rtsp_url`]
/// requires a host to build identity from, so that value could load as a
/// "camera" no identity could ever be constructed for — and
/// [`reject_duplicate_camera_analysis_endpoints`]'s own defensive `Ok(..)`
/// check then silently skipped it from duplicate comparison too. Routing
/// load-time validation through the identical identity-construction call
/// closes both gaps by construction: load time and identity time can no
/// longer answer "is this endpoint valid" differently. This is the ONLY
/// gate a URL-shaped camera field passes before ANYTHING about it — even
/// a redacted form — is echoed anywhere: [`redact_url_userinfo`] is a
/// backstop for a value that reaches it, but the real fix for the
/// userinfo-leak class of defect is that a value failing this check is
/// classified `FieldOutcome::Invalid` and its error text never carries the
/// raw value at all (see [`resolve_camera_source_kind`]).
///
/// The query digest's salt only affects the *value* of a constructed
/// identity, never whether construction succeeds — so, exactly like
/// `ENDPOINT_VALIDATION_NODE`, an ephemeral, never-persisted
/// [`CameraQuerySalt::generate`] is enough here.
fn is_valid_rtsp_url(value: &str) -> bool {
    CameraId::from_rtsp_url(
        ENDPOINT_VALIDATION_NODE,
        value,
        &CameraQuerySalt::generate(),
    )
    .is_ok()
}

/// The MJPEG counterpart to [`is_valid_rtsp_url`]: whether `value` is a
/// real, valid http/https endpoint (deliberately not spelling out the
/// scheme-plus-separator literal — see [`invalid_value_hint`]'s own note),
/// answered by [`CameraId::from_mjpeg_url`] itself rather than a parallel
/// check, on the identical host-required, credential-stripping terms.
fn is_valid_mjpeg_url(value: &str) -> bool {
    CameraId::from_mjpeg_url(
        ENDPOINT_VALIDATION_NODE,
        value,
        &CameraQuerySalt::generate(),
    )
    .is_ok()
}

/// Whether `value` is a durable hardware identity (vendor:product:serial
/// for USB, a stable module identity for CSI) rather than a transient
/// `/dev/videoN`-style device path — the intent's own USB/CSI identity
/// contract ("Vigil resolves the durable identity when the platform
/// supplies one... a bare enumeration index is insufficient unless the
/// platform guarantees its stability"). Answered by
/// [`CameraId::from_usb`] itself (identical, whichever of `from_usb` /
/// `from_csi` is asked — both delegate to the same trim-then-`/dev/`-check
/// durability rule) rather than a parallel `starts_with("/dev/")` check: a
/// prior parallel check compared the RAW, untrimmed value, so a
/// whitespace-padded value like `" /dev/video0"` passed load-time
/// validation (it does not start with `/dev/`) while identity construction
/// — which trims first — would still have refused it as transient. Routing
/// through the real constructor closes that gap by construction.
fn is_durable_hardware_identity(value: &str) -> bool {
    CameraId::from_usb(ENDPOINT_VALIDATION_NODE, value).is_ok()
}

/// The actionable, camera-named rejection message for a source kind this
/// artifact cannot carry. Carries the operator's own declared value
/// (`declared_value`, e.g. the `usb_device`/`csi_module`/`mjpeg_url`
/// string) so the refusal is checkable against what was actually typed
/// rather than only naming the kind in the abstract — but always through
/// [`redact_url_userinfo`] first: a USB/CSI hardware identity is untouched
/// by that redaction (it never contains userinfo), while a credentialed
/// `mjpeg_url`/`rtsp_url`/`live_rtsp_url` has its userinfo stripped before
/// it can reach this message, this function's caller's log line, or any
/// other display of the returned string. Every caller of this function
/// reaches it only after [`resolve_camera_source_kind`] has already
/// confirmed the field parses as a real URL (`FieldOutcome::Invalid` is
/// checked and refused BEFORE this message is ever built), so
/// `declared_value` here is always well-formed for
/// [`redact_url_userinfo`] to redact correctly.
pub(crate) fn unsupported_source_kind_message(
    camera_name: &str,
    kind: CameraSourceKind,
    declared_value: &str,
) -> String {
    let declared_value = redact_url_userinfo(declared_value, UrlRedactionPolicy::Display);
    format!(
        "camera '{camera_name}' declares a {kind} source '{declared_value}' (unsupported_by_this_artifact): this artifact carries no {kind} capture/encode path yet; install a supported artifact or configure a native RTSP source",
    )
}

/// One actionable configuration error: names the camera and the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CameraConfigError {
    pub(crate) camera_name: String,
    pub(crate) message: String,
}

/// One field's own hint for what a valid value looks like, used ONLY when
/// building an [`FieldOutcome::Invalid`] refusal — the hint never repeats
/// the operator's actual (malformed, possibly credential-bearing) input.
fn invalid_value_hint(kind: CameraSourceKind) -> &'static str {
    match kind {
        CameraSourceKind::Rtsp => "rtsp_url must be a valid rtsp:// or rtsps:// URL",
        CameraSourceKind::Usb => {
            "usb_device must be a durable vendor:product:serial hardware identity, not a transient /dev/ device path"
        }
        CameraSourceKind::Csi => {
            "csi_module must be a durable hardware/module identity, not a transient /dev/ device path"
        }
        // Deliberately says "http/https" rather than spelling out the
        // scheme-plus-separator literal: this crate's own
        // source-runtime-fetch guard (tests/runtime_packages_manifest.rs)
        // flags that literal anywhere in crates/vigil/src, comments
        // included, as a possible runtime network fetch.
        CameraSourceKind::Mjpeg => "mjpeg_url must be a valid http/https URL",
    }
}

fn invalid_field_error(camera_name: &str, kind: CameraSourceKind) -> CameraConfigError {
    CameraConfigError {
        camera_name: camera_name.to_string(),
        message: format!(
            "camera '{camera_name}' declares an invalid {kind} value ({}); the value itself is not repeated here because a malformed URL can carry credentials",
            invalid_value_hint(kind)
        ),
    }
}

/// One source-kind field candidate: its kind, its raw declared value (if
/// any), and the pure validator that decides whether a declared value is
/// well-formed for this kind.
type SourceFieldCandidate<'a> = (CameraSourceKind, Option<&'a str>, fn(&str) -> bool);

/// Validate that a camera entry declares exactly one source kind, using
/// the REAL per-field [`FieldOutcome`] classification (URL fields
/// validated through [`is_valid_rtsp_url`]/[`is_valid_mjpeg_url`]; USB/CSI fields through
/// [`is_durable_hardware_identity`]) — not a bare non-empty check. Missing,
/// empty, invalid, and conflicting source fields are all rejected as
/// distinct [`CameraConfigError`]s naming the camera and the fix, never
/// silently picked between or coerced into one another.
pub(crate) fn resolve_camera_source_kind(
    entry: &CameraEntryPartial,
    registry: &SourceKindCapabilityRegistry,
) -> Result<CameraSourceKind, CameraConfigError> {
    let candidates: [SourceFieldCandidate<'_>; 4] = [
        (
            CameraSourceKind::Rtsp,
            entry.rtsp_url.as_deref(),
            is_valid_rtsp_url,
        ),
        (
            CameraSourceKind::Usb,
            entry.usb_device.as_deref(),
            is_durable_hardware_identity,
        ),
        (
            CameraSourceKind::Csi,
            entry.csi_module.as_deref(),
            is_durable_hardware_identity,
        ),
        (
            CameraSourceKind::Mjpeg,
            entry.mjpeg_url.as_deref(),
            is_valid_mjpeg_url,
        ),
    ];

    let classified: Vec<(CameraSourceKind, Option<&str>, Option<FieldOutcome>)> = candidates
        .into_iter()
        .map(|(kind, value, validator)| {
            let is_valid = value.is_none_or(|v| v.is_empty() || validator(v));
            let outcome = classify_field_outcome(value, is_valid, registry.supports(kind));
            (kind, value, outcome)
        })
        .collect();

    // An explicit empty value is checked FIRST, before anything else on the
    // entry — including a sibling field that is itself fully valid. An
    // operator who wrote `usb_device = ""` typed something, even if it was
    // wrong, and that mistake is real and actionable regardless of whether
    // `rtsp_url` on the same entry happens to be well-formed: silently
    // accepting the entry because ONE field validated would throw away the
    // signal that another field was explicitly left blank. Named per field
    // (not a single generic sentence) so two different empty fields beside
    // the identical valid sibling produce distinguishable error text.
    let empty_fields: Vec<CameraSourceKind> = classified
        .iter()
        .filter(|(_, _, outcome)| matches!(outcome, Some(FieldOutcome::Empty)))
        .map(|(kind, _, _)| *kind)
        .collect();
    if !empty_fields.is_empty() {
        let field_names = empty_fields
            .iter()
            .map(|kind| match kind {
                CameraSourceKind::Rtsp => "rtsp_url",
                CameraSourceKind::Usb => "usb_device",
                CameraSourceKind::Csi => "csi_module",
                CameraSourceKind::Mjpeg => "mjpeg_url",
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CameraConfigError {
            camera_name: entry.name.clone(),
            message: format!(
                "camera '{}' declares an empty source field ({}): remove the field entirely, or set exactly one of rtsp_url, usb_device, csi_module, mjpeg_url to a real value",
                entry.name, field_names
            ),
        });
    }

    let attempted: Vec<&(CameraSourceKind, Option<&str>, Option<FieldOutcome>)> = classified
        .iter()
        .filter(|(_, _, outcome)| !matches!(outcome, Some(FieldOutcome::Omitted)))
        .collect();

    match attempted.as_slice() {
        [] => Err(CameraConfigError {
            camera_name: entry.name.clone(),
            message: format!(
                "camera '{}' declares no source: set exactly one of rtsp_url, usb_device, csi_module, mjpeg_url",
                entry.name
            ),
        }),
        [(kind, value, outcome)] => match outcome {
            Some(FieldOutcome::Invalid) => Err(invalid_field_error(&entry.name, *kind)),
            Some(FieldOutcome::Unavailable) => Err(CameraConfigError {
                camera_name: entry.name.clone(),
                message: unsupported_source_kind_message(
                    &entry.name,
                    *kind,
                    value.expect("Unavailable is only reached for a Some(_) value"),
                ),
            }),
            None => Ok(*kind),
            Some(FieldOutcome::Omitted) | Some(FieldOutcome::Empty) => {
                unreachable!("Omitted/Empty were already filtered out of `attempted`")
            }
        },
        many => Err(CameraConfigError {
            camera_name: entry.name.clone(),
            message: format!(
                "camera '{}' declares more than one source ({}): set exactly one of rtsp_url, usb_device, csi_module, mjpeg_url",
                entry.name,
                many.iter()
                    .map(|(kind, _, _)| kind.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
    }
}

/// Validate the optional `live_rtsp_url` field independently of kind
/// resolution above: it is never a source-kind selector on its own (it
/// only ever accompanies `rtsp_url`), but it is exactly as URL-shaped and
/// exactly as capable of carrying an embedded credential, so it gets the
/// identical Invalid-without-raw-echo treatment.
fn validate_live_rtsp_url(camera_name: &str, value: Option<&str>) -> Result<(), CameraConfigError> {
    match value {
        None | Some("") => Ok(()),
        Some(v) if is_valid_rtsp_url(v) => Ok(()),
        Some(_) => Err(CameraConfigError {
            camera_name: camera_name.to_string(),
            message: format!(
                "camera '{camera_name}' declares an invalid live_rtsp_url value (live_rtsp_url must be a valid rtsp:// or rtsps:// URL); the value itself is not repeated here because a malformed URL can carry credentials",
            ),
        }),
    }
}

/// Reject two `[[cameras]]` entries whose ANALYSIS endpoint (`rtsp_url`
/// alone — `live_rtsp_url` never participates, matching the entry-level
/// role-congruence rule) canonically collides, even when the two declared
/// `rtsp_url` strings are byte-different (mixed host case, an explicit vs.
/// omitted default port). This is a configuration error, not two silently
/// accepted cameras: without it, a copy/paste mistake or an operator
/// re-declaring the same NVR channel under two names silently drops one of
/// the two cameras at capture time. Comparison goes through
/// [`CameraId::from_rtsp_url`] (the same canonical-endpoint rule pinned in
/// `crate::camera_track`), never a raw string comparison, and the error
/// names both colliding entries without ever echoing the (possibly
/// credentialed) URL itself.
fn reject_duplicate_camera_analysis_endpoints(
    cameras: &[CameraEntry],
) -> Result<(), CameraConfigError> {
    // A fixed, arbitrary node scope: duplicate detection is entirely
    // within one config load, so the node component of `CameraId` (which
    // only exists to keep two different nodes' cameras from colliding) is
    // irrelevant here as long as it is applied identically to every entry.
    const DUPLICATE_DETECTION_NODE: &str = "config-load";
    // One salt for the whole pass, generated once and reused for every
    // entry: the query digest is keyed (see `CameraQuerySalt`), so
    // comparing identities built under two DIFFERENT salts would make
    // even the identical query look like two different cameras. The salt
    // value itself is as throwaway as `DUPLICATE_DETECTION_NODE` — only
    // equality among entries within this one pass matters, never its
    // value across process runs.
    let duplicate_detection_salt = CameraQuerySalt::generate();
    let mut seen: Vec<(&str, CameraId)> = Vec::new();
    for camera in cameras {
        let Some(rtsp_url) = camera.rtsp_url.as_deref() else {
            continue;
        };
        // `resolve_camera_source_kind` already validated this value as a
        // real rtsp:// or rtsps:// URL before this point, so construction
        // cannot fail here in practice; an entry that somehow fails is
        // simply not a duplicate-detection candidate rather than a hard
        // error, since URL validity is already enforced elsewhere.
        let Ok(canonical) = CameraId::from_rtsp_url(
            DUPLICATE_DETECTION_NODE,
            rtsp_url,
            &duplicate_detection_salt,
        ) else {
            continue;
        };
        if let Some((other_name, _)) = seen.iter().find(|(_, id)| *id == canonical) {
            return Err(CameraConfigError {
                camera_name: camera.name.clone(),
                message: format!(
                    "camera '{other_name}' and camera '{}' declare the same analysis endpoint (duplicate camera): give each camera its own distinct rtsp_url, or remove one of the two entries",
                    camera.name
                ),
            });
        }
        seen.push((&camera.name, canonical));
    }
    Ok(())
}

/// One camera entry in the multi-camera list.
///
/// `Debug` is hand-written, not derived: `rtsp_url`/`live_rtsp_url`/
/// `mjpeg_url` are exactly as capable of carrying an embedded credential
/// as the separate `password` field, so this follows the same discipline
/// `Secret` establishes — the TYPE makes disclosure impossible, rather
/// than relying on every future caller to remember to redact before
/// printing.
#[derive(Clone)]
pub(crate) struct CameraEntry {
    pub(crate) name: String,
    pub(crate) rtsp_url: Option<String>,
    pub(crate) live_rtsp_url: Option<String>,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<Secret>,
    pub(crate) usb_device: Option<String>,
    pub(crate) csi_module: Option<String>,
    pub(crate) mjpeg_url: Option<String>,
    /// The source kind this entry resolved to at load time
    /// ([`resolve_camera_source_kind`]), carried alongside the entry so a
    /// later consumer (the `/health` by-kind surface) never has to
    /// re-derive it from the raw fields. `None` only for the pre-OSS
    /// legacy zero-source gap (see `load`'s legacy-fallback branch), where
    /// no source field was declared at all — such an entry has no real
    /// kind to report.
    pub(crate) source_kind: Option<CameraSourceKind>,
}

impl fmt::Debug for CameraEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CameraEntry")
            .field("name", &self.name)
            .field("rtsp_url", &debug_redacted_url(&self.rtsp_url))
            .field("live_rtsp_url", &debug_redacted_url(&self.live_rtsp_url))
            .field("username", &debug_visible_username(&self.username))
            .field("password", &self.password)
            .field("usb_device", &self.usb_device)
            .field("csi_module", &self.csi_module)
            .field("mjpeg_url", &debug_redacted_url(&self.mjpeg_url))
            .field("source_kind", &self.source_kind)
            .finish()
    }
}

/// A camera's `/health`-surface identity: its display name and resolved
/// source kind only — never the raw URL/username/password fields
/// [`CameraEntry`] carries — so the health render path never holds
/// anything that needs redaction at request time.
#[derive(Debug, Clone)]
pub(crate) struct CameraHealthEntry {
    pub(crate) name: String,
    pub(crate) kind: Option<CameraSourceKind>,
}

impl CameraEntry {
    pub(crate) fn health_entry(&self) -> CameraHealthEntry {
        CameraHealthEntry {
            name: self.name.clone(),
            kind: self.source_kind,
        }
    }
}

/// `Debug` is hand-written for the same reason as [`CameraEntry`]: the
/// legacy single-camera `rtsp_url` field is exactly as URL-shaped and
/// exactly as capable of carrying an embedded credential.
#[derive(Clone)]
pub(crate) struct RuntimeConfig {
    /// What the file surface — a named config file, or the add-on options —
    /// asserted on its OWN, before the command line and the environment were
    /// merged over it. The store is the source of truth and input surfaces are
    /// authors that write into it, so what a surface says has to survive the
    /// merge that produces the resolved values: a merged value cannot say
    /// which surface it came from, and per-surface records are the whole
    /// reason a config file never silently reverts a `vigil settings` change
    /// at the next boot.
    pub(crate) file_surface: Option<SurfaceAssertions>,
    /// What the startup options assert. Its own surface with its own record:
    /// a flag the service passes and a `vigil settings` change a person made
    /// later are two different people speaking, and collapsing them into one
    /// record is what would let the service's command line silently revert
    /// that change at every restart.
    pub(crate) startup_surface: Option<SurfaceAssertions>,
    pub(crate) data_dir: PathBuf,
    pub(crate) store_path: PathBuf,
    pub(crate) health_port: u16,
    pub(crate) review_port: u16,
    pub(crate) site_name: String,
    /// First camera's name — retained for backward-compat with log_startup and
    /// single-camera deployments.
    pub(crate) camera_name: String,
    /// First camera's RTSP URL — retained for backward compat.
    pub(crate) rtsp_url: Option<String>,
    pub(crate) rtsp_username: Option<String>,
    pub(crate) rtsp_password: Option<Secret>,
    pub(crate) detector_model_id: String,
    pub(crate) detector_model_path: Option<PathBuf>,
    pub(crate) detector_confidence_threshold: f64,
    pub(crate) detector_sample_frames: usize,
    pub(crate) detector_stationary_interval_secs: u64,
    /// Canonical multi-camera list.  Always contains at least one entry (the
    /// single camera_name/rtsp_url for backward compat).
    pub(crate) cameras: Vec<CameraEntry>,
    /// Outbound connection an integration adapter may use, present when a
    /// broker is configured (e.g. via HA Supervisor MQTT service or env vars
    /// MQTT_HOST / MQTT_PORT).
    pub(crate) mqtt: Option<ConnectionEndpoint>,
    /// This node's stable service identifier: the MQTT topic namespace and the
    /// Home Assistant device id. Carries what the deployment supplied and is
    /// otherwise EMPTY until startup resolves the persisted identity — which
    /// is derived once, at first start, and never recomputed from the site
    /// name again, because recomputing it renames the device and orphans the
    /// history hanging off the old one.
    pub(crate) service_id: String,
    /// Recognition: crop → embed → enroll → match. Off unless a weights
    /// directory is configured; a configured-but-missing weights dir fails
    /// loud at startup, never silently.
    pub(crate) recognition: crate::recognition::RecognitionConfig,
    /// The class indices every detector this runtime builds emits, resolved
    /// from the detection class setting in the store once the store is open.
    /// Until then it carries Vigil's own default subset, which is what a run
    /// with no store behind it keeps. It lives here so every construction site
    /// reads ONE resolved value rather than each deriving its own — the shape
    /// that let recognition decide the detector's class breadth.
    pub(crate) detector_class_indices: Vec<usize>,
    /// How many captured segments the detector queue holds — the OPERATOR
    /// control, resolved from the store. Its other leg, the deterministic
    /// pressure lever the owner smoke uses, stays in the environment and
    /// overrides this one for that run only: two different things sharing one
    /// spelling, and this is the one a person configures.
    pub(crate) detector_queue_capacity: usize,
    /// How long vigil waits before retrying a dropped camera stream, and the
    /// widest that wait gets as it backs off. Both resolved from the store:
    /// how patiently a node re-approaches a camera is a thing an operator sets.
    pub(crate) rtsp_retry_initial_ms: u64,
    pub(crate) rtsp_retry_max_ms: u64,
    /// Acceleration intent: probe and use hardware decode only when a real
    /// startup probe succeeds; missing means true.
    pub(crate) hardware_decoding: bool,
    /// Acceleration intent: probe and use an accelerated detector backend
    /// only when the artifact ships one and its probe succeeds; missing
    /// means true.
    pub(crate) accelerated_detection: bool,
    /// Fabric enrollment ticket (criterion C6/C10). Inert scaffold: present
    /// on the config surface with a sane default (absent) so every shape
    /// shows the knob; not yet wired to any fabric client behavior.
    pub(crate) fabric_ticket: Option<String>,
    /// Whether this node embeds the fabric hub (criterion C10). Sane
    /// default false — no silent new network surface on existing installs.
    /// Inert scaffold: not yet wired to any hub behavior.
    pub(crate) fabric_hub: bool,
    /// Per-source opt-out (default ON) for moving an ephemeral compressed
    /// clip of a motion event to a same-tenant fabric node under queue
    /// pressure (criterion C10 / the addon privacy wording). The owner's
    /// binding promise — offload becomes automatic on join — requires
    /// default movement; this is the documented knob to turn it back off
    /// for one node while keeping fabric enrollment (and claiming OTHER
    /// nodes' work) otherwise unaffected. Only read by the fabric offload
    /// path (`runtime.rs`, behind the `fabric` feature) — a no-feature
    /// build never reads it, hence the blanket allow rather than a
    /// cfg-gated one (the field itself is unconditional, so config
    /// resolution/precedence/CLI/env/addon-surface behavior is identical
    /// across builds).
    #[allow(dead_code)]
    pub(crate) fabric_allow_frame_offload: bool,
    /// Worker lease duration in milliseconds (criterion C10, fix cycle 9).
    /// Inert scaffold: present on the config surface with a sane default
    /// (300000ms = 5 minutes, today's hardcoded `fabric.rs`
    /// `lease_duration_ms: 5 * 60_000` literal) so every shape shows the
    /// knob; not yet wired to `WorkerConfig.lease_duration_ms` construction.
    /// Precedent: `fabric_ticket`/`fabric_hub` above (705b1ac).
    #[allow(dead_code)]
    pub(crate) fabric_worker_lease_ms: u64,
    /// Offload fallback horizon in milliseconds (criterion C10, fix cycle
    /// 9). Inert scaffold: sane default (5000ms, today's hardcoded
    /// `OffloadPolicyConfig::default()` value) so every shape shows the
    /// knob; not yet wired to `OffloadPolicyConfig.fallback_horizon_ms`
    /// construction. Precedent: `fabric_ticket`/`fabric_hub` above
    /// (705b1ac).
    #[allow(dead_code)]
    pub(crate) fabric_fallback_horizon_ms: u64,
    /// Resolved through the typed settings registry (see
    /// [`declare_settings`]): the multiplier applied to a stream's
    /// effective output frame rate to derive its automatic keyframe
    /// interval, in output frames. Owner-ratified automatic default `2`.
    /// Inert scaffold today — no producer wires a real encoder through
    /// `crate::encode` yet — but resolved onto this surface exactly like
    /// `detector_stationary_interval_secs` so the config/add-on/docs
    /// surfaces and an operator pin are already real.
    pub(crate) keyframe_interval_fps_multiplier: u32,
    /// The automatic keyframe interval's lower clamp, in output frames.
    /// Owner-ratified automatic default `15`.
    pub(crate) keyframe_interval_min_frames: u32,
    /// The automatic keyframe interval's upper clamp, in output frames.
    /// Owner-ratified automatic default `300`.
    pub(crate) keyframe_interval_max_frames: u32,
    /// The automatic encode bitrate, in bits per second, for a stream
    /// whose resolution is at most 640x480. Owner-ratified automatic
    /// default `1_000_000`.
    pub(crate) bitrate_bps_up_to_640x480: u32,
    /// The automatic encode bitrate, in bits per second, for a stream
    /// whose resolution is above 640x480 and at most 1280x720.
    /// Owner-ratified automatic default `2_000_000`.
    pub(crate) bitrate_bps_up_to_1280x720: u32,
    /// The automatic encode bitrate, in bits per second, for a stream
    /// whose resolution is above 1280x720 and at most 1920x1080.
    /// Owner-ratified automatic default `4_000_000`.
    pub(crate) bitrate_bps_up_to_1920x1080: u32,
    /// The automatic encode bitrate, in bits per second, for a stream
    /// whose resolution is above 1920x1080 and at most 2560x1440.
    /// Owner-ratified automatic default `6_000_000`.
    pub(crate) bitrate_bps_up_to_2560x1440: u32,
    /// The automatic encode bitrate, in bits per second, for a stream
    /// whose resolution is above 2560x1440. Owner-ratified automatic
    /// default `10_000_000`.
    pub(crate) bitrate_bps_above_2560x1440: u32,
}

impl fmt::Debug for RuntimeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeConfig")
            .field("data_dir", &self.data_dir)
            .field("store_path", &self.store_path)
            .field("health_port", &self.health_port)
            .field("review_port", &self.review_port)
            .field("site_name", &self.site_name)
            .field("camera_name", &self.camera_name)
            .field("rtsp_url", &debug_redacted_url(&self.rtsp_url))
            .field(
                "rtsp_username",
                &debug_visible_username(&self.rtsp_username),
            )
            .field("rtsp_password", &self.rtsp_password)
            .field("detector_model_id", &self.detector_model_id)
            .field("detector_model_path", &self.detector_model_path)
            .field(
                "detector_confidence_threshold",
                &self.detector_confidence_threshold,
            )
            .field("detector_sample_frames", &self.detector_sample_frames)
            .field(
                "detector_stationary_interval_secs",
                &self.detector_stationary_interval_secs,
            )
            .field("cameras", &self.cameras)
            .field("mqtt", &self.mqtt)
            .field("service_id", &self.service_id)
            .field("recognition", &self.recognition)
            .field("detector_class_indices", &self.detector_class_indices)
            .field("detector_queue_capacity", &self.detector_queue_capacity)
            .field("rtsp_retry_initial_ms", &self.rtsp_retry_initial_ms)
            .field("rtsp_retry_max_ms", &self.rtsp_retry_max_ms)
            .field("hardware_decoding", &self.hardware_decoding)
            .field("accelerated_detection", &self.accelerated_detection)
            .field("fabric_ticket", &debug_redacted_ticket(&self.fabric_ticket))
            .field("fabric_hub", &self.fabric_hub)
            .field(
                "fabric_allow_frame_offload",
                &self.fabric_allow_frame_offload,
            )
            .field("fabric_worker_lease_ms", &self.fabric_worker_lease_ms)
            .field(
                "fabric_fallback_horizon_ms",
                &self.fabric_fallback_horizon_ms,
            )
            .field(
                "keyframe_interval_fps_multiplier",
                &self.keyframe_interval_fps_multiplier,
            )
            .field(
                "keyframe_interval_min_frames",
                &self.keyframe_interval_min_frames,
            )
            .field(
                "keyframe_interval_max_frames",
                &self.keyframe_interval_max_frames,
            )
            .field("bitrate_bps_up_to_640x480", &self.bitrate_bps_up_to_640x480)
            .field(
                "bitrate_bps_up_to_1280x720",
                &self.bitrate_bps_up_to_1280x720,
            )
            .field(
                "bitrate_bps_up_to_1920x1080",
                &self.bitrate_bps_up_to_1920x1080,
            )
            .field(
                "bitrate_bps_up_to_2560x1440",
                &self.bitrate_bps_up_to_2560x1440,
            )
            .field(
                "bitrate_bps_above_2560x1440",
                &self.bitrate_bps_above_2560x1440,
            )
            .finish()
    }
}

/// Per-camera entry as it appears in TOML/JSON config files. Carries every
/// source-kind field as a plain `Option`, kind-agnostic, so "never
/// supplied", "supplied empty", and "names a source this artifact cannot
/// reach" ([`resolve_camera_source_kind`], [`SourceKindCapabilityRegistry`])
/// stay distinct outcomes rather than being resolved during parsing.
///
/// `Debug` is hand-written for the same URL-userinfo-leak reason as
/// [`CameraEntry`] — this predates that type's own new fields, but the
/// leak is in the same class and in scope now that this struct itself
/// carries `usb_device`/`csi_module`/`mjpeg_url`.
#[derive(Clone, Default, Deserialize)]
pub(crate) struct CameraEntryPartial {
    name: String,
    rtsp_url: Option<String>,
    live_rtsp_url: Option<String>,
    username: Option<String>,
    password: Option<Secret>,
    usb_device: Option<String>,
    csi_module: Option<String>,
    mjpeg_url: Option<String>,
    /// What THIS camera's motion gate runs at, which is the whole reason the
    /// setting is declared twice: turning the driveway down for a tree in the
    /// wind must not turn the hallway down with it. Read here so a value typed
    /// into the camera's own row on the add-on options page authors a record at
    /// that camera's scope, exactly as the deployment-wide field authors one at
    /// the node's.
    motion_sensitivity: Option<i64>,
}

impl fmt::Debug for CameraEntryPartial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CameraEntryPartial")
            .field("name", &self.name)
            .field("rtsp_url", &debug_redacted_url(&self.rtsp_url))
            .field("live_rtsp_url", &debug_redacted_url(&self.live_rtsp_url))
            .field("username", &debug_visible_username(&self.username))
            .field("password", &self.password)
            .field("usb_device", &self.usb_device)
            .field("csi_module", &self.csi_module)
            .field("mjpeg_url", &debug_redacted_url(&self.mjpeg_url))
            .field("motion_sensitivity", &self.motion_sensitivity)
            .finish()
    }
}

/// `Debug` is hand-written for the same reason as [`CameraEntry`]/
/// [`CameraEntryPartial`]: `rtsp_url`/`live_rtsp_url` are exactly as
/// URL-shaped and exactly as capable of carrying an embedded credential.
#[derive(Default, Deserialize)]
struct PartialConfig {
    data_dir: Option<PathBuf>,
    store_path: Option<PathBuf>,
    health_port: Option<u16>,
    review_port: Option<u16>,
    site_name: Option<String>,
    camera_name: Option<String>,
    rtsp_url: Option<String>,
    live_rtsp_url: Option<String>,
    rtsp_username: Option<String>,
    rtsp_password: Option<Secret>,
    /// Legacy single-camera USB source, the [`CameraEntryPartial::usb_device`]
    /// counterpart of `rtsp_url` above — same unvalidated legacy-synthesis
    /// treatment `rtsp_url`/`live_rtsp_url` already get (see `load`'s
    /// single-camera fallback branch), never routed through
    /// `resolve_camera_source_kind`.
    usb_device: Option<String>,
    /// Legacy single-camera CSI source; see `usb_device` above.
    csi_module: Option<String>,
    /// Legacy single-camera MJPEG source; see `usb_device` above.
    mjpeg_url: Option<String>,
    detector_model_id: Option<String>,
    detector_model_path: Option<PathBuf>,
    detector_confidence_threshold: Option<f64>,
    detector_sample_frames: Option<usize>,
    detector_stationary_interval_secs: Option<u64>,
    /// Multi-camera list.  When present, supersedes camera_name/rtsp_url.
    cameras: Option<Vec<CameraEntryPartial>>,
    // MQTT broker — provided by HA Supervisor or env vars when broker is configured.
    mqtt_host: Option<String>,
    mqtt_port: Option<u16>,
    mqtt_username: Option<String>,
    mqtt_password: Option<Secret>,
    service_id: Option<String>,
    recognition_weights_dir: Option<PathBuf>,
    recognition_space_id: Option<String>,
    recognition_threshold: Option<f64>,
    recognition_covered_classes: Option<Vec<String>>,
    /// The classes Vigil looks for. Its own field rather than a second reading
    /// of the recognition list: what the machine detects and what recognition
    /// puts a name to are two choices a person makes separately, and one field
    /// serving both means widening either silently widens the other.
    detector_classes: Option<Vec<String>>,
    /// How long a stream session gathers real video before it chooses a decode
    /// path — a knob a Home Assistant user sets as an add-on option. It authors
    /// into the store like any other behavior value; the decode probe reads
    /// what the runtime resolved.
    decode_probe_deadline_secs: Option<u64>,
    /// The other startup-probe deadline, alongside the one above it: how long
    /// the hardware probe is given before decode falls back visibly.
    hardware_probe_deadline_secs: Option<u64>,
    /// The first and widest waits between attempts to reach a camera whose
    /// stream has dropped. An operator with a slow camera lengthens them.
    rtsp_retry_initial_ms: Option<u64>,
    rtsp_retry_max_ms: Option<u64>,
    hardware_decoding: Option<bool>,
    accelerated_detection: Option<bool>,
    /// The deployment-wide motion sensitivity every camera without one of its
    /// own runs at — the node-scope half of [`CameraEntryPartial`]'s field of
    /// the same name.
    motion_sensitivity: Option<i64>,
    /// Whether Vigil, having mirrored a value onto the add-on options page,
    /// also takes the restart that makes the container's own copy of the file
    /// agree. An ordinary setting, read off a surface like any other.
    restart_on_reflect: Option<bool>,
    /// The third of the rate controls, beside `detector_sample_frames` and
    /// `detector_confidence_threshold` above: how deep the detector queue holds
    /// work.
    detector_queue_capacity: Option<usize>,
    /// The two values automatic management chooses, so an operator who turns
    /// that automation off has something left to set. Loose strings, never an
    /// enumeration of backend names: which backends exist is a property of the
    /// build, and the store refuses one this artifact does not carry rather
    /// than this reader guessing at the roster.
    detection_backend: Option<String>,
    decode_backend: Option<String>,
    fabric_ticket: Option<String>,
    fabric_hub: Option<bool>,
    fabric_allow_frame_offload: Option<bool>,
    fabric_worker_lease_ms: Option<u64>,
    fabric_fallback_horizon_ms: Option<u64>,
    keyframe_interval_fps_multiplier: Option<u32>,
    keyframe_interval_min_frames: Option<u32>,
    keyframe_interval_max_frames: Option<u32>,
    bitrate_bps_up_to_640x480: Option<u32>,
    bitrate_bps_up_to_1280x720: Option<u32>,
    bitrate_bps_up_to_1920x1080: Option<u32>,
    bitrate_bps_up_to_2560x1440: Option<u32>,
    bitrate_bps_above_2560x1440: Option<u32>,
}

impl fmt::Debug for PartialConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PartialConfig")
            .field("data_dir", &self.data_dir)
            .field("store_path", &self.store_path)
            .field("health_port", &self.health_port)
            .field("review_port", &self.review_port)
            .field("site_name", &self.site_name)
            .field("camera_name", &self.camera_name)
            .field("rtsp_url", &debug_redacted_url(&self.rtsp_url))
            .field("live_rtsp_url", &debug_redacted_url(&self.live_rtsp_url))
            .field(
                "rtsp_username",
                &debug_visible_username(&self.rtsp_username),
            )
            .field("rtsp_password", &self.rtsp_password)
            .field("usb_device", &self.usb_device)
            .field("csi_module", &self.csi_module)
            .field("mjpeg_url", &debug_redacted_url(&self.mjpeg_url))
            .field("detector_model_id", &self.detector_model_id)
            .field("detector_model_path", &self.detector_model_path)
            .field(
                "detector_confidence_threshold",
                &self.detector_confidence_threshold,
            )
            .field("detector_sample_frames", &self.detector_sample_frames)
            .field(
                "detector_stationary_interval_secs",
                &self.detector_stationary_interval_secs,
            )
            .field("cameras", &self.cameras)
            .field("mqtt_host", &self.mqtt_host)
            .field("mqtt_port", &self.mqtt_port)
            .field(
                "mqtt_username",
                &debug_visible_username(&self.mqtt_username),
            )
            .field("mqtt_password", &self.mqtt_password)
            .field("service_id", &self.service_id)
            .field("recognition_weights_dir", &self.recognition_weights_dir)
            .field("recognition_space_id", &self.recognition_space_id)
            .field("recognition_threshold", &self.recognition_threshold)
            .field(
                "recognition_covered_classes",
                &self.recognition_covered_classes,
            )
            .field("detector_classes", &self.detector_classes)
            .field(
                "hardware_probe_deadline_secs",
                &self.hardware_probe_deadline_secs,
            )
            .field("rtsp_retry_initial_ms", &self.rtsp_retry_initial_ms)
            .field("rtsp_retry_max_ms", &self.rtsp_retry_max_ms)
            .field("hardware_decoding", &self.hardware_decoding)
            .field("accelerated_detection", &self.accelerated_detection)
            .field("motion_sensitivity", &self.motion_sensitivity)
            .field("restart_on_reflect", &self.restart_on_reflect)
            .field("detector_queue_capacity", &self.detector_queue_capacity)
            .field("detection_backend", &self.detection_backend)
            .field("decode_backend", &self.decode_backend)
            .field("fabric_ticket", &debug_redacted_ticket(&self.fabric_ticket))
            .field("fabric_hub", &self.fabric_hub)
            .field(
                "fabric_allow_frame_offload",
                &self.fabric_allow_frame_offload,
            )
            .field("fabric_worker_lease_ms", &self.fabric_worker_lease_ms)
            .field(
                "fabric_fallback_horizon_ms",
                &self.fabric_fallback_horizon_ms,
            )
            .field(
                "keyframe_interval_fps_multiplier",
                &self.keyframe_interval_fps_multiplier,
            )
            .field(
                "keyframe_interval_min_frames",
                &self.keyframe_interval_min_frames,
            )
            .field(
                "keyframe_interval_max_frames",
                &self.keyframe_interval_max_frames,
            )
            .field("bitrate_bps_up_to_640x480", &self.bitrate_bps_up_to_640x480)
            .field(
                "bitrate_bps_up_to_1280x720",
                &self.bitrate_bps_up_to_1280x720,
            )
            .field(
                "bitrate_bps_up_to_1920x1080",
                &self.bitrate_bps_up_to_1920x1080,
            )
            .field(
                "bitrate_bps_up_to_2560x1440",
                &self.bitrate_bps_up_to_2560x1440,
            )
            .field(
                "bitrate_bps_above_2560x1440",
                &self.bitrate_bps_above_2560x1440,
            )
            .finish()
    }
}

/// `Debug` is hand-written for the same reason as [`PartialConfig`]:
/// `--rtsp-url`/`--live-rtsp-url` land here before merging into
/// `PartialConfig`, carrying the identical userinfo-leak risk.
#[derive(Default)]
struct CliOverrides {
    config_path: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    store_path: Option<PathBuf>,
    health_port: Option<u16>,
    review_port: Option<u16>,
    site_name: Option<String>,
    camera_name: Option<String>,
    rtsp_url: Option<String>,
    live_rtsp_url: Option<String>,
    rtsp_username: Option<String>,
    rtsp_password: Option<Secret>,
    /// Legacy single-camera USB/CSI/MJPEG overrides — see `PartialConfig`'s
    /// own `usb_device` field doc comment.
    usb_device: Option<String>,
    csi_module: Option<String>,
    mjpeg_url: Option<String>,
    detector_model_id: Option<String>,
    detector_model_path: Option<PathBuf>,
    detector_confidence_threshold: Option<f64>,
    detector_sample_frames: Option<usize>,
    detector_stationary_interval_secs: Option<u64>,
    recognition_weights_dir: Option<PathBuf>,
    hardware_decoding: Option<bool>,
    accelerated_detection: Option<bool>,
    fabric_ticket: Option<String>,
    fabric_hub: Option<bool>,
    fabric_allow_frame_offload: Option<bool>,
    fabric_worker_lease_ms: Option<u64>,
    fabric_fallback_horizon_ms: Option<u64>,
}

impl fmt::Debug for CliOverrides {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CliOverrides")
            .field("config_path", &self.config_path)
            .field("data_dir", &self.data_dir)
            .field("store_path", &self.store_path)
            .field("health_port", &self.health_port)
            .field("review_port", &self.review_port)
            .field("site_name", &self.site_name)
            .field("camera_name", &self.camera_name)
            .field("rtsp_url", &debug_redacted_url(&self.rtsp_url))
            .field("live_rtsp_url", &debug_redacted_url(&self.live_rtsp_url))
            .field(
                "rtsp_username",
                &debug_visible_username(&self.rtsp_username),
            )
            .field("rtsp_password", &self.rtsp_password)
            .field("usb_device", &self.usb_device)
            .field("csi_module", &self.csi_module)
            .field("mjpeg_url", &debug_redacted_url(&self.mjpeg_url))
            .field("detector_model_id", &self.detector_model_id)
            .field("detector_model_path", &self.detector_model_path)
            .field(
                "detector_confidence_threshold",
                &self.detector_confidence_threshold,
            )
            .field("detector_sample_frames", &self.detector_sample_frames)
            .field(
                "detector_stationary_interval_secs",
                &self.detector_stationary_interval_secs,
            )
            .field("recognition_weights_dir", &self.recognition_weights_dir)
            .field("hardware_decoding", &self.hardware_decoding)
            .field("accelerated_detection", &self.accelerated_detection)
            .field("fabric_ticket", &debug_redacted_ticket(&self.fabric_ticket))
            .field("fabric_hub", &self.fabric_hub)
            .field(
                "fabric_allow_frame_offload",
                &self.fabric_allow_frame_offload,
            )
            .field("fabric_worker_lease_ms", &self.fabric_worker_lease_ms)
            .field(
                "fabric_fallback_horizon_ms",
                &self.fabric_fallback_horizon_ms,
            )
            .finish()
    }
}

/// `<data_root>/fabric.toml` — the fabric enrollment file surface (criterion
/// C6/C10), mirroring cg's `fabric.toml` file convention (a plain TOML file
/// under the data root, read on every start). vigil's own knob vocabulary
/// (`fabric_ticket`/`fabric_hub`, the SAME names the HAOS add-on options
/// and env vars use) is kept rather than cg's `hub_endpoint`/`tenant` key
/// names: vigil's tenant is a fixed, non-operator-facing value by design
/// (`fabric.rs`'s `FABRIC_TENANT`), and a second key vocabulary for the
/// same two knobs would violate the one-vocabulary obligation (C7/C10)
/// this same run is chartered to uphold.
/// `Debug` is hand-written for the same reason as `RuntimeConfig`/
/// `PartialConfig`/`CliOverrides`: `fabric_ticket` is an enrollment
/// credential.
#[derive(Default, Deserialize)]
struct FabricFileConfig {
    fabric_ticket: Option<String>,
    fabric_hub: Option<bool>,
}

impl fmt::Debug for FabricFileConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FabricFileConfig")
            .field("fabric_ticket", &debug_redacted_ticket(&self.fabric_ticket))
            .field("fabric_hub", &self.fabric_hub)
            .finish()
    }
}

fn read_fabric_toml(data_dir: &Path) -> FabricFileConfig {
    let path = data_dir.join("fabric.toml");
    let Ok(text) = fs::read_to_string(&path) else {
        return FabricFileConfig::default();
    };
    toml::from_str(&text).unwrap_or_else(|error| {
        // Same defect class as `read_toml_config`/`read_options_json`:
        // `error` here is `toml::de::Error`, whose own `Display` renders
        // an annotated snippet of the offending source line by default —
        // `fabric_ticket` is an enrollment credential, so printing
        // `{error}` straight into this log line could echo it verbatim.
        // Routed through the same location-only describer.
        println!(
            "fabric_toml_parse_failed=true path={} error={}",
            path.display(),
            describe_toml_parse_error(&text, &error)
        );
        FabricFileConfig::default()
    })
}

/// One input surface's own assertions: which surface it is, and the settings
/// it names right now with the values it gives them. Named for the surface
/// rather than for files because the startup options are one of these too — a
/// flag a person passed is a thing that surface says, and it keeps its own
/// record like any other.
#[derive(Debug, Clone)]
pub(crate) struct SurfaceAssertions {
    pub(crate) surface: crate::settings_model::Surface,
    pub(crate) entries: Vec<(String, crate::settings_model::SettingValue)>,
    /// What the surface says about each camera it lists, camera by camera, kept
    /// apart from the deployment-wide entries because they are records at a
    /// different scope: a value in the driveway's own row governs the driveway
    /// and nothing else. Every listed camera appears here even when it names no
    /// setting of its own — that is what makes deleting a camera's value clear
    /// that camera's record, the same way deleting a deployment-wide key clears
    /// the deployment's.
    pub(crate) camera_entries: Vec<(String, Vec<(String, crate::settings_model::SettingValue)>)>,
}

/// The settings this arc carries that an input surface can assert, read off
/// the partial that surface alone produced. A field the surface did not name is
/// absent rather than defaulted, which is what makes "this surface no longer
/// says anything about this setting" expressible at all — and, for a file
/// surface, what makes clearing by absence possible. Startup options never
/// clear by absence; the store enforces that per surface, so one reader serves
/// both.
fn surface_assertions(
    surface: crate::settings_model::Surface,
    partial: &PartialConfig,
) -> SurfaceAssertions {
    use crate::settings_model::{
        DETECTOR_CONFIDENCE_THRESHOLD_SETTING, DETECTOR_SAMPLE_FRAMES_SETTING,
        DETECTOR_STATIONARY_INTERVAL_SETTING, SettingValue,
    };
    let mut entries: Vec<(String, SettingValue)> = Vec::new();
    if let Some(frames) = partial.detector_sample_frames {
        entries.push((
            DETECTOR_SAMPLE_FRAMES_SETTING.to_string(),
            SettingValue::Int(i64::try_from(frames).unwrap_or(i64::MAX)),
        ));
    }
    if let Some(interval) = partial.detector_stationary_interval_secs {
        entries.push((
            DETECTOR_STATIONARY_INTERVAL_SETTING.to_string(),
            SettingValue::Int(i64::try_from(interval).unwrap_or(i64::MAX)),
        ));
    }
    if let Some(threshold) = partial.detector_confidence_threshold {
        entries.push((
            DETECTOR_CONFIDENCE_THRESHOLD_SETTING.to_string(),
            SettingValue::Float(threshold),
        ));
    }
    // The values that used to arrive through the environment. A surface that
    // names one of them authors a record for it like any other, so the operator
    // surface can answer WHO chose this node's ports, its model, or its site
    // name — which a merged value could never say.
    if let Some(name) = partial.site_name.as_ref() {
        entries.push((
            crate::settings_model::SITE_NAME_SETTING.to_string(),
            SettingValue::text(name.clone()),
        ));
    }
    if let Some(name) = partial.camera_name.as_ref() {
        entries.push((
            crate::settings_model::CAMERA_NAME_SETTING.to_string(),
            SettingValue::text(name.clone()),
        ));
    }
    if let Some(port) = partial.health_port {
        entries.push((
            crate::settings_model::HEALTH_PORT_SETTING.to_string(),
            SettingValue::Int(i64::from(port)),
        ));
    }
    if let Some(port) = partial.review_port {
        entries.push((
            crate::settings_model::REVIEW_PORT_SETTING.to_string(),
            SettingValue::Int(i64::from(port)),
        ));
    }
    if let Some(model) = partial.detector_model_id.as_ref() {
        entries.push((
            crate::settings_model::DETECTOR_MODEL_ID_SETTING.to_string(),
            SettingValue::text(model.clone()),
        ));
    }
    if let Some(path) = partial.detector_model_path.as_ref() {
        entries.push((
            crate::settings_model::DETECTOR_MODEL_PATH_SETTING.to_string(),
            SettingValue::text(path.display().to_string()),
        ));
    }
    if let Some(path) = partial.recognition_weights_dir.as_ref() {
        entries.push((
            crate::settings_model::RECOGNITION_WEIGHTS_DIR_SETTING.to_string(),
            SettingValue::text(path.display().to_string()),
        ));
    }
    if let Some(space) = partial.recognition_space_id.as_ref() {
        entries.push((
            crate::settings_model::RECOGNITION_SPACE_ID_SETTING.to_string(),
            SettingValue::text(space.clone()),
        ));
    }
    if let Some(threshold) = partial.recognition_threshold {
        entries.push((
            crate::settings_model::RECOGNITION_THRESHOLD_SETTING.to_string(),
            SettingValue::Float(threshold),
        ));
    }
    if let Some(on) = partial.fabric_hub {
        entries.push((
            crate::settings_model::FABRIC_HUB_SETTING.to_string(),
            SettingValue::Bool(on),
        ));
    }
    if let Some(on) = partial.fabric_allow_frame_offload {
        entries.push((
            crate::settings_model::FABRIC_ALLOW_FRAME_OFFLOAD_SETTING.to_string(),
            SettingValue::Bool(on),
        ));
    }
    if let Some(lease) = partial.fabric_worker_lease_ms {
        entries.push((
            crate::settings_model::FABRIC_WORKER_LEASE_MS_SETTING.to_string(),
            SettingValue::Int(i64::try_from(lease).unwrap_or(i64::MAX)),
        ));
    }
    if let Some(horizon) = partial.fabric_fallback_horizon_ms {
        entries.push((
            crate::settings_model::FABRIC_FALLBACK_HORIZON_MS_SETTING.to_string(),
            SettingValue::Int(i64::try_from(horizon).unwrap_or(i64::MAX)),
        ));
    }
    if let Some(secs) = partial.decode_probe_deadline_secs {
        entries.push((
            crate::settings_model::DECODE_PROBE_DEADLINE_SECS_SETTING.to_string(),
            SettingValue::Int(i64::try_from(secs).unwrap_or(i64::MAX)),
        ));
    }
    if let Some(secs) = partial.hardware_probe_deadline_secs {
        entries.push((
            crate::settings_model::HARDWARE_PROBE_DEADLINE_SECS_SETTING.to_string(),
            SettingValue::Int(i64::try_from(secs).unwrap_or(i64::MAX)),
        ));
    }
    if let Some(wait) = partial.rtsp_retry_initial_ms {
        entries.push((
            crate::settings_model::RTSP_RETRY_INITIAL_MS_SETTING.to_string(),
            SettingValue::Int(i64::try_from(wait).unwrap_or(i64::MAX)),
        ));
    }
    if let Some(wait) = partial.rtsp_retry_max_ms {
        entries.push((
            crate::settings_model::RTSP_RETRY_MAX_MS_SETTING.to_string(),
            SettingValue::Int(i64::try_from(wait).unwrap_or(i64::MAX)),
        ));
    }
    // The camera stream stays OUT of this list deliberately, unlike everything
    // above it. A stream URL carries the camera's credentials in its userinfo,
    // and a settings record is read back on an operator surface — so the one
    // value whose spelling is a secret is not authored as an ordinary setting.

    // The two class lists, each authoring its own setting and nothing else.
    // Naming the classes to look for is what an operator does when they want
    // vehicles detected; naming the classes recognition covers is what they do
    // when they want a face put to a person. Routing both through one field
    // means a person who widens either one silently widens the other, and
    // leaves someone who wants detection without recognition no field to say it
    // in. The detector resolves its allowlist from its own record in the store,
    // never from the recognition configuration.
    if let Some(classes) = partial.detector_classes.as_ref() {
        entries.push((
            crate::settings_model::DETECTOR_CLASSES_SETTING.to_string(),
            SettingValue::list(classes.iter().map(String::as_str)),
        ));
    }
    if let Some(classes) = partial.recognition_covered_classes.as_ref() {
        entries.push((
            crate::settings_model::RECOGNITION_COVERED_CLASSES_SETTING.to_string(),
            SettingValue::list(classes.iter().map(String::as_str)),
        ));
    }
    if let Some(on) = partial.hardware_decoding {
        entries.push((
            crate::settings_domains::HARDWARE_DECODING_DOMAIN.to_string(),
            SettingValue::Bool(on),
        ));
    }
    if let Some(on) = partial.accelerated_detection {
        entries.push((
            crate::settings_domains::ACCELERATED_DETECTION_DOMAIN.to_string(),
            SettingValue::Bool(on),
        ));
    }
    // The rate controls and the two governed backends. A surface that declares
    // a key owes the person who edits it a record or a refusal they can read;
    // a key the reader cannot see is an edit that goes nowhere and says nothing,
    // which is the one outcome this whole model exists to refuse. The backends
    // are carried through as the text the surface holds — the store is what
    // knows which backends this build carries, and refuses the rest by name.
    if let Some(sensitivity) = partial.motion_sensitivity {
        entries.push((
            crate::settings_model::MOTION_SENSITIVITY_SETTING.to_string(),
            SettingValue::Int(sensitivity),
        ));
    }
    if let Some(on) = partial.restart_on_reflect {
        entries.push((
            crate::settings_reflection::RESTART_ON_REFLECT_SETTING.to_string(),
            SettingValue::Bool(on),
        ));
    }
    if let Some(capacity) = partial.detector_queue_capacity {
        entries.push((
            crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING.to_string(),
            SettingValue::Int(i64::try_from(capacity).unwrap_or(i64::MAX)),
        ));
    }
    if let Some(backend) = partial.detection_backend.as_ref() {
        entries.push((
            crate::settings_backends::DETECTION_BACKEND_SETTING.to_string(),
            SettingValue::text(backend.clone()),
        ));
    }
    if let Some(backend) = partial.decode_backend.as_ref() {
        entries.push((
            crate::settings_backends::DECODE_BACKEND_SETTING.to_string(),
            SettingValue::text(backend.clone()),
        ));
    }
    SurfaceAssertions {
        surface,
        entries,
        camera_entries: camera_surface_entries(partial),
    }
}

/// What each camera this surface lists says about itself. A camera contributes
/// an entry whether or not it names a setting, because a camera that has gone
/// quiet is exactly what clears the record it used to author — the same
/// clearing the deployment-wide half gets, one scope down.
fn camera_surface_entries(
    partial: &PartialConfig,
) -> Vec<(String, Vec<(String, crate::settings_model::SettingValue)>)> {
    use crate::settings_model::SettingValue;

    let Some(cameras) = partial.cameras.as_ref() else {
        return Vec::new();
    };
    cameras
        .iter()
        // A camera with no name cannot be addressed, so it is not a scope
        // anything can be said about.
        .filter(|camera| !camera.name.trim().is_empty())
        .map(|camera| {
            let mut entries: Vec<(String, SettingValue)> = Vec::new();
            if let Some(sensitivity) = camera.motion_sensitivity {
                entries.push((
                    crate::settings_model::MOTION_SENSITIVITY_SETTING.to_string(),
                    SettingValue::Int(sensitivity),
                ));
            }
            (camera.name.clone(), entries)
        })
        .collect()
}

pub(crate) fn load(args: Vec<OsString>) -> Result<RuntimeConfig, String> {
    let cli = parse_cli(args)?;
    let mut partial = PartialConfig::default();

    let mut file_surface = None;
    if let Some(path) = cli.config_path.as_ref() {
        let from_file = read_toml_config(path)?;
        file_surface = Some(surface_assertions(
            crate::settings_model::Surface::ConfigFile,
            &from_file,
        ));
        merge(&mut partial, from_file);
    } else {
        let options_path = default_options_json_path();
        if options_path.exists() {
            let from_options = read_options_json(&options_path)?;
            file_surface = Some(surface_assertions(
                crate::settings_model::Surface::AddonOptions,
                &from_options,
            ));
            merge(&mut partial, from_options);
        }
    }

    let from_cli = PartialConfig {
        data_dir: cli.data_dir,
        store_path: cli.store_path,
        health_port: cli.health_port,
        review_port: cli.review_port,
        site_name: cli.site_name,
        camera_name: cli.camera_name,
        rtsp_url: cli.rtsp_url,
        live_rtsp_url: cli.live_rtsp_url,
        rtsp_username: cli.rtsp_username,
        rtsp_password: cli.rtsp_password,
        usb_device: cli.usb_device,
        csi_module: cli.csi_module,
        mjpeg_url: cli.mjpeg_url,
        detector_model_id: cli.detector_model_id,
        detector_model_path: cli.detector_model_path,
        detector_confidence_threshold: cli.detector_confidence_threshold,
        detector_sample_frames: cli.detector_sample_frames,
        detector_stationary_interval_secs: cli.detector_stationary_interval_secs,
        // Multi-camera list not exposed as CLI flags; comes from config file or options.json.
        cameras: None,
        // MQTT fields are not exposed as CLI flags; they come from env vars or options.json.
        mqtt_host: None,
        mqtt_port: None,
        mqtt_username: None,
        mqtt_password: None,
        service_id: None,
        recognition_weights_dir: cli.recognition_weights_dir,
        recognition_space_id: None,
        recognition_threshold: None,
        recognition_covered_classes: None,
        // Neither class list has a command-line flag: like `cameras`, they come
        // from the configuration file or the add-on options.
        detector_classes: None,
        decode_probe_deadline_secs: None,
        // The two probe deadlines and the two stream-retry waits have no
        // command-line flag: the configuration file or the add-on options is
        // where they are set, like the four video-encoding levers below.
        hardware_probe_deadline_secs: None,
        rtsp_retry_initial_ms: None,
        rtsp_retry_max_ms: None,
        hardware_decoding: cli.hardware_decoding,
        accelerated_detection: cli.accelerated_detection,
        fabric_worker_lease_ms: cli.fabric_worker_lease_ms,
        fabric_fallback_horizon_ms: cli.fabric_fallback_horizon_ms,
        // Fabric enrollment knobs are deliberately left OUT of this
        // merge (unlike every other CLI flag above): their precedence
        // is CLI > env > options.json/fabric.toml, the REVERSE of this
        // merge's CLI-before-env order — applied explicitly, below,
        // once `data_dir` (needed for fabric.toml) is resolved.
        fabric_ticket: None,
        fabric_hub: None,
        // The per-source offload opt-out follows the SAME precedence as
        // hardware_decoding/accelerated_detection above (CLI here, env
        // last) — it is not a fabric.toml-eligible knob.
        fabric_allow_frame_offload: cli.fabric_allow_frame_offload,
        // The rate controls and the two governed backends have no CLI flag
        // either: they are set on the add-on options page or in the config
        // file, and changed live through `vigil settings`.
        motion_sensitivity: None,
        restart_on_reflect: None,
        detector_queue_capacity: None,
        detection_backend: None,
        decode_backend: None,
        // The four video-encoding levers have no CLI flag (like
        // `cameras`/`recognition_covered_classes` above): config file
        // or add-on options only.
        keyframe_interval_fps_multiplier: None,
        keyframe_interval_min_frames: None,
        keyframe_interval_max_frames: None,
        bitrate_bps_up_to_640x480: None,
        bitrate_bps_up_to_1280x720: None,
        bitrate_bps_up_to_1920x1080: None,
        bitrate_bps_up_to_2560x1440: None,
        bitrate_bps_above_2560x1440: None,
    };
    // The startup options author before they are merged, so the record carries
    // what the command line ITSELF said rather than the merged result of every
    // surface — a merged value cannot say which surface it came from, and that
    // attribution is the whole point of keeping per-surface records.
    let startup_surface = Some(surface_assertions(
        crate::settings_model::Surface::StartupOptions,
        &from_cli,
    ));
    merge(&mut partial, from_cli);
    merge(&mut partial, env_overrides()?);

    // If MQTT_HOST was not supplied via options.json or env, attempt Supervisor services API.
    // Only called when SUPERVISOR_TOKEN is present (i.e. running as an HA add-on).
    if partial.mqtt_host.is_none()
        && let Some(cfg) = crate::supervisor::fetch_supervisor_mqtt()
    {
        partial.mqtt_host = Some(cfg.host);
        // Only override port/creds if the env didn't provide them explicitly.
        if partial.mqtt_port.is_none() {
            partial.mqtt_port = Some(cfg.port);
        }
        if partial.mqtt_username.is_none() {
            partial.mqtt_username = cfg.username;
        }
        if partial.mqtt_password.is_none() {
            partial.mqtt_password = cfg.password;
        }
    }

    // The daemon's locations come out of the SAME rule a live command resolves
    // through (`resolve_store_location`), applied to the surfaces this merge
    // has already collapsed — so a command asking about this deployment lands
    // on the file this run is about to open.
    let StoreLocation {
        data_dir,
        store_path,
    } = store_location_from_stated(partial.data_dir, partial.store_path);
    let health_port = partial
        .health_port
        .unwrap_or(crate::settings_backends::automatic::HEALTH_PORT as u16);
    let review_port = partial
        .review_port
        .unwrap_or(crate::settings_backends::automatic::REVIEW_PORT as u16);
    let site_name = partial
        .site_name
        .unwrap_or_else(|| crate::settings_backends::automatic::SITE_NAME.to_string());
    let camera_name = partial
        .camera_name
        .unwrap_or_else(|| crate::settings_backends::automatic::CAMERA_NAME.to_string());
    let detector_model_id = partial
        .detector_model_id
        .unwrap_or_else(|| crate::settings_backends::automatic::DETECTOR_MODEL_ID.to_string());
    let detector_confidence_threshold =
        validate_confidence_threshold(partial.detector_confidence_threshold.unwrap_or(0.5))?;
    let detector_sample_frames = validate_detector_sample_frames(
        partial.detector_sample_frames.unwrap_or(5),
        "detector_sample_frames",
    )?;
    // Resolved through the typed settings registry: an operator-supplied
    // value (from any source already merged into `partial` above) is a
    // manual pin; nothing supplied leaves it Automatic at its declared
    // default. This is the one knob moved behind the registry so its
    // control states are real; the effective value is unchanged.
    // `declare_settings` is the single function that declares every
    // setting this crate has (see its own doc comment) — `load` and the
    // coverage check both go through it, so they can never disagree about
    // which settings exist.
    let mut settings_registry = SettingsRegistry::new();
    let declared_settings = declare_settings(&mut settings_registry);
    if let Some(value) = partial.detector_stationary_interval_secs {
        declared_settings
            .stationary_interval
            .set_manual(value)
            .map_err(|error| format!("detector_stationary_interval_secs {error}"))?;
    }
    let detector_stationary_interval_secs = declared_settings.stationary_interval.effective_value();
    // The four owner-ratified video-encoding levers (Part 1 of the
    // operator-adjustable-settings work): each follows the identical
    // resolve-through-the-registry shape as `stationary_interval` above —
    // an operator-supplied value from any already-merged source becomes a
    // manual pin; nothing supplied leaves the setting Automatic at its
    // ratified default (`crate::encode`'s `*_AUTOMATIC_DEFAULT` consts).
    if let Some(value) = partial.keyframe_interval_fps_multiplier {
        declared_settings
            .keyframe_interval_fps_multiplier
            .set_manual(value)
            .map_err(|error| format!("keyframe_interval_fps_multiplier {error}"))?;
    }
    let keyframe_interval_fps_multiplier = declared_settings
        .keyframe_interval_fps_multiplier
        .effective_value();
    if let Some(value) = partial.keyframe_interval_min_frames {
        declared_settings
            .keyframe_interval_min_frames
            .set_manual(value)
            .map_err(|error| format!("keyframe_interval_min_frames {error}"))?;
    }
    let keyframe_interval_min_frames = declared_settings
        .keyframe_interval_min_frames
        .effective_value();
    if let Some(value) = partial.keyframe_interval_max_frames {
        declared_settings
            .keyframe_interval_max_frames
            .set_manual(value)
            .map_err(|error| format!("keyframe_interval_max_frames {error}"))?;
    }
    let keyframe_interval_max_frames = declared_settings
        .keyframe_interval_max_frames
        .effective_value();
    // Each of the two bounds above is validated only against its own
    // independent range (1..=1800 / 1..=3600); nothing above checks the
    // PAIR against each other, so a min above the max would otherwise load
    // successfully and only panic later at
    // `f64::clamp(min, max)` (`crate::encode::automatic_keyframe_interval_frames`).
    // A configuration mistake must never become a runtime panic, so reject
    // the inverted pair here, naming both settings and both values, before
    // anything downstream can ever see it. `min == max` is legal (a clamp
    // to a single value never panics) so this is strictly `>`, never `>=`.
    if keyframe_interval_min_frames > keyframe_interval_max_frames {
        return Err(format!(
            "keyframe_interval_min_frames ({keyframe_interval_min_frames}) must not exceed keyframe_interval_max_frames ({keyframe_interval_max_frames})"
        ));
    }
    if let Some(value) = partial.bitrate_bps_up_to_640x480 {
        declared_settings
            .bitrate_bps_up_to_640x480
            .set_manual(value)
            .map_err(|error| format!("bitrate_bps_up_to_640x480 {error}"))?;
    }
    let bitrate_bps_up_to_640x480 = declared_settings
        .bitrate_bps_up_to_640x480
        .effective_value();
    if let Some(value) = partial.bitrate_bps_up_to_1280x720 {
        declared_settings
            .bitrate_bps_up_to_1280x720
            .set_manual(value)
            .map_err(|error| format!("bitrate_bps_up_to_1280x720 {error}"))?;
    }
    let bitrate_bps_up_to_1280x720 = declared_settings
        .bitrate_bps_up_to_1280x720
        .effective_value();
    if let Some(value) = partial.bitrate_bps_up_to_1920x1080 {
        declared_settings
            .bitrate_bps_up_to_1920x1080
            .set_manual(value)
            .map_err(|error| format!("bitrate_bps_up_to_1920x1080 {error}"))?;
    }
    let bitrate_bps_up_to_1920x1080 = declared_settings
        .bitrate_bps_up_to_1920x1080
        .effective_value();
    if let Some(value) = partial.bitrate_bps_up_to_2560x1440 {
        declared_settings
            .bitrate_bps_up_to_2560x1440
            .set_manual(value)
            .map_err(|error| format!("bitrate_bps_up_to_2560x1440 {error}"))?;
    }
    let bitrate_bps_up_to_2560x1440 = declared_settings
        .bitrate_bps_up_to_2560x1440
        .effective_value();
    if let Some(value) = partial.bitrate_bps_above_2560x1440 {
        declared_settings
            .bitrate_bps_above_2560x1440
            .set_manual(value)
            .map_err(|error| format!("bitrate_bps_above_2560x1440 {error}"))?;
    }
    let bitrate_bps_above_2560x1440 = declared_settings
        .bitrate_bps_above_2560x1440
        .effective_value();
    // Fabric knobs: absent ticket, hub embedding defaults off (criterion
    // C10 — every knob has a sane default, works with nothing provided).
    // Precedence CLI > env > options.json/fabric.toml — `partial.fabric_*`
    // at this point already reflects env-over-(config-file/options.json)
    // from the merges above; fabric.toml is consulted as one more
    // lowest-priority source (data_dir-relative, so only readable once
    // `data_dir` itself is resolved, just above), and the CLI flag —
    // deliberately excluded from the earlier CLI merge — is applied last.
    let fabric_toml = read_fabric_toml(&data_dir);
    // An empty ticket string from ANY source (a blank HAOS options.json
    // field mapped to `VIGIL_FABRIC_TICKET=""`, an empty fabric.toml key)
    // means the operator never configured a ticket — normalize it to `None`
    // here so it is treated identically to an untouched field, never as an
    // operator-typed malformed value that earns a "ticket rejected" error.
    // A non-empty but malformed ticket still passes through and is rejected
    // loudly at enrollment (criterion C6).
    let fabric_ticket = cli
        .fabric_ticket
        .or(partial.fabric_ticket)
        .or(fabric_toml.fabric_ticket)
        .map(|ticket| ticket.trim().to_string())
        .filter(|ticket| !ticket.is_empty());
    let fabric_hub = cli
        .fabric_hub
        .or(partial.fabric_hub)
        .or(fabric_toml.fabric_hub)
        .unwrap_or(false);
    // Default ON: the owner's binding promise (automatic offload on join)
    // requires default movement; this is the documented per-source opt-out
    // (criterion C10 / the addon privacy wording).
    let fabric_allow_frame_offload = partial.fabric_allow_frame_offload.unwrap_or(true);
    // Fabric tuning knobs (criterion C10, fix cycle 9): sane defaults match
    // today's hardcoded literals byte-identical (fabric.rs's
    // `lease_duration_ms: 5 * 60_000` and `OffloadPolicyConfig::default()`'s
    // `fallback_horizon_ms: 5_000`) — inert scaffold, neither is consumed
    // yet.
    let fabric_worker_lease_ms = partial
        .fabric_worker_lease_ms
        .unwrap_or(crate::settings_backends::automatic::FABRIC_WORKER_LEASE_MS as u64);
    let fabric_fallback_horizon_ms = partial
        .fabric_fallback_horizon_ms
        .unwrap_or(crate::settings_backends::automatic::FABRIC_FALLBACK_HORIZON_MS as u64);

    // Outbound connection: present when a host is configured. The password
    // stays wrapped in `Secret` all the way out of config resolution — an
    // integration adapter is the one that ultimately exposes it, at the
    // point it actually opens a connection.
    let mqtt = partial.mqtt_host.map(|host| ConnectionEndpoint {
        host,
        port: partial.mqtt_port.unwrap_or(1883),
        username: partial.mqtt_username,
        password: partial.mqtt_password,
    });

    // This node's service identifier, as the deployment supplied it. Empty
    // when nothing named this node: the identity is then derived once, at the
    // first start that has a store to persist it into, and read back from that
    // record on every later start. Deriving a fallback here as well would give
    // the product two derivations, and the one that runs earliest — this one —
    // is the one that follows the site name and moves the Home Assistant
    // device every time the site is renamed.
    let service_id = partial.service_id.unwrap_or_default();

    // Build the canonical multi-camera list.
    // If a `cameras` KEY is present in config/JSON it supersedes the single
    // camera_name / rtsp_url fields, even when the list it names is
    // explicitly empty — `cameras = []`, written deliberately, names a
    // legitimate worker/discovery node with zero cameras, never a request
    // for the legacy single-camera fallback. Only an ABSENT `cameras` key
    // (the field never appears in any config source at all) synthesizes the
    // one-element legacy list from the single-camera fields.
    let cameras: Vec<CameraEntry> = if let Some(cam_list) = partial.cameras {
        let mut resolved = Vec::with_capacity(cam_list.len());
        for c in cam_list {
            // Exactly-one-source-kind resolution: missing, empty,
            // invalid, conflicting, and artifact-unsupported source
            // fields are all rejected here — real `FieldOutcome`
            // classification against real validators, not a bare
            // non-empty check. `live_rtsp_url` is not a kind selector
            // (it only ever accompanies `rtsp_url`), so it gets its
            // own, identical Invalid-without-raw-echo validation.
            let kind = resolve_camera_source_kind(&c, &artifact_source_kind_capabilities())
                .map_err(|error| error.message)?;
            validate_live_rtsp_url(&c.name, c.live_rtsp_url.as_deref())
                .map_err(|error| error.message)?;
            resolved.push(CameraEntry {
                name: c.name,
                rtsp_url: c.rtsp_url,
                live_rtsp_url: c.live_rtsp_url,
                username: c.username,
                password: c.password,
                usb_device: c.usb_device,
                csi_module: c.csi_module,
                mjpeg_url: c.mjpeg_url,
                source_kind: Some(kind),
            });
        }
        resolved
    } else {
        // The legacy single-camera fields, synthesized into one entry. Now
        // that an explicit empty `[[cameras]]` list means zero cameras (see
        // above), this branch only ever runs when `cameras` was never
        // mentioned at all — so it can validate exactly like the table
        // route does: a declared source kind this artifact cannot carry is
        // refused identically (naming the same kind), through the same
        // resolve_camera_source_kind/validate_live_rtsp_url path the
        // [[cameras]] table uses. A legacy deployment that declares NO
        // source field at all keeps loading successfully (the documented,
        // still-open pre-OSS zero-camera gap in docs/configuration.md) —
        // resolving that gap is a separate concern from source-kind
        // validation, so it is left untouched here.
        let legacy_entry = CameraEntryPartial {
            name: camera_name.clone(),
            rtsp_url: partial.rtsp_url.clone(),
            live_rtsp_url: partial.live_rtsp_url.clone(),
            username: partial.rtsp_username.clone(),
            password: partial.rtsp_password.clone(),
            usb_device: partial.usb_device.clone(),
            csi_module: partial.csi_module.clone(),
            mjpeg_url: partial.mjpeg_url.clone(),
            // The legacy single-camera form has no per-camera field of its
            // own; what such a deployment sets deployment-wide is what its one
            // camera runs.
            motion_sensitivity: None,
        };
        let declares_no_source = legacy_entry.rtsp_url.is_none()
            && legacy_entry.usb_device.is_none()
            && legacy_entry.csi_module.is_none()
            && legacy_entry.mjpeg_url.is_none();
        let legacy_kind = if !declares_no_source {
            let kind =
                resolve_camera_source_kind(&legacy_entry, &artifact_source_kind_capabilities())
                    .map_err(|error| error.message)?;
            validate_live_rtsp_url(&legacy_entry.name, legacy_entry.live_rtsp_url.as_deref())
                .map_err(|error| error.message)?;
            Some(kind)
        } else {
            None
        };
        vec![CameraEntry {
            name: legacy_entry.name,
            rtsp_url: legacy_entry.rtsp_url,
            live_rtsp_url: legacy_entry.live_rtsp_url,
            username: legacy_entry.username,
            password: legacy_entry.password,
            usb_device: legacy_entry.usb_device,
            csi_module: legacy_entry.csi_module,
            mjpeg_url: legacy_entry.mjpeg_url,
            source_kind: legacy_kind,
        }]
    };

    reject_duplicate_camera_analysis_endpoints(&cameras).map_err(|error| error.message)?;

    // Recognition switches on when a weights directory is configured. Class
    // coverage is independent of detection: it starts from its own default
    // (`settings_backends::default_recognition_covered_classes`), never
    // `RecognitionConfig::default()`'s wider engine baseline, so widening
    // `detector_classes` never silently widens what recognition covers too.
    let mut recognition = crate::recognition::RecognitionConfig {
        covered_classes: crate::settings_backends::default_recognition_covered_classes()
            .iter()
            .map(|class| (*class).to_string())
            .collect(),
        ..crate::recognition::RecognitionConfig::default()
    };
    if let Some(weights_dir) = partial.recognition_weights_dir {
        recognition.enabled = true;
        recognition.weights_dir = Some(weights_dir);
    }
    if let Some(space) = partial.recognition_space_id {
        recognition.embedding_space_id = space;
    }
    if let Some(threshold) = partial.recognition_threshold {
        recognition.match_threshold = validate_recognition_threshold(threshold)?;
    }
    if let Some(classes) = partial.recognition_covered_classes {
        recognition.covered_classes = classes;
    }

    Ok(RuntimeConfig {
        file_surface,
        startup_surface,
        data_dir,
        store_path,
        health_port,
        review_port,
        site_name,
        camera_name,
        rtsp_url: partial.rtsp_url,
        rtsp_username: partial.rtsp_username,
        rtsp_password: partial.rtsp_password,
        detector_model_id,
        detector_model_path: partial.detector_model_path,
        detector_confidence_threshold,
        detector_sample_frames,
        detector_stationary_interval_secs,
        cameras,
        mqtt,
        service_id,
        recognition,
        detector_class_indices: crate::settings_backends::automatic_detection_class_indices(),
        detector_queue_capacity: crate::settings_backends::AUTOMATIC_DETECTOR_QUEUE_CAPACITY,
        rtsp_retry_initial_ms: crate::settings_backends::automatic::RTSP_RETRY_INITIAL_MS as u64,
        rtsp_retry_max_ms: crate::settings_backends::automatic::RTSP_RETRY_MAX_MS as u64,
        // Intent booleans: absent means true (probe, use only on a
        // passed probe, fall back visibly).
        hardware_decoding: partial.hardware_decoding.unwrap_or(true),
        accelerated_detection: partial.accelerated_detection.unwrap_or(true),
        fabric_ticket,
        fabric_hub,
        fabric_allow_frame_offload,
        fabric_worker_lease_ms,
        fabric_fallback_horizon_ms,
        keyframe_interval_fps_multiplier,
        keyframe_interval_min_frames,
        keyframe_interval_max_frames,
        bitrate_bps_up_to_640x480,
        bitrate_bps_up_to_1280x720,
        bitrate_bps_up_to_1920x1080,
        bitrate_bps_up_to_2560x1440,
        bitrate_bps_above_2560x1440,
    })
}

fn parse_cli(args: Vec<OsString>) -> Result<CliOverrides, String> {
    let mut cli = CliOverrides::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let arg = arg
            .into_string()
            .map_err(|_| "run arguments must be valid UTF-8".to_string())?;
        match arg.as_str() {
            "--config" => cli.config_path = Some(next_path(&mut iter, "--config")?),
            "--data-dir" => cli.data_dir = Some(next_path(&mut iter, "--data-dir")?),
            "--store-path" => cli.store_path = Some(next_path(&mut iter, "--store-path")?),
            "--recognition-weights-dir" => {
                cli.recognition_weights_dir =
                    Some(next_path(&mut iter, "--recognition-weights-dir")?)
            }
            "--health-port" => cli.health_port = Some(next_port(&mut iter, "--health-port")?),
            "--review-port" => cli.review_port = Some(next_port(&mut iter, "--review-port")?),
            "--site-name" => cli.site_name = Some(next_string(&mut iter, "--site-name")?),
            "--camera-name" => cli.camera_name = Some(next_string(&mut iter, "--camera-name")?),
            "--rtsp-url" => cli.rtsp_url = Some(next_string(&mut iter, "--rtsp-url")?),
            "--live-rtsp-url" => {
                cli.live_rtsp_url = Some(next_string(&mut iter, "--live-rtsp-url")?)
            }
            // The legacy single-camera USB/CSI/MJPEG counterparts of
            // --rtsp-url, following the identical parsing shape. Credentials
            // are NOT a separate flag here: --rtsp-username/--rtsp-password
            // already populate the legacy single camera's shared
            // username/password fields regardless of source kind (see
            // CameraEntry, where username/password are not RTSP-specific),
            // and an MJPEG URL can also carry embedded userinfo the same way
            // rtsp_url already can.
            "--usb-device" => cli.usb_device = Some(next_string(&mut iter, "--usb-device")?),
            "--csi-module" => cli.csi_module = Some(next_string(&mut iter, "--csi-module")?),
            "--mjpeg-url" => cli.mjpeg_url = Some(next_string(&mut iter, "--mjpeg-url")?),
            "--rtsp-username" => {
                cli.rtsp_username = Some(next_string(&mut iter, "--rtsp-username")?)
            }
            "--rtsp-password" => {
                cli.rtsp_password = Some(Secret::new(next_string(&mut iter, "--rtsp-password")?))
            }
            "--detector-model-id" => {
                cli.detector_model_id = Some(next_string(&mut iter, "--detector-model-id")?)
            }
            "--detector-model-path" => {
                cli.detector_model_path = Some(next_path(&mut iter, "--detector-model-path")?)
            }
            "--detector-confidence-threshold" => {
                let value = next_string(&mut iter, "--detector-confidence-threshold")?;
                cli.detector_confidence_threshold = Some(value.parse().map_err(|error| {
                    format!("--detector-confidence-threshold must be a number: {error}")
                })?);
            }
            "--detector-sample-frames" => {
                let value = next_string(&mut iter, "--detector-sample-frames")?;
                cli.detector_sample_frames = Some(value.parse().map_err(|error| {
                    format!("--detector-sample-frames must be an integer: {error}")
                })?);
            }
            "--detector-stationary-interval-secs" => {
                let value = next_string(&mut iter, "--detector-stationary-interval-secs")?;
                cli.detector_stationary_interval_secs = Some(value.parse().map_err(|error| {
                    format!("--detector-stationary-interval-secs must be an integer: {error}")
                })?);
            }
            "--hardware-decoding" => {
                cli.hardware_decoding = Some(next_bool(&mut iter, "--hardware-decoding")?)
            }
            "--accelerated-detection" => {
                cli.accelerated_detection = Some(next_bool(&mut iter, "--accelerated-detection")?)
            }
            "--fabric-ticket" => {
                cli.fabric_ticket = Some(next_string(&mut iter, "--fabric-ticket")?)
            }
            "--fabric-hub" => cli.fabric_hub = Some(next_bool(&mut iter, "--fabric-hub")?),
            "--fabric-allow-frame-offload" => {
                cli.fabric_allow_frame_offload =
                    Some(next_bool(&mut iter, "--fabric-allow-frame-offload")?)
            }
            "--fabric-worker-lease-ms" => {
                let value = next_string(&mut iter, "--fabric-worker-lease-ms")?;
                cli.fabric_worker_lease_ms = Some(value.parse().map_err(|error| {
                    format!("--fabric-worker-lease-ms must be an integer: {error}")
                })?);
            }
            "--fabric-fallback-horizon-ms" => {
                let value = next_string(&mut iter, "--fabric-fallback-horizon-ms")?;
                cli.fabric_fallback_horizon_ms = Some(value.parse().map_err(|error| {
                    format!("--fabric-fallback-horizon-ms must be an integer: {error}")
                })?);
            }
            "--help" | "-h" => return Err(run_usage()),
            other => return Err(format!("{other} is not a supported run option")),
        }
    }
    Ok(cli)
}

fn next_string(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("{flag} requires a value"))
        .and_then(|value| {
            value
                .into_string()
                .map_err(|_| format!("{flag} value must be valid UTF-8"))
        })
}

fn next_path(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<PathBuf, String> {
    iter.next()
        .ok_or_else(|| format!("{flag} requires a value"))
        .and_then(|value| {
            value
                .into_string()
                .map(PathBuf::from)
                .map_err(|_| format!("{flag} value must be valid UTF-8"))
        })
}

fn next_port(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<u16, String> {
    let value = next_path(iter, flag)?;
    value
        .to_string_lossy()
        .parse::<u16>()
        .map_err(|error| format!("{flag} must be a TCP port: {error}"))
}

fn next_bool(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<bool, String> {
    let value = next_string(iter, flag)?;
    parse_intent_bool(&value).ok_or_else(|| format!("{flag} must be true or false, got {value}"))
}

/// Intent booleans accept exactly true/false (case-insensitive).
fn parse_intent_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn validate_confidence_threshold(value: f64) -> Result<f64, String> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "detector_confidence_threshold must be between 0.0 and 1.0, got {value}"
        ))
    }
}

fn validate_recognition_threshold(value: f64) -> Result<f64, String> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "recognition_threshold must be between 0.0 and 1.0, got {value}"
        ))
    }
}

fn validate_detector_sample_frames(value: usize, name: &str) -> Result<usize, String> {
    if (1..=64).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{name} must be between 1 and 64, got {value}"))
    }
}

/// Every setting this crate declares through the typed settings registry,
/// handed back to whoever declared them.
pub(crate) struct DeclaredSettings {
    pub(crate) stationary_interval: SettingHandle<u64>,
    pub(crate) keyframe_interval_fps_multiplier: SettingHandle<u32>,
    pub(crate) keyframe_interval_min_frames: SettingHandle<u32>,
    pub(crate) keyframe_interval_max_frames: SettingHandle<u32>,
    pub(crate) bitrate_bps_up_to_640x480: SettingHandle<u32>,
    pub(crate) bitrate_bps_up_to_1280x720: SettingHandle<u32>,
    pub(crate) bitrate_bps_up_to_1920x1080: SettingHandle<u32>,
    pub(crate) bitrate_bps_up_to_2560x1440: SettingHandle<u32>,
    pub(crate) bitrate_bps_above_2560x1440: SettingHandle<u32>,
}

/// The single production function that declares every setting this crate
/// has. `load` (to resolve real values) and `declared_settings` at the
/// crate root (to check coverage) both call this and nothing else
/// constructs a config setting's handle, so the two can never disagree
/// about which settings exist: adding a setting means adding one field to
/// [`DeclaredSettings`] and one `registry.declare` call here, and both
/// callers pick it up without further edits.
///
/// This is a structural guarantee, enforced two ways, not a discipline
/// one:
/// - Outside this crate, it is a compile error: [`SettingsRegistry::new`]
///   and [`SettingsRegistry::declare`] are both `pub(crate)`, so no
///   external caller can build a registry at all, let alone declare a
///   setting on one (see the doctest on [`SettingsRegistry`] itself).
/// - Inside this crate, a `declare` call added to any function other than
///   this one is a gate failure: `settings_registry_declare_boundary.rs`
///   scans the crate's own source and fails the moment `declare(` appears
///   anywhere outside this function's body (its own unit tests in
///   `settings.rs` are the sole, reviewed exception, since they exist to
///   test the registry primitive itself, not to resolve a real setting).
///
/// [`SettingHandle::new`] is private to the `settings` module, so the only
/// way to obtain one — including the one this function returns — is
/// [`SettingsRegistry::declare`], which always records the setting on the
/// registry's coverage entries first. A handle that reaches
/// [`DeclaredSettings`] without having gone through that call cannot exist.
pub(crate) fn declare_settings(registry: &mut SettingsRegistry) -> DeclaredSettings {
    DeclaredSettings {
        stationary_interval: registry.declare(stationary_interval_setting_spec()),
        keyframe_interval_fps_multiplier: registry
            .declare(keyframe_interval_fps_multiplier_setting_spec()),
        keyframe_interval_min_frames: registry.declare(keyframe_interval_min_frames_setting_spec()),
        keyframe_interval_max_frames: registry.declare(keyframe_interval_max_frames_setting_spec()),
        bitrate_bps_up_to_640x480: registry.declare(bitrate_bps_up_to_640x480_setting_spec()),
        bitrate_bps_up_to_1280x720: registry.declare(bitrate_bps_up_to_1280x720_setting_spec()),
        bitrate_bps_up_to_1920x1080: registry.declare(bitrate_bps_up_to_1920x1080_setting_spec()),
        bitrate_bps_up_to_2560x1440: registry.declare(bitrate_bps_up_to_2560x1440_setting_spec()),
        bitrate_bps_above_2560x1440: registry.declare(bitrate_bps_above_2560x1440_setting_spec()),
    }
}

/// The stationary scan interval's settings-registry declaration: a stable
/// name, its today-unchanged default and validation, and where an owner
/// sees and changes it. Unconstrained, matching the resolution this
/// replaces (any interval was previously accepted).
fn stationary_interval_setting_spec() -> SettingSpec<u64> {
    SettingSpec {
        name: "detector_stationary_interval_secs",
        default: 30,
        validate: |_value| Ok(()),
        surfaces: SettingSurfaces {
            config_key: "detector_stationary_interval_secs",
            addon_option_key: "detector_stationary_interval_secs",
            documentation_page: "docs/configuration.md",
        },
    }
}

fn validate_keyframe_interval_fps_multiplier(value: &u32) -> Result<(), String> {
    if (1..=10).contains(value) {
        Ok(())
    } else {
        Err(format!(
            "keyframe_interval_fps_multiplier must be between 1 and 10, got {value}"
        ))
    }
}

/// The keyframe-interval multiplier's settings-registry declaration: see
/// `crate::encode::KEYFRAME_INTERVAL_FPS_MULTIPLIER_AUTOMATIC_DEFAULT` for
/// the single-sourced ratified default this reads.
fn keyframe_interval_fps_multiplier_setting_spec() -> SettingSpec<u32> {
    SettingSpec {
        name: "keyframe_interval_fps_multiplier",
        default: crate::encode::KEYFRAME_INTERVAL_FPS_MULTIPLIER_AUTOMATIC_DEFAULT,
        validate: validate_keyframe_interval_fps_multiplier,
        surfaces: SettingSurfaces {
            config_key: "keyframe_interval_fps_multiplier",
            addon_option_key: "keyframe_interval_fps_multiplier",
            documentation_page: "docs/configuration.md",
        },
    }
}

fn validate_keyframe_interval_min_frames(value: &u32) -> Result<(), String> {
    if (1..=1800).contains(value) {
        Ok(())
    } else {
        Err(format!(
            "keyframe_interval_min_frames must be between 1 and 1800, got {value}"
        ))
    }
}

/// The keyframe-interval lower clamp's settings-registry declaration.
/// `AUTOMATIC_CAPACITY_FLOOR_FRAMES` in `camera_hub.rs` derives from the
/// SAME `KEYFRAME_INTERVAL_MIN_FRAMES_AUTOMATIC_DEFAULT` constant this
/// reads as its default, rather than carrying an independent literal.
fn keyframe_interval_min_frames_setting_spec() -> SettingSpec<u32> {
    SettingSpec {
        name: "keyframe_interval_min_frames",
        default: crate::encode::KEYFRAME_INTERVAL_MIN_FRAMES_AUTOMATIC_DEFAULT,
        validate: validate_keyframe_interval_min_frames,
        surfaces: SettingSurfaces {
            config_key: "keyframe_interval_min_frames",
            addon_option_key: "keyframe_interval_min_frames",
            documentation_page: "docs/configuration.md",
        },
    }
}

fn validate_keyframe_interval_max_frames(value: &u32) -> Result<(), String> {
    if (1..=3600).contains(value) {
        Ok(())
    } else {
        Err(format!(
            "keyframe_interval_max_frames must be between 1 and 3600, got {value}"
        ))
    }
}

/// The keyframe-interval upper clamp's settings-registry declaration.
fn keyframe_interval_max_frames_setting_spec() -> SettingSpec<u32> {
    SettingSpec {
        name: "keyframe_interval_max_frames",
        default: crate::encode::KEYFRAME_INTERVAL_MAX_FRAMES_AUTOMATIC_DEFAULT,
        validate: validate_keyframe_interval_max_frames,
        surfaces: SettingSurfaces {
            config_key: "keyframe_interval_max_frames",
            addon_option_key: "keyframe_interval_max_frames",
            documentation_page: "docs/configuration.md",
        },
    }
}

/// Sane bounds shared by every per-resolution-class bitrate setting: below
/// 100kbps nothing recognizable decodes, and above 100Mbps is far past any
/// resolution class this artifact encodes today — the bound exists to
/// catch a typo (an extra digit), not to opine on real-world bitrate
/// tuning.
fn validate_bitrate_bps(name: &'static str, value: &u32) -> Result<(), String> {
    if (100_000..=100_000_000).contains(value) {
        Ok(())
    } else {
        Err(format!(
            "{name} must be between 100000 and 100000000, got {value}"
        ))
    }
}

fn validate_bitrate_bps_up_to_640x480(value: &u32) -> Result<(), String> {
    validate_bitrate_bps("bitrate_bps_up_to_640x480", value)
}

fn bitrate_bps_up_to_640x480_setting_spec() -> SettingSpec<u32> {
    SettingSpec {
        name: "bitrate_bps_up_to_640x480",
        default: crate::encode::BITRATE_BPS_UP_TO_640X480_AUTOMATIC_DEFAULT,
        validate: validate_bitrate_bps_up_to_640x480,
        surfaces: SettingSurfaces {
            config_key: "bitrate_bps_up_to_640x480",
            addon_option_key: "bitrate_bps_up_to_640x480",
            documentation_page: "docs/configuration.md",
        },
    }
}

fn validate_bitrate_bps_up_to_1280x720(value: &u32) -> Result<(), String> {
    validate_bitrate_bps("bitrate_bps_up_to_1280x720", value)
}

fn bitrate_bps_up_to_1280x720_setting_spec() -> SettingSpec<u32> {
    SettingSpec {
        name: "bitrate_bps_up_to_1280x720",
        default: crate::encode::BITRATE_BPS_UP_TO_1280X720_AUTOMATIC_DEFAULT,
        validate: validate_bitrate_bps_up_to_1280x720,
        surfaces: SettingSurfaces {
            config_key: "bitrate_bps_up_to_1280x720",
            addon_option_key: "bitrate_bps_up_to_1280x720",
            documentation_page: "docs/configuration.md",
        },
    }
}

fn validate_bitrate_bps_up_to_1920x1080(value: &u32) -> Result<(), String> {
    validate_bitrate_bps("bitrate_bps_up_to_1920x1080", value)
}

fn bitrate_bps_up_to_1920x1080_setting_spec() -> SettingSpec<u32> {
    SettingSpec {
        name: "bitrate_bps_up_to_1920x1080",
        default: crate::encode::BITRATE_BPS_UP_TO_1920X1080_AUTOMATIC_DEFAULT,
        validate: validate_bitrate_bps_up_to_1920x1080,
        surfaces: SettingSurfaces {
            config_key: "bitrate_bps_up_to_1920x1080",
            addon_option_key: "bitrate_bps_up_to_1920x1080",
            documentation_page: "docs/configuration.md",
        },
    }
}

fn validate_bitrate_bps_up_to_2560x1440(value: &u32) -> Result<(), String> {
    validate_bitrate_bps("bitrate_bps_up_to_2560x1440", value)
}

fn bitrate_bps_up_to_2560x1440_setting_spec() -> SettingSpec<u32> {
    SettingSpec {
        name: "bitrate_bps_up_to_2560x1440",
        default: crate::encode::BITRATE_BPS_UP_TO_2560X1440_AUTOMATIC_DEFAULT,
        validate: validate_bitrate_bps_up_to_2560x1440,
        surfaces: SettingSurfaces {
            config_key: "bitrate_bps_up_to_2560x1440",
            addon_option_key: "bitrate_bps_up_to_2560x1440",
            documentation_page: "docs/configuration.md",
        },
    }
}

fn validate_bitrate_bps_above_2560x1440(value: &u32) -> Result<(), String> {
    validate_bitrate_bps("bitrate_bps_above_2560x1440", value)
}

fn bitrate_bps_above_2560x1440_setting_spec() -> SettingSpec<u32> {
    SettingSpec {
        name: "bitrate_bps_above_2560x1440",
        default: crate::encode::BITRATE_BPS_ABOVE_2560X1440_AUTOMATIC_DEFAULT,
        validate: validate_bitrate_bps_above_2560x1440,
        surfaces: SettingSurfaces {
            config_key: "bitrate_bps_above_2560x1440",
            addon_option_key: "bitrate_bps_above_2560x1440",
            documentation_page: "docs/configuration.md",
        },
    }
}

/// Whether a standalone-config TOML fragment assigning `raw_value` to
/// `name` actually sets the field it names, using the real config-file
/// parser (`PartialConfig`). Test/tooling support for the settings-registry
/// coverage check: parsing a fragment and confirming it lands on the right
/// field cannot be done generically over an arbitrary setting name, so this
/// dispatches by name, one declared setting at a time, and stays honest by
/// refusing to guess for a name it does not recognize.
pub(crate) fn config_file_fragment_sets(name: &str, raw_value: &str) -> bool {
    let fragment = format!("{name} = {raw_value}");
    let Ok(parsed) = toml::from_str::<PartialConfig>(&fragment) else {
        return false;
    };
    match name {
        "detector_stationary_interval_secs" => {
            parsed.detector_stationary_interval_secs == raw_value.parse().ok()
        }
        "keyframe_interval_fps_multiplier" => {
            parsed.keyframe_interval_fps_multiplier == raw_value.parse().ok()
        }
        "keyframe_interval_min_frames" => {
            parsed.keyframe_interval_min_frames == raw_value.parse().ok()
        }
        "keyframe_interval_max_frames" => {
            parsed.keyframe_interval_max_frames == raw_value.parse().ok()
        }
        "bitrate_bps_up_to_640x480" => parsed.bitrate_bps_up_to_640x480 == raw_value.parse().ok(),
        "bitrate_bps_up_to_1280x720" => parsed.bitrate_bps_up_to_1280x720 == raw_value.parse().ok(),
        "bitrate_bps_up_to_1920x1080" => {
            parsed.bitrate_bps_up_to_1920x1080 == raw_value.parse().ok()
        }
        "bitrate_bps_up_to_2560x1440" => {
            parsed.bitrate_bps_up_to_2560x1440 == raw_value.parse().ok()
        }
        "bitrate_bps_above_2560x1440" => {
            parsed.bitrate_bps_above_2560x1440 == raw_value.parse().ok()
        }
        _ => false,
    }
}

fn read_toml_config(path: &Path) -> Result<PartialConfig, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read config {}: {error}", path.display()))?;
    toml::from_str(&text).map_err(|error| {
        format!(
            "could not parse config {}: {}",
            path.display(),
            describe_toml_parse_error(&text, &error)
        )
    })
}

/// Describe a TOML parse error by a STATIC category plus LOCATION only —
/// never by reproducing anything from the error itself. Two leaks this
/// closes, not one: `toml::de::Error`'s own `Display` impl renders an
/// annotated snippet of the offending source line by default (the crate's
/// documented behavior); and, less obviously, `error.message()` ALONE is
/// also unsafe to use, because for a typed-deserialization failure (a
/// non-boolean value for `hardware_decoding`, for instance) that message
/// is serde's own `invalid type: string "hunter2", expected a boolean` —
/// the offending scalar embedded in the "bare" message text, not only in
/// the annotated snippet. So `error.message()`/`error.to_string()` are
/// NEVER called here, matching [`describe_json_parse_error`]'s shape
/// exactly: a fixed category string plus `error.span()` converted to a
/// 1-based line/column, plus the offending KEY read directly from the
/// source line (never anything past its `=`, since a TOML key is never
/// itself a secret, only a value can be).
///
/// `toml::de::Error` has no public `classify()`-equivalent (unlike
/// [`serde_json::Error`]), so unlike the JSON helper this cannot
/// distinguish a syntax error from a data/type error without inspecting
/// `message()` text — which is exactly what must not happen. One honest,
/// static category covers every case instead of guessing.
fn describe_toml_parse_error(text: &str, error: &toml::de::Error) -> String {
    const CATEGORY: &str = "invalid TOML configuration value";
    let Some(span) = error.span() else {
        return CATEGORY.to_string();
    };
    let start = span.start.min(text.len());
    let line_number = text[..start].matches('\n').count() + 1;
    let line_start = text[..start]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let column = start - line_start + 1;
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |index| line_start + index);
    let line = &text[line_start..line_end];
    // Name the KEY the problem is on, never anything past its `=` — a
    // TOML key is never itself a secret, only a value can be, so slicing
    // only the key portion of the offending line stays safe even though
    // we are reading the source text to build it.
    let key_hint = line.split('=').next().map(str::trim).filter(|key| {
        !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    });
    match key_hint {
        Some(key) => format!("{CATEGORY} (`{key}`) at line {line_number}, column {column}"),
        None => format!("{CATEGORY} at line {line_number}, column {column}"),
    }
}

fn read_options_json(path: &Path) -> Result<PartialConfig, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read options {}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| {
        format!(
            "could not parse options {}: {}",
            path.display(),
            describe_json_parse_error(&text, &error)
        )
    })
}

/// Describe a `serde_json` parse/deserialize error by LOCATION and KEY
/// only — never by using `serde_json::Error`'s own `Display`. This is the
/// `/data/options.json` (Home Assistant add-on options) counterpart of
/// [`describe_toml_parse_error`], and matters MORE than the TOML case:
/// `/data/options.json` is the DEFAULT production configuration surface
/// for a real add-on deployment, and it is exactly where an operator's
/// MJPEG camera password lives. `serde_json::Error`'s `Display` renders a
/// type-mismatch error as `invalid type: string "hunter2", expected u16
/// at line 3 column 20` — the offending scalar VERBATIM — so a mistyped
/// credential-bearing option would otherwise leak the credential straight
/// into the add-on log. Built ONLY from `error.line()`/`error.column()`/
/// `error.classify()` plus our own safe key extraction (the JSON key
/// portion of the offending line, sliced before its first `:` — a JSON
/// object key is never itself a secret, only the value after it is);
/// `error.to_string()`/`format!("{error}")` are never called here.
fn describe_json_parse_error(text: &str, error: &serde_json::Error) -> String {
    let category = match error.classify() {
        serde_json::error::Category::Io => "I/O error reading options",
        serde_json::error::Category::Syntax => "JSON syntax error",
        serde_json::error::Category::Data => "invalid value for an option",
        serde_json::error::Category::Eof => "unexpected end of options file",
    };
    let line_number = error.line();
    let column = error.column();
    if line_number == 0 {
        return category.to_string();
    }
    let key_hint = text
        .lines()
        .nth(line_number - 1)
        .and_then(|line| json_key_before_column(line, column));
    match key_hint {
        Some(key) => format!("{category} (`{key}`) at line {line_number}, column {column}"),
        None => format!("{category} at line {line_number}, column {column}"),
    }
}

/// The JSON object key whose `: value` the error's column falls inside,
/// found by looking BACKWARD from the error position for the nearest
/// `"key":` — never by taking the first key on the line, which a compact
/// (single-line, many-keys) options.json would get wrong: the offending
/// key is not necessarily the line's first one.
fn json_key_before_column(line: &str, column: usize) -> Option<String> {
    let prefix_end = column.saturating_sub(1).min(line.len());
    let prefix = &line[..prefix_end];
    let colon_pos = prefix.rfind(':')?;
    let before_colon = prefix[..colon_pos].trim_end();
    let closing_quote_stripped = before_colon.strip_suffix('"')?;
    let opening_quote = closing_quote_stripped.rfind('"')?;
    let key = &closing_quote_stripped[opening_quote + 1..];
    (!key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    .then(|| key.to_string())
}

// Every read below is a baselined legacy environment read (an
// add-on/Supervisor-injected value), enumerated in
// `environment_read_surface.baseline.txt` and pending migration to the
// settings registry — only the stationary scan interval is a declared
// setting today (see `stationary_interval_setting_spec`); this is the one
// function that gathers the rest, not an ad-hoc scattering.
#[allow(clippy::disallowed_methods)]
fn env_overrides() -> Result<PartialConfig, String> {
    // What the environment is still for, now that it is not a settings surface:
    // the bootstrap locations that must be readable before the store can be
    // opened, the secrets that stay settable per process, the platform's own
    // injected service discovery, and this node's identity. Every behavior
    // value that used to be read here is a setting now — written through the
    // add-on options, the config file, the startup options, or a `vigil
    // settings` change, and resolved from the store. Naming one of them in the
    // environment is reported as ignored rather than silently doing nothing
    // (`settings_environment::ignored_behavior_variables`).
    Ok(PartialConfig {
        // Bootstrap locations: a store cannot say where the store is.
        data_dir: std::env::var_os("VIGIL_DATA_DIR").map(PathBuf::from),
        store_path: std::env::var_os("VIGIL_STORE_PATH").map(PathBuf::from),
        health_port: None,
        review_port: None,
        site_name: None,
        camera_name: None,
        rtsp_url: None,
        live_rtsp_url: None,
        usb_device: None,
        csi_module: None,
        mjpeg_url: None,
        // Secrets: settable through the ordinary surfaces AND here, with the
        // environment winning, because a rotation just handed this run a
        // credential and a stored value shadowing it is the silent
        // stale-credential failure the model refuses everywhere else.
        rtsp_username: std::env::var("VIGIL_RTSP_USERNAME").ok(),
        rtsp_password: std::env::var("VIGIL_RTSP_PASSWORD").ok().map(Secret::new),
        detector_model_id: None,
        detector_model_path: None,
        recognition_weights_dir: None,
        recognition_space_id: None,
        recognition_threshold: None,
        recognition_covered_classes: None,
        detector_classes: None,
        decode_probe_deadline_secs: None,
        // Behavior settings are never expressed through the environment, so
        // the environment asserts nothing about any of these three.
        hardware_probe_deadline_secs: None,
        rtsp_retry_initial_ms: None,
        rtsp_retry_max_ms: None,
        detector_confidence_threshold: None,
        detector_sample_frames: None,
        detector_stationary_interval_secs: None,
        // Platform-injected service discovery: the Supervisor sets these
        // because the add-on declares MQTT as a wanted service. Nobody typed
        // them, and pinning them would let a rotated broker password be
        // shadowed forever by a stale record — taking down event delivery,
        // silently.
        mqtt_host: std::env::var("MQTT_HOST").ok(),
        mqtt_port: match std::env::var("MQTT_PORT") {
            Ok(value) => Some(
                value
                    .parse::<u16>()
                    .map_err(|error| format!("MQTT_PORT must be a TCP port: {error}"))?,
            ),
            Err(_) => None,
        },
        mqtt_username: std::env::var("MQTT_USER")
            .ok()
            .or_else(|| std::env::var("MQTT_USERNAME").ok()),
        mqtt_password: std::env::var("MQTT_PASSWORD").ok().map(Secret::new),
        // Bootstrap identity: it names this node's messaging topics and its
        // Home Assistant device, so it is derived once and persisted rather
        // than recomputed, and it is readable before the store opens.
        service_id: std::env::var("VIGIL_SERVICE_ID").ok(),
        hardware_decoding: None,
        accelerated_detection: None,
        // Behavior values, every one of them: they are settings now, and the
        // environment is not a surface that authors settings.
        motion_sensitivity: None,
        restart_on_reflect: None,
        detector_queue_capacity: None,
        detection_backend: None,
        decode_backend: None,
        // An enrollment credential, on the same footing as a camera password.
        fabric_ticket: std::env::var("VIGIL_FABRIC_TICKET").ok(),
        fabric_hub: None,
        fabric_allow_frame_offload: None,
        fabric_worker_lease_ms: None,
        fabric_fallback_horizon_ms: None,
        cameras: None,
        keyframe_interval_fps_multiplier: None,
        keyframe_interval_min_frames: None,
        keyframe_interval_max_frames: None,
        bitrate_bps_up_to_640x480: None,
        bitrate_bps_up_to_1280x720: None,
        bitrate_bps_up_to_1920x1080: None,
        bitrate_bps_up_to_2560x1440: None,
        bitrate_bps_above_2560x1440: None,
    })
}

fn merge(target: &mut PartialConfig, source: PartialConfig) {
    if source.data_dir.is_some() {
        target.data_dir = source.data_dir;
    }
    if source.store_path.is_some() {
        target.store_path = source.store_path;
    }
    if source.health_port.is_some() {
        target.health_port = source.health_port;
    }
    if source.review_port.is_some() {
        target.review_port = source.review_port;
    }
    if source.site_name.is_some() {
        target.site_name = source.site_name;
    }
    if source.camera_name.is_some() {
        target.camera_name = source.camera_name;
    }
    if source.rtsp_url.is_some() {
        target.rtsp_url = source.rtsp_url;
    }
    if source.live_rtsp_url.is_some() {
        target.live_rtsp_url = source.live_rtsp_url;
    }
    if source.rtsp_username.is_some() {
        target.rtsp_username = source.rtsp_username;
    }
    if source.rtsp_password.is_some() {
        target.rtsp_password = source.rtsp_password;
    }
    if source.usb_device.is_some() {
        target.usb_device = source.usb_device;
    }
    if source.csi_module.is_some() {
        target.csi_module = source.csi_module;
    }
    if source.mjpeg_url.is_some() {
        target.mjpeg_url = source.mjpeg_url;
    }
    if source.detector_model_id.is_some() {
        target.detector_model_id = source.detector_model_id;
    }
    if source.detector_model_path.is_some() {
        target.detector_model_path = source.detector_model_path;
    }
    if source.detector_confidence_threshold.is_some() {
        target.detector_confidence_threshold = source.detector_confidence_threshold;
    }
    if source.recognition_weights_dir.is_some() {
        target.recognition_weights_dir = source.recognition_weights_dir;
    }
    if source.recognition_space_id.is_some() {
        target.recognition_space_id = source.recognition_space_id;
    }
    if source.recognition_threshold.is_some() {
        target.recognition_threshold = source.recognition_threshold;
    }
    if source.recognition_covered_classes.is_some() {
        target.recognition_covered_classes = source.recognition_covered_classes;
    }
    if source.detector_classes.is_some() {
        target.detector_classes = source.detector_classes;
    }
    if source.decode_probe_deadline_secs.is_some() {
        target.decode_probe_deadline_secs = source.decode_probe_deadline_secs;
    }
    if source.hardware_probe_deadline_secs.is_some() {
        target.hardware_probe_deadline_secs = source.hardware_probe_deadline_secs;
    }
    if source.rtsp_retry_initial_ms.is_some() {
        target.rtsp_retry_initial_ms = source.rtsp_retry_initial_ms;
    }
    if source.rtsp_retry_max_ms.is_some() {
        target.rtsp_retry_max_ms = source.rtsp_retry_max_ms;
    }
    if source.detector_sample_frames.is_some() {
        target.detector_sample_frames = source.detector_sample_frames;
    }
    if source.detector_stationary_interval_secs.is_some() {
        target.detector_stationary_interval_secs = source.detector_stationary_interval_secs;
    }
    if source.mqtt_host.is_some() {
        target.mqtt_host = source.mqtt_host;
    }
    if source.mqtt_port.is_some() {
        target.mqtt_port = source.mqtt_port;
    }
    if source.mqtt_username.is_some() {
        target.mqtt_username = source.mqtt_username;
    }
    if source.mqtt_password.is_some() {
        target.mqtt_password = source.mqtt_password;
    }
    if source.cameras.is_some() {
        target.cameras = source.cameras;
    }
    if source.service_id.is_some() {
        target.service_id = source.service_id;
    }
    if source.hardware_decoding.is_some() {
        target.hardware_decoding = source.hardware_decoding;
    }
    if source.accelerated_detection.is_some() {
        target.accelerated_detection = source.accelerated_detection;
    }
    if source.fabric_ticket.is_some() {
        target.fabric_ticket = source.fabric_ticket;
    }
    if source.fabric_hub.is_some() {
        target.fabric_hub = source.fabric_hub;
    }
    if source.fabric_allow_frame_offload.is_some() {
        target.fabric_allow_frame_offload = source.fabric_allow_frame_offload;
    }
    if source.fabric_worker_lease_ms.is_some() {
        target.fabric_worker_lease_ms = source.fabric_worker_lease_ms;
    }
    if source.fabric_fallback_horizon_ms.is_some() {
        target.fabric_fallback_horizon_ms = source.fabric_fallback_horizon_ms;
    }
    if source.keyframe_interval_fps_multiplier.is_some() {
        target.keyframe_interval_fps_multiplier = source.keyframe_interval_fps_multiplier;
    }
    if source.keyframe_interval_min_frames.is_some() {
        target.keyframe_interval_min_frames = source.keyframe_interval_min_frames;
    }
    if source.keyframe_interval_max_frames.is_some() {
        target.keyframe_interval_max_frames = source.keyframe_interval_max_frames;
    }
    if source.bitrate_bps_up_to_640x480.is_some() {
        target.bitrate_bps_up_to_640x480 = source.bitrate_bps_up_to_640x480;
    }
    if source.bitrate_bps_up_to_1280x720.is_some() {
        target.bitrate_bps_up_to_1280x720 = source.bitrate_bps_up_to_1280x720;
    }
    if source.bitrate_bps_up_to_1920x1080.is_some() {
        target.bitrate_bps_up_to_1920x1080 = source.bitrate_bps_up_to_1920x1080;
    }
    if source.bitrate_bps_up_to_2560x1440.is_some() {
        target.bitrate_bps_up_to_2560x1440 = source.bitrate_bps_up_to_2560x1440;
    }
    if source.bitrate_bps_above_2560x1440.is_some() {
        target.bitrate_bps_above_2560x1440 = source.bitrate_bps_above_2560x1440;
    }
}

/// One deployment's resolved locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLocation {
    /// The deployment directory.
    pub data_dir: PathBuf,
    /// The store file itself.
    pub store_path: PathBuf,
}

/// What this process's own environment states about where things are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreLocationEnvironment {
    /// `VIGIL_DATA_DIR`.
    pub data_dir: Option<PathBuf>,
    /// `VIGIL_STORE_PATH`.
    pub store_path: Option<PathBuf>,
}

/// The location keys of an add-on options file, and nothing else: a live
/// command has no business failing on a camera list it never came to read.
#[derive(Debug, Clone, Default, Deserialize)]
struct StoreLocationOptions {
    #[serde(default)]
    data_dir: Option<PathBuf>,
    #[serde(default)]
    store_path: Option<PathBuf>,
}

/// Where this deployment's store is, resolved from the LOCATION-ONLY
/// configuration: the surfaces that say where things are, read without
/// loading — or validating — anything else.
///
/// A live command has no command line to carry a store path and no business
/// validating a camera list, but it must arrive at the SAME file the daemon
/// opened or it asks its question of a store nobody is holding. Precedence is
/// the deployment's, unchanged: the environment over the add-on's options, and
/// a store pathname stated anywhere over any filename joined onto a directory.
/// `options_json` is the add-on's options file, or `None` where there is none;
/// a pathname that is not there is the same as none, because an install with
/// no add-on behind it is the ordinary case rather than a fault.
pub fn resolve_store_location(
    environment: &StoreLocationEnvironment,
    options_json: Option<&Path>,
) -> Result<StoreLocation, String> {
    let stated = match options_json {
        Some(path) if path.exists() => read_store_location_options(path)?,
        _ => StoreLocationOptions::default(),
    };
    Ok(store_location_from_stated(
        environment.data_dir.clone().or(stated.data_dir),
        environment.store_path.clone().or(stated.store_path),
    ))
}

/// The one location rule, applied to whatever the deployment's surfaces
/// stated. Used by [`resolve_store_location`] for a live command and by
/// [`load`] for the daemon, off the same merged surfaces the daemon already
/// resolves every other value from — so the two sides cannot land on
/// different files.
///
/// A store pathname stated anywhere wins outright; with none stated, the store
/// sits in this deployment's own directory under the product's filename, which
/// is why the packaged `/data/store.contextgraph` falls OUT of a packaged
/// deployment rather than being written down a second time inside the binary.
fn store_location_from_stated(
    data_dir: Option<PathBuf>,
    store_path: Option<PathBuf>,
) -> StoreLocation {
    let data_dir = data_dir.unwrap_or_else(default_data_dir);
    let store_path = store_path.unwrap_or_else(|| data_dir.join("store.contextgraph"));
    StoreLocation {
        data_dir,
        store_path,
    }
}

/// Read the add-on options file for its location keys alone. Its parse errors
/// are described by LOCATION and KEY exactly as [`read_options_json`]
/// describes them, so a mistyped credential elsewhere in the file can never
/// ride out on a live command's error text.
fn read_store_location_options(path: &Path) -> Result<StoreLocationOptions, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read options {}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| {
        format!(
            "could not parse options {}: {}",
            path.display(),
            describe_json_parse_error(&text, &error)
        )
    })
}

/// This process's locations as its ENVIRONMENT alone states them. No add-on
/// options file is involved, so there is nothing here that can fail — which is
/// what a caller that has no way to report a failure needs.
pub(crate) fn store_location_from_environment(
    environment: &StoreLocationEnvironment,
) -> StoreLocation {
    store_location_from_stated(environment.data_dir.clone(), environment.store_path.clone())
}

/// The same resolution against this process's real environment and the real
/// `/data/options.json`. The ONE answer both [`load`] and live-command
/// dispatch use.
pub(crate) fn configured_store_location() -> Result<StoreLocation, String> {
    resolve_store_location(
        &crate::store_location_environment(),
        Some(&default_options_json_path()),
    )
}

fn default_data_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("vigil-data")
}

fn default_options_json_path() -> PathBuf {
    // Test-only escape hatch so the suite can point this at a fixture path
    // instead of the real Supervisor-mounted file.
    #[cfg(test)]
    #[allow(clippy::disallowed_methods)]
    if let Some(path) = std::env::var_os("VIGIL_TEST_OPTIONS_JSON") {
        return PathBuf::from(path);
    }

    PathBuf::from("/data/options.json")
}

fn run_usage() -> String {
    "Usage: vigil run [--config PATH] [--data-dir PATH] [--store-path PATH] [--health-port PORT] [--review-port PORT] [--site-name NAME] [--camera-name NAME] [--rtsp-url URL] [--live-rtsp-url URL] [--usb-device IDENTITY] [--csi-module IDENTITY] [--mjpeg-url URL] [--rtsp-username USER] [--rtsp-password PASSWORD] [--detector-model-id ID] [--detector-model-path PATH] [--detector-confidence-threshold FLOAT] [--detector-sample-frames N] [--detector-stationary-interval-secs N] [--recognition-weights-dir PATH] [--hardware-decoding BOOL] [--accelerated-detection BOOL] [--fabric-ticket TICKET] [--fabric-hub BOOL] [--fabric-allow-frame-offload BOOL] [--fabric-worker-lease-ms MS] [--fabric-fallback-horizon-ms MS]"
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    use super::{
        CameraEntry, CameraEntryPartial, CameraSourceKind, FieldOutcome,
        SourceKindCapabilityRegistry, UrlRedactionPolicy, classify_field_outcome,
        config_file_fragment_sets, load, redact_url_userinfo, resolve_camera_source_kind,
        run_usage,
    };

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// `redact_url_userinfo` is `pub(crate)`, so the guard is pinned here
    /// directly for `rtsp_url`/`live_rtsp_url` too — branches that do not
    /// reach the real refusal message today (only native RTSP is
    /// supported, so an RTSP-kind camera never hits
    /// `unsupported_source_kind_message`) but carry credentials in the
    /// identical `user:pass@host` shape, so a helper proven only against
    /// today's one reachable case (MJPEG, exercised through the real
    /// refusal message by
    /// `tests/camera_config_schema.rs::a_credentialed_mjpeg_url_redacts_the_embedded_password_but_keeps_the_host_in_the_refusal`)
    /// would be a trap for whoever wires the next adapter-kind refusal.
    /// (Scheme is deliberately RTSP here, never the HTTP scheme this
    /// module's `mjpeg_url` actually uses: the scheme is irrelevant to
    /// this scheme-agnostic redaction logic, and this file's own
    /// source-runtime-fetch guard flags that HTTP scheme literal anywhere
    /// in `crates/vigil/src` — comments and tests included — as a
    /// possible runtime network fetch.)
    #[test]
    fn redact_url_userinfo_strips_the_password_but_keeps_username_host_and_path() {
        assert_eq!(
            redact_url_userinfo(
                "rtsp://vigil:hunter2@camera.local:554/substream",
                UrlRedactionPolicy::Display
            ),
            "rtsp://vigil:<redacted>@camera.local:554/substream",
            "a credentialed camera URL must redact the PASSWORD but keep the username (a \
             diagnostic, not a secret) and the actionable host and path"
        );
        assert_eq!(
            redact_url_userinfo(
                "rtsp://vigil:hunter2@camera.local:554/mainstream",
                UrlRedactionPolicy::Display
            ),
            "rtsp://vigil:<redacted>@camera.local:554/mainstream",
            "live_rtsp_url is the same URL shape as rtsp_url and must redact identically"
        );
    }

    /// `redact_url_userinfo` itself is well covered above, but nothing
    /// proved it is actually WIRED into the hand-written `Debug` impls on
    /// `CameraEntry`/`CameraEntryPartial` — a mistake in either `fmt::Debug`
    /// (e.g. printing the raw field instead of routing it through
    /// `debug_redacted_url`) would pass every other test in this file. This
    /// formats both structs with a credentialed URL that also carries a
    /// `?token=` query, so both the password and the token would show up
    /// in `{:?}` output if the wiring broke.
    #[test]
    fn camera_entry_and_partial_debug_redact_url_credentials_and_token_but_keep_username() {
        let credentialed_rtsp =
            "rtsp://vigil:hunter2@camera.local:554/substream?token=abc123secret".to_string();
        // Scheme is deliberately RTSP here, never the HTTP scheme this
        // module's `mjpeg_url` actually uses: the scheme is irrelevant to
        // this scheme-agnostic redaction logic, and this file's own
        // source-runtime-fetch guard flags that HTTP scheme literal
        // anywhere in `crates/vigil/src` — comments and tests included —
        // as a possible runtime network fetch (see
        // `redact_url_userinfo_strips_the_password_but_keeps_username_host_and_path`
        // above, which follows the identical discipline).
        let credentialed_mjpeg =
            "rtsp://vigil:hunter2@mjpeg-camera.local:8080/stream?token=xyz789secret".to_string();

        let entry = CameraEntry {
            name: "front-door".to_string(),
            rtsp_url: Some(credentialed_rtsp.clone()),
            live_rtsp_url: None,
            username: Some("vigil".to_string()),
            password: None,
            usb_device: None,
            csi_module: None,
            mjpeg_url: Some(credentialed_mjpeg.clone()),
            source_kind: Some(CameraSourceKind::Rtsp),
        };
        let rendered = format!("{entry:?}");
        assert!(
            !rendered.contains("hunter2"),
            "CameraEntry::Debug must never print the RTSP/MJPEG password, got: {rendered}"
        );
        assert!(
            !rendered.contains("abc123secret") && !rendered.contains("xyz789secret"),
            "CameraEntry::Debug must never print a `?token=` query value, got: {rendered}"
        );
        assert!(
            rendered.contains("vigil"),
            "CameraEntry::Debug must keep the username visible (a diagnostic, not a secret), \
             got: {rendered}"
        );

        let partial = CameraEntryPartial {
            name: "front-door".to_string(),
            rtsp_url: Some(credentialed_rtsp),
            live_rtsp_url: None,
            username: Some("vigil".to_string()),
            password: None,
            usb_device: None,
            csi_module: None,
            mjpeg_url: Some(credentialed_mjpeg),
            motion_sensitivity: None,
        };
        let rendered_partial = format!("{partial:?}");
        assert!(
            !rendered_partial.contains("hunter2"),
            "CameraEntryPartial::Debug must never print the RTSP/MJPEG password, got: \
             {rendered_partial}"
        );
        assert!(
            !rendered_partial.contains("abc123secret")
                && !rendered_partial.contains("xyz789secret"),
            "CameraEntryPartial::Debug must never print a `?token=` query value, got: \
             {rendered_partial}"
        );
        assert!(
            rendered_partial.contains("vigil"),
            "CameraEntryPartial::Debug must keep the username visible, got: {rendered_partial}"
        );
    }

    #[test]
    fn redact_url_userinfo_leaves_a_url_with_no_userinfo_unchanged() {
        assert_eq!(
            redact_url_userinfo(
                "rtsp://camera.local:554/substream",
                UrlRedactionPolicy::Display
            ),
            "rtsp://camera.local:554/substream",
            "redaction must not mangle the ordinary, credential-free case"
        );
        assert_eq!(
            redact_url_userinfo("usb-1234:5678-serial-ABC123", UrlRedactionPolicy::Display),
            "usb-1234:5678-serial-ABC123",
            "a non-URL identity (no `://`) must pass through byte-for-byte unchanged"
        );
    }

    #[test]
    fn redact_url_userinfo_leaves_a_username_with_no_password_visible() {
        // Owner ruling: a username is a diagnostic, not a secret — this is
        // the OPPOSITE of the property this test asserted before that
        // ruling (it used to require redaction here too).
        assert_eq!(
            redact_url_userinfo(
                "rtsp://vigil@camera.local:554/substream",
                UrlRedactionPolicy::Display
            ),
            "rtsp://vigil@camera.local:554/substream",
            "an account name with no password is not a secret and must pass through unchanged"
        );
    }

    #[test]
    fn redact_url_userinfo_does_not_mistake_an_at_sign_in_the_path_or_query_for_userinfo() {
        // The query/fragment is now ALWAYS stripped (it can carry its own
        // token/credential, e.g. `?token=...`), so an `@` appearing only
        // there can no longer leak either way: it never survives into the
        // output at all, and — proven here — it is also never mistaken
        // for an authority userinfo boundary while being dropped.
        assert_eq!(
            redact_url_userinfo(
                "rtsp://camera.local:554/substream?contact=owner@example.com",
                UrlRedactionPolicy::Display
            ),
            "rtsp://camera.local:554/substream",
            "the query must be stripped entirely, and an `@` appearing only in it (never in the \
             authority) must not be mistaken for a userinfo boundary that truncates the path too"
        );
        assert_eq!(
            redact_url_userinfo(
                "rtsp://vigil:hunter2@camera.local:554/substream?contact=owner@example.com",
                UrlRedactionPolicy::Display
            ),
            "rtsp://vigil:<redacted>@camera.local:554/substream",
            "a real userinfo redacts the password correctly (keeping the username) AND the \
             query is stripped, even though the query ALSO carries an `@`"
        );
        assert_eq!(
            redact_url_userinfo(
                "rtsp://camera.local:554/substream#fragment@example.com",
                UrlRedactionPolicy::Display
            ),
            "rtsp://camera.local:554/substream",
            "a fragment must be stripped the same way a query is"
        );
    }

    /// A URL with no credentials at all can still leak a token through its
    /// QUERY string — a common shape for camera/streaming endpoints
    /// (`?token=...`). Pinned separately from the userinfo tests above
    /// because this is a genuinely different leak surface: no `user@`
    /// anywhere, only `?token=...`.
    #[test]
    fn redact_url_userinfo_strips_a_query_token_even_with_no_userinfo_present() {
        assert_eq!(
            redact_url_userinfo(
                "rtsp://camera.local:554/stream?token=abc123",
                UrlRedactionPolicy::Display
            ),
            "rtsp://camera.local:554/stream",
            "a query-carried token must be stripped even when there is no userinfo at all"
        );
    }

    /// Table test for [`UrlRedactionPolicy::Persistence`] across the
    /// authority edge shapes: no password, empty userinfo, percent-encoded
    /// userinfo, multiple `@` in the authority, and no userinfo at all.
    /// Every case must strip the ENTIRE userinfo (not just the password —
    /// that is the `Display`-policy behavior pinned above) while scheme,
    /// host, port, and path all survive; a value with no userinfo and no
    /// query/fragment must pass through byte-for-byte unchanged.
    #[test]
    fn redact_url_userinfo_persistence_policy_strips_every_userinfo_shape() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "no password (`user@host`) — Persistence strips the bare \
                 username too, unlike Display",
                "rtsp://vigil@camera.local:554/substream",
                "rtsp://camera.local:554/substream",
            ),
            (
                "empty userinfo (`@host`) — an empty username still forms an \
                 authority-boundary `@` that must be stripped",
                "rtsp://@camera.local:554/substream",
                "rtsp://camera.local:554/substream",
            ),
            (
                "percent-encoded userinfo — encoding must not hide credential \
                 bytes from the strip",
                "rtsp://vigil%40corp:hun%3Ater2@camera.local:554/substream",
                "rtsp://camera.local:554/substream",
            ),
            (
                "multiple `@` in the authority — the boundary is the LAST `@`, \
                 so everything before it (including an embedded `@`) is \
                 userinfo and is stripped",
                "rtsp://vigil:hunter2@extra@camera.local:554/substream",
                "rtsp://camera.local:554/substream",
            ),
            (
                "no userinfo but a query present — the query is always \
                 stripped independent of userinfo",
                "rtsp://camera.local:554/substream?token=abc123",
                "rtsp://camera.local:554/substream",
            ),
            (
                "no userinfo at all — passes through byte-for-byte unchanged",
                "rtsp://camera.local:554/substream",
                "rtsp://camera.local:554/substream",
            ),
        ];

        for (description, input, expected) in cases {
            let redacted = redact_url_userinfo(input, UrlRedactionPolicy::Persistence);
            assert_eq!(
                &redacted, expected,
                "case {description:?}: input {input:?} did not redact to the expected \
                 credential-free value under Persistence"
            );
            assert!(
                redacted.starts_with("rtsp://"),
                "case {description:?}: scheme did not survive, got {redacted:?}"
            );
            assert!(
                redacted.contains("camera.local"),
                "case {description:?}: host did not survive, got {redacted:?}"
            );
            assert!(
                redacted.contains(":554"),
                "case {description:?}: port did not survive, got {redacted:?}"
            );
            assert!(
                redacted.ends_with("/substream"),
                "case {description:?}: path did not survive, got {redacted:?}"
            );
            assert!(
                !redacted.contains('@'),
                "case {description:?}: userinfo boundary `@` leaked into the persisted value, \
                 got {redacted:?}"
            );
            assert!(
                !redacted.contains("vigil") && !redacted.contains("hunter2"),
                "case {description:?}: a credential fragment leaked into the persisted value, \
                 got {redacted:?}"
            );
        }
    }

    /// The `/data/options.json` counterpart of the TOML location-only test
    /// (`camera_config_schema.rs::a_malformed_toml_config_reports_location_only_never_the_offending_source_line`)
    /// — and the more important of the two, since `/data/options.json` is
    /// the DEFAULT production configuration surface for a real Home
    /// Assistant add-on deployment, exactly where an operator's MJPEG
    /// camera password lives. `serde_json::Error`'s own `Display` would
    /// render a type-mismatch error as `invalid type: string "hunter2",
    /// expected a boolean at line 1 column N` — the offending scalar
    /// VERBATIM — so this proves vigil's own wrapping message never uses
    /// it: `hardware_decoding` is given a credential-shaped STRING value
    /// where a boolean is expected, and the resulting error must name the
    /// key and location without ever containing that string.
    #[test]
    fn a_malformed_options_json_reports_key_and_location_never_the_offending_value() {
        let _guard = env_lock().lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let options_path = tmp.path().join("options.json");
        fs::write(
            &options_path,
            r#"{"data_dir":"/tmp/vigil-data","hardware_decoding":"hunter2"}"#,
        )
        .expect("write malformed add-on options json");
        let _options_env = EnvVarGuard::set("VIGIL_TEST_OPTIONS_JSON", &options_path);

        let error = load(Vec::<OsString>::new()).expect_err(
            "a string where a boolean is expected must fail loud, never default silently",
        );

        assert!(
            !error.contains("hunter2"),
            "a malformed options.json value must never appear in the error text — the value \
             might be exactly the credential that made it invalid, got: {error}"
        );
        assert!(
            error.contains("hardware_decoding"),
            "the error must still name the offending KEY (never a secret, unlike the value), \
             got: {error}"
        );
        assert!(
            error.contains("line") && error.contains("column"),
            "the error must still report WHERE the problem is, got: {error}"
        );
    }

    /// `classify_field_outcome` is `pub(crate)`, so its own coverage lives
    /// here (an integration test cannot see a `pub(crate)` item at all) —
    /// the camera-configuration schema contract itself is proven end to
    /// end through the real loader in `tests/camera_config_schema.rs`,
    /// which this pure classifier feeds.
    #[test]
    fn omitted_empty_invalid_and_unavailable_are_four_pairwise_distinct_field_outcomes() {
        assert_eq!(
            classify_field_outcome(None, true, true),
            Some(FieldOutcome::Omitted)
        );
        assert_eq!(
            classify_field_outcome(Some(""), true, true),
            Some(FieldOutcome::Empty)
        );
        assert_eq!(
            classify_field_outcome(Some("not a valid url"), false, true),
            Some(FieldOutcome::Invalid)
        );
        assert_eq!(
            classify_field_outcome(Some("usb-1234:5678-serial-ABC123"), true, false),
            Some(FieldOutcome::Unavailable)
        );
        assert_eq!(
            classify_field_outcome(Some("rtsp://camera.local/stream"), true, true),
            None,
            "a supplied, valid, reachable value is not a failure outcome at all"
        );

        assert_ne!(FieldOutcome::Omitted, FieldOutcome::Empty);
        assert_ne!(FieldOutcome::Empty, FieldOutcome::Invalid);
        assert_ne!(FieldOutcome::Invalid, FieldOutcome::Unavailable);
        assert_ne!(FieldOutcome::Omitted, FieldOutcome::Unavailable);
    }

    fn bare_camera_entry_partial(name: &str) -> CameraEntryPartial {
        CameraEntryPartial {
            name: name.to_string(),
            rtsp_url: None,
            live_rtsp_url: None,
            username: None,
            password: None,
            usb_device: None,
            csi_module: None,
            mjpeg_url: None,
            motion_sensitivity: None,
        }
    }

    /// `resolve_camera_source_kind` and `SourceKindCapabilityRegistry` are
    /// both `pub(crate)`, so this seam's own coverage lives here (an
    /// integration test cannot construct a registry or call the resolver
    /// at all). The registry is passed in as an explicit, immutable
    /// value — never read from any process-global registration — so this
    /// test builds its own registry declaring USB supported (the inverse
    /// of the real artifact's own RTSP-only registration) with no
    /// dependency on module load order, global state, or which other test
    /// ran first. A resolver that still consults the hardcoded
    /// `artifact_supports_source_kind` instead of `registry` fails this:
    /// USB stays refused as `unsupported_by_this_artifact` regardless of
    /// what the registry declares.
    #[test]
    fn resolve_camera_source_kind_accepts_a_kind_the_passed_in_registry_declares_supported() {
        let mut entry = bare_camera_entry_partial("workshop");
        entry.usb_device = Some("usb-1234:5678-serial-ABC123".to_string());

        let registry = SourceKindCapabilityRegistry::new([CameraSourceKind::Usb]);

        let kind = resolve_camera_source_kind(&entry, &registry).unwrap_or_else(|error| {
            panic!(
                "a registry that DOES declare USB supported must let a well-formed usb_device \
                 entry resolve, not refuse it as unsupported_by_this_artifact: {}",
                error.message
            )
        });

        assert_eq!(
            kind,
            CameraSourceKind::Usb,
            "the resolved kind must be the one the registry declared supported"
        );
    }

    /// The inverse proof: a registry that declares Rtsp UNSUPPORTED must
    /// produce the same honest `unsupported_by_this_artifact` refusal
    /// Rtsp is exempt from today — proving the refusal tracks the
    /// registry rather than a hardcoded "Rtsp is always supported"
    /// assumption. A resolver that ignores `registry` and keeps consulting
    /// the hardcoded `artifact_supports_source_kind` fails this: a
    /// well-formed rtsp_url resolves `Ok` regardless of what the registry
    /// declares.
    #[test]
    fn resolve_camera_source_kind_refuses_a_kind_the_passed_in_registry_does_not_declare_supported()
    {
        let mut entry = bare_camera_entry_partial("front gate");
        entry.rtsp_url = Some("rtsp://camera.local/substream".to_string());

        // Declares USB supported but NOT Rtsp — the inverse of today's
        // real artifact registration.
        let registry = SourceKindCapabilityRegistry::new([CameraSourceKind::Usb]);

        let error = resolve_camera_source_kind(&entry, &registry).expect_err(
            "a registry that does NOT declare Rtsp supported must refuse a well-formed \
             rtsp_url entry, even though Rtsp is the real artifact's own default today",
        );

        assert!(
            error.message.contains("unsupported_by_this_artifact"),
            "an unsupported kind must be refused with the standard honest-capability-refusal \
             vocabulary, got: {}",
            error.message
        );
        assert!(
            error.message.contains("rtsp"),
            "the refusal must name the refused kind, got: {}",
            error.message
        );
    }

    #[test]
    fn review_port_cli_override_is_documented_and_loaded() {
        let _guard = env_lock().lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let _options_env = EnvVarGuard::set(
            "VIGIL_TEST_OPTIONS_JSON",
            tmp.path().join("absent-options.json"),
        );
        let _clean_env = [
            "VIGIL_DATA_DIR",
            "VIGIL_STORE_PATH",
            "VIGIL_HEALTH_PORT",
            "VIGIL_REVIEW_PORT",
            "VIGIL_SITE_NAME",
            "VIGIL_CAMERA_NAME",
            "VIGIL_RTSP_URL",
            "VIGIL_LIVE_RTSP_URL",
            "VIGIL_RTSP_USERNAME",
            "VIGIL_RTSP_PASSWORD",
            "VIGIL_DETECTOR_MODEL_ID",
            "VIGIL_DETECTOR_MODEL_PATH",
            "VIGIL_RECOGNITION_WEIGHTS_DIR",
            "VIGIL_RECOGNITION_SPACE_ID",
            "VIGIL_RECOGNITION_THRESHOLD",
            "VIGIL_DETECTOR_CONFIDENCE_THRESHOLD",
            "VIGIL_DETECTOR_SAMPLE_FRAMES",
            "VIGIL_DETECTOR_STATIONARY_INTERVAL_SECS",
            "VIGIL_SERVICE_ID",
            "VIGIL_HARDWARE_DECODING",
            "VIGIL_ACCELERATED_DETECTION",
            "VIGIL_FABRIC_TICKET",
            "VIGIL_FABRIC_HUB",
            "VIGIL_FABRIC_ALLOW_FRAME_OFFLOAD",
            "VIGIL_FABRIC_WORKER_LEASE_MS",
            "VIGIL_FABRIC_FALLBACK_HORIZON_MS",
            "MQTT_HOST",
            "MQTT_PORT",
            "MQTT_USER",
            "MQTT_USERNAME",
            "MQTT_PASSWORD",
        ]
        .map(EnvVarGuard::remove);
        let usage = run_usage();
        let cli_doc = include_str!("../../../docs/cli.md");
        let table_start = cli_doc
            .find("Common options:")
            .expect("CLI doc has the Common options heading");
        let table_end = cli_doc
            .find("<!-- vigil-claim: `vigil.docs-cli.run-options-and-current-defaults` -->")
            .expect("CLI doc has the run-options contract marker");
        let run_options_table = &cli_doc[table_start..table_end];
        for line in run_options_table
            .lines()
            .filter(|line| line.starts_with("| `--"))
        {
            let documented = line
                .split('`')
                .nth(1)
                .expect("CLI table row has a backtick-delimited option");
            assert!(
                usage.contains(documented),
                "documented run option {documented} is missing from the real usage surface"
            );
        }
        let config = load(vec![
            OsString::from("--review-port"),
            OsString::from("8765"),
            OsString::from("--detector-stationary-interval-secs"),
            OsString::from("30"),
        ])
        .expect("documented CLI overrides load");
        assert_eq!(config.review_port, 8765);
        assert_eq!(config.detector_stationary_interval_secs, 30);
        let defaults = load(Vec::<OsString>::new()).expect("load documented default run config");
        assert_eq!(
            defaults.data_dir.file_name().and_then(|name| name.to_str()),
            Some("vigil-data")
        );
        assert_eq!(
            defaults.store_path,
            defaults.data_dir.join("store.contextgraph")
        );
        assert!(defaults.rtsp_url.is_none());
        assert!(defaults.rtsp_username.is_none());
        assert!(defaults.rtsp_password.is_none());
        assert!(defaults.detector_model_path.is_none());
        assert!(!defaults.recognition.enabled);
        assert!(defaults.fabric_ticket.is_none());

        let detection_url = load(vec![
            OsString::from("--rtsp-url"),
            OsString::from("rtsp://camera.example/detection"),
        ])
        .expect("detection URL without a separate live URL loads");
        assert!(
            detection_url.cameras[0].live_rtsp_url.is_none(),
            "config must preserve an omitted live URL so runtime can apply the documented detection-URL fallback"
        );

        for documented_default in [
            "| `--config PATH` | Read a TOML configuration file | none |".to_string(),
            "| `--data-dir PATH` | Runtime data root | `./vigil-data` |".to_string(),
            "| `--store-path PATH` | Context Graph store | `<data-dir>/store.contextgraph` |".to_string(),
            format!("| `--health-port PORT` | Health HTTP port | `{}` |", defaults.health_port),
            format!("| `--review-port PORT` | Review HTTP port | `{}` |", defaults.review_port),
            format!("| `--site-name NAME` | Site/context name | `{}` |", defaults.site_name),
            format!("| `--camera-name NAME` | Single-camera name | `{}` |", defaults.camera_name),
            "| `--rtsp-url URL` | Detection stream | none |".to_string(),
            "| `--live-rtsp-url URL` | Separate Home Assistant live stream | detection URL |".to_string(),
            "| `--rtsp-username USER` | RTSP username outside the URL | none |".to_string(),
            "| `--rtsp-password PASSWORD` | RTSP password outside the URL | none |".to_string(),
            format!("| `--detector-model-id ID` | Model identity written to provenance | `{}` |", defaults.detector_model_id),
            "| `--detector-model-path PATH` | Detector weights path | artifact/config dependent |".to_string(),
            format!("| `--detector-confidence-threshold FLOAT` | Keep detections at or above this value | `{}` |", defaults.detector_confidence_threshold),
            format!("| `--detector-sample-frames N` | Frames sampled per segment | `{}` |", defaults.detector_sample_frames),
            format!("| `--detector-stationary-interval-secs N` | Sampling interval for stationary scenes | `{}` |", defaults.detector_stationary_interval_secs),
            "| `--recognition-weights-dir PATH` | Enable recognition with local weights | disabled |".to_string(),
            format!("| `--hardware-decoding BOOL` | Request hardware-decode probing | `{}` |", defaults.hardware_decoding),
            format!("| `--accelerated-detection BOOL` | Request accelerated-detector probing | `{}` |", defaults.accelerated_detection),
            "| `--fabric-ticket TICKET` | Join an existing configured fabric | none |".to_string(),
            format!("| `--fabric-hub BOOL` | Start the node as a fabric join point | `{}` |", defaults.fabric_hub),
            format!("| `--fabric-allow-frame-offload BOOL` | Permit this node's detector work to move | `{}` |", defaults.fabric_allow_frame_offload),
            format!("| `--fabric-worker-lease-ms MS` | Fabric worker lease setting | `{}` |", defaults.fabric_worker_lease_ms),
            format!("| `--fabric-fallback-horizon-ms MS` | Remote-result wait setting | `{}` |", defaults.fabric_fallback_horizon_ms),
        ] {
            assert!(
                run_options_table.contains(&documented_default),
                "CLI defaults table diverged from runtime config: {documented_default}"
            );
        }

        for (args, expected_error) in [
            (
                vec!["--detector-confidence-threshold", "-0.1"],
                "detector_confidence_threshold must be between 0.0 and 1.0",
            ),
            (
                vec!["--detector-sample-frames", "0"],
                "detector_sample_frames must be between 1 and 64",
            ),
            (
                vec!["--hardware-decoding", "sometimes"],
                "--hardware-decoding must be true or false",
            ),
        ] {
            let error = load(args.into_iter().map(OsString::from).collect())
                .expect_err("documented invalid CLI value must fail");
            assert!(
                error.contains(expected_error),
                "invalid CLI value must name its contract: expected {expected_error:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn live_rtsp_url_is_distinct_from_detection_rtsp_url() {
        // `load()` reads process environment variables (VIGIL_TEST_OPTIONS_JSON
        // among them), and `cargo test`'s default parallelism runs every test
        // in this binary as threads sharing one process — without this lock,
        // this test can observe VIGIL_TEST_OPTIONS_JSON left pointing at
        // another concurrently running test's fixture (or vice versa) and
        // fail intermittently. Every other `load()`-calling test in this
        // module already holds this same lock; this one didn't.
        let _guard = env_lock().lock().expect("env lock");
        assert!(run_usage().contains("--live-rtsp-url URL"));
        let config = load(vec![
            OsString::from("--rtsp-url"),
            OsString::from("rtsp://camera/detect"),
            OsString::from("--live-rtsp-url"),
            OsString::from("rtsp://camera/live"),
        ])
        .expect("live RTSP CLI override loads");

        assert_eq!(config.rtsp_url.as_deref(), Some("rtsp://camera/detect"));
        assert_eq!(
            config.cameras[0].rtsp_url.as_deref(),
            Some("rtsp://camera/detect")
        );
        assert_eq!(
            config.cameras[0].live_rtsp_url.as_deref(),
            Some("rtsp://camera/live")
        );
    }

    /// The legacy single-camera `--usb-device`/`--csi-module`/`--mjpeg-url`
    /// CLI flags, added alongside `--rtsp-url`/`--live-rtsp-url`, are still
    /// discoverable (an operator can find them in `run_usage()`), but under
    /// the route-parity ruling — the same source value must get the same
    /// accept-or-refuse answer whichever configuration route it arrives by
    /// — loading with one of them now REFUSES, naming the same kind and the
    /// same `unsupported_by_this_artifact` reason the `[[cameras]]` route
    /// already gives (see `every_adapter_source_kind_is_honestly_rejected_by_the_real_load_as_unsupported_by_this_artifact`
    /// in `tests/camera_config_schema.rs`). This test used to assert the
    /// opposite (the CLI-only route silently accepting a kind the table
    /// route refused): that inconsistency is the defect the ruling closes,
    /// so the old assertion is inverted here rather than deleted. The
    /// config-file-vs-CLI precedence proof survives too, now expressed
    /// through which value the refusal names: the CLI value still reaches
    /// (and is refused at) the legacy entry over the config-file value.
    #[test]
    fn usb_csi_mjpeg_legacy_cli_flags_are_advertised_but_refused_like_the_cameras_list_route() {
        let _guard = env_lock().lock().expect("env lock");
        assert!(run_usage().contains("--usb-device IDENTITY"));
        assert!(run_usage().contains("--csi-module IDENTITY"));
        assert!(run_usage().contains("--mjpeg-url URL"));

        let usb_error = load(vec![
            OsString::from("--usb-device"),
            OsString::from("vendor:1234:product:5678:serial:ABC"),
        ])
        .expect_err(
            "the legacy --usb-device CLI flag must be refused exactly like a [[cameras]] \
             usb_device entry is",
        );
        assert!(
            usb_error.contains("unsupported_by_this_artifact"),
            "got: {usb_error}"
        );
        assert!(usb_error.contains("usb"), "got: {usb_error}");

        let csi_error = load(vec![
            OsString::from("--csi-module"),
            OsString::from("imx219-cam0"),
        ])
        .expect_err(
            "the legacy --csi-module CLI flag must be refused exactly like a [[cameras]] \
             csi_module entry is",
        );
        assert!(
            csi_error.contains("unsupported_by_this_artifact"),
            "got: {csi_error}"
        );
        assert!(csi_error.contains("csi"), "got: {csi_error}");

        // The URLs below use the example.com host, never any other host:
        // this file's own outbound-fetch scan flags an HTTP-scheme literal
        // anywhere under `crates/vigil/src` (comments and tests included),
        // and allows only that one host — the same discipline already
        // documented at
        // `redact_url_userinfo_strips_the_password_but_keeps_username_host_and_path`.
        let mjpeg_error = load(vec![
            OsString::from("--mjpeg-url"),
            OsString::from("http://example.com/esp32cam-stream"),
        ])
        .expect_err(
            "the legacy --mjpeg-url CLI flag must be refused exactly like a [[cameras]] \
             mjpeg_url entry is",
        );
        assert!(
            mjpeg_error.contains("unsupported_by_this_artifact"),
            "got: {mjpeg_error}"
        );
        assert!(mjpeg_error.contains("mjpeg"), "got: {mjpeg_error}");

        let tmp = tempfile::tempdir().expect("tempdir");
        let config_path = tmp.path().join("vigil.toml");
        fs::write(
            &config_path,
            "mjpeg_url = \"http://example.com/from-file-stream\"\n",
        )
        .expect("write TOML config file");
        let overridden_error = load(vec![
            OsString::from("--config"),
            OsString::from(config_path),
            OsString::from("--mjpeg-url"),
            OsString::from("http://example.com/from-cli-stream"),
        ])
        .expect_err(
            "the CLI flag still reaches the legacy single-camera entry over the config-file \
             value, and that value is refused exactly like every other legacy mjpeg_url value",
        );
        assert!(
            overridden_error.contains("from-cli-stream"),
            "the refusal must name the CLI value, proving the CLI flag still wins over the \
             config-file value even though both are now refused, got: {overridden_error}"
        );
        assert!(
            !overridden_error.contains("from-file-stream"),
            "the refusal must not name the superseded config-file value, got: {overridden_error}"
        );
    }

    #[test]
    fn addon_options_json_recognition_fields_enable_runtime_config_and_startup_line() {
        let _guard = env_lock().lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().join("data");
        let weights_dir = tmp.path().join("recognition").join("siglip");
        fs::create_dir_all(&weights_dir).expect("create weights dir");
        let options_path = tmp.path().join("options.json");
        fs::write(
            &options_path,
            serde_json::json!({
                "data_dir": data_dir,
                "store_path": tmp.path().join("store.contextgraph"),
                "recognition_weights_dir": weights_dir,
                "recognition_space_id": "vigil_site_vision_smoke",
                "recognition_threshold": 0.73,
                "detector_stationary_interval_secs": 30,
                "recognition_covered_classes": ["person", "dog"]
            })
            .to_string(),
        )
        .expect("write add-on options json");

        let _options_env = EnvVarGuard::set("VIGIL_TEST_OPTIONS_JSON", &options_path);
        let _weights_env = EnvVarGuard::remove("VIGIL_RECOGNITION_WEIGHTS_DIR");
        let _space_env = EnvVarGuard::remove("VIGIL_RECOGNITION_SPACE_ID");
        let _threshold_env = EnvVarGuard::remove("VIGIL_RECOGNITION_THRESHOLD");
        let _stationary_env = EnvVarGuard::remove("VIGIL_DETECTOR_STATIONARY_INTERVAL_SECS");

        let config = load(Vec::<OsString>::new()).expect("load add-on options json");

        assert_eq!(
            crate::recognition::RecognitionConfig::default().match_threshold,
            0.90,
            "recognition's visible default must be the actual matching threshold"
        );

        assert!(
            config.recognition.enabled,
            "recognition_weights_dir in add-on options must enable local recognition"
        );
        assert_eq!(
            config.recognition.weights_dir.as_deref(),
            Some(weights_dir.as_path())
        );
        assert_eq!(
            config.recognition.embedding_space_id,
            "vigil_site_vision_smoke"
        );
        assert_eq!(config.recognition.match_threshold, 0.73);
        assert_eq!(
            config.detector_stationary_interval_secs, 30,
            "detector_stationary_interval_secs in add-on options must allow periodic detector scans on no-motion segments so a visible stationary person is not skipped before recognition"
        );
        assert_eq!(
            config.recognition.covered_classes,
            vec!["person".to_string(), "dog".to_string()],
            "recognition_covered_classes in add-on options must control the detector/recognizer class allowlist from HAOS, not require a Rust change"
        );
        assert_eq!(
            crate::runtime::recognition_enabled_startup_line(&config.recognition),
            "recognition_enabled=true space=vigil_site_vision_smoke threshold=0.73 accuracy_warning=below_recommended_default_0.9",
            "the runtime startup line must expose that local add-on recognition is active"
        );

        fs::write(
            &options_path,
            serde_json::json!({
                "data_dir": data_dir,
                "recognition_threshold": 1.01
            })
            .to_string(),
        )
        .expect("write invalid recognition threshold");
        let error = load(Vec::<OsString>::new()).expect_err("out-of-range threshold must fail");
        assert!(
            error.contains("recognition_threshold must be between 0.0 and 1.0"),
            "invalid recognition threshold must fail with its field and accepted range, got {error}"
        );

        fs::write(
            &options_path,
            serde_json::json!({ "data_dir": data_dir }).to_string(),
        )
        .expect("write config with no stationary override");
        let defaults = load(Vec::<OsString>::new()).expect("load shared deployment defaults");
        assert_eq!(
            defaults.detector_stationary_interval_secs, 30,
            "standalone and add-on configuration must share the 30-second look-anyway default"
        );
    }

    /// Proves the four owner-ratified video-encoding levers (multiplier,
    /// min/max keyframe-interval frames, and the five per-resolution-class
    /// bitrates) are real declared settings: their automatic defaults
    /// match `crate::encode`'s single-sourced `*_AUTOMATIC_DEFAULT`
    /// constants (never an independently retyped literal), their
    /// `config_key`/`addon_option_key` are exactly their own field name
    /// (matching `detector_stationary_interval_secs`'s own precedent),
    /// and an operator-supplied add-on-options value actually resolves as
    /// the effective value through the real settings registry, not just
    /// the raw parsed field.
    #[test]
    fn keyframe_and_bitrate_settings_declare_with_ratified_defaults_and_real_surfaces() {
        let _guard = env_lock().lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let _options_env = EnvVarGuard::set(
            "VIGIL_TEST_OPTIONS_JSON",
            tmp.path().join("absent-options.json"),
        );

        let entries = crate::declared_settings();
        for (name, expected_default) in [
            (
                "keyframe_interval_fps_multiplier",
                u64::from(crate::encode::KEYFRAME_INTERVAL_FPS_MULTIPLIER_AUTOMATIC_DEFAULT),
            ),
            (
                "keyframe_interval_min_frames",
                u64::from(crate::encode::KEYFRAME_INTERVAL_MIN_FRAMES_AUTOMATIC_DEFAULT),
            ),
            (
                "keyframe_interval_max_frames",
                u64::from(crate::encode::KEYFRAME_INTERVAL_MAX_FRAMES_AUTOMATIC_DEFAULT),
            ),
            (
                "bitrate_bps_up_to_640x480",
                u64::from(crate::encode::BITRATE_BPS_UP_TO_640X480_AUTOMATIC_DEFAULT),
            ),
            (
                "bitrate_bps_up_to_1280x720",
                u64::from(crate::encode::BITRATE_BPS_UP_TO_1280X720_AUTOMATIC_DEFAULT),
            ),
            (
                "bitrate_bps_up_to_1920x1080",
                u64::from(crate::encode::BITRATE_BPS_UP_TO_1920X1080_AUTOMATIC_DEFAULT),
            ),
            (
                "bitrate_bps_up_to_2560x1440",
                u64::from(crate::encode::BITRATE_BPS_UP_TO_2560X1440_AUTOMATIC_DEFAULT),
            ),
            (
                "bitrate_bps_above_2560x1440",
                u64::from(crate::encode::BITRATE_BPS_ABOVE_2560X1440_AUTOMATIC_DEFAULT),
            ),
        ] {
            let entry = entries
                .iter()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("{name} must be a declared setting"));
            assert_eq!(entry.surfaces.config_key, name);
            assert_eq!(entry.surfaces.addon_option_key, name);
            assert_eq!(entry.surfaces.documentation_page, "docs/configuration.md");
            assert!(
                config_file_fragment_sets(name, &expected_default.to_string()),
                "{name} must round-trip through the real config-file fragment parser"
            );
        }

        let defaults = load(Vec::<OsString>::new()).expect("load documented default run config");
        assert_eq!(defaults.keyframe_interval_fps_multiplier, 2);
        assert_eq!(defaults.keyframe_interval_min_frames, 15);
        assert_eq!(defaults.keyframe_interval_max_frames, 300);
        assert_eq!(defaults.bitrate_bps_up_to_640x480, 1_000_000);
        assert_eq!(defaults.bitrate_bps_up_to_1280x720, 2_000_000);
        assert_eq!(defaults.bitrate_bps_up_to_1920x1080, 4_000_000);
        assert_eq!(defaults.bitrate_bps_up_to_2560x1440, 6_000_000);
        assert_eq!(defaults.bitrate_bps_above_2560x1440, 10_000_000);

        let options_path = tmp.path().join("options.json");
        fs::write(
            &options_path,
            serde_json::json!({
                "data_dir": tmp.path().join("data"),
                "keyframe_interval_fps_multiplier": 4,
                "bitrate_bps_up_to_1920x1080": 5_000_000,
            })
            .to_string(),
        )
        .expect("write add-on options json");
        let _options_env = EnvVarGuard::set("VIGIL_TEST_OPTIONS_JSON", &options_path);
        let pinned = load(Vec::<OsString>::new()).expect("load add-on options json override");
        assert_eq!(
            pinned.keyframe_interval_fps_multiplier, 4,
            "an operator-supplied add-on option must resolve as the effective value"
        );
        assert_eq!(pinned.bitrate_bps_up_to_1920x1080, 5_000_000);
        assert_eq!(
            pinned.keyframe_interval_min_frames, 15,
            "an untouched sibling setting must keep its own automatic default"
        );
    }

    /// `keyframe_interval_min_frames` and `keyframe_interval_max_frames`
    /// are each validated independently against their OWN range (1..=1800
    /// and 1..=3600 respectively) — nothing today checks the pair against
    /// each other, so a min above the max is accepted at load and only
    /// blows up later at `f64::clamp(min, max)`
    /// (`crate::encode::automatic_keyframe_interval_frames`), which PANICS
    /// when `min > max` rather than returning an error. A configuration
    /// mistake must never become a runtime panic: inverted bounds must be
    /// rejected right here, at the real load, naming both values, so the
    /// panic is structurally unreachable — never merely avoided by luck at
    /// whatever frame rate happens to be configured.
    #[test]
    fn an_inverted_keyframe_interval_min_above_max_is_rejected_at_the_real_load_naming_both_values()
    {
        let _guard = env_lock().lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let _options_env = EnvVarGuard::set(
            "VIGIL_TEST_OPTIONS_JSON",
            tmp.path().join("absent-options.json"),
        );

        // Both bounds are individually well within their own documented
        // ranges (min: 1..=1800, max: 1..=3600) — only the PAIR is
        // nonsensical, so this can only be caught by a check that compares
        // them against each other, never by either one's own independent
        // range validation.
        let config_path = tmp.path().join("vigil.toml");
        fs::write(
            &config_path,
            "keyframe_interval_min_frames = 500\nkeyframe_interval_max_frames = 100\n\
             rtsp_url = \"rtsp://camera.local/substream\"\n",
        )
        .expect("write TOML config file");

        let error = load(vec![
            OsString::from("--config"),
            OsString::from(config_path),
        ])
        .expect_err(
            "keyframe_interval_min_frames (500) exceeding keyframe_interval_max_frames \
                 (100) must be rejected at the real load — accepting it defers a real \
                 configuration mistake to a later panic in the encoder's clamp",
        );

        assert!(
            error.contains("500"),
            "the load rejection must name the configured min value, got: {error}"
        );
        assert!(
            error.contains("100"),
            "the load rejection must name the configured max value, got: {error}"
        );
        assert!(
            error.contains("keyframe_interval_min_frames")
                && error.contains("keyframe_interval_max_frames"),
            "the load rejection must name both settings, not just report a bare number \
             mismatch, got: {error}"
        );
    }

    /// The anti-cheat companion to the inversion test above: a configured
    /// min EQUAL to the configured max is the boundary case a check that
    /// merely tests `min > max` (rather than an off-by-one `min >= max`)
    /// must still accept — `f64::clamp(min, max)` with `min == max` never
    /// panics, it simply always clamps to that single value, so this is
    /// real, legal configuration, not a second inversion case.
    #[test]
    fn a_keyframe_interval_min_equal_to_max_loads_successfully() {
        let _guard = env_lock().lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let _options_env = EnvVarGuard::set(
            "VIGIL_TEST_OPTIONS_JSON",
            tmp.path().join("absent-options.json"),
        );

        let config_path = tmp.path().join("vigil.toml");
        fs::write(
            &config_path,
            "keyframe_interval_min_frames = 200\nkeyframe_interval_max_frames = 200\n\
             rtsp_url = \"rtsp://camera.local/substream\"\n",
        )
        .expect("write TOML config file");

        let loaded = load(vec![
            OsString::from("--config"),
            OsString::from(config_path),
        ])
        .expect("min == max is a legal, non-inverted bound and must load successfully");

        assert_eq!(loaded.keyframe_interval_min_frames, 200);
        assert_eq!(loaded.keyframe_interval_max_frames, 200);
    }

    /// The RTSP password can arrive from a CLI flag, a TOML config file, or
    /// an environment variable — this must keep resolving to the identical
    /// effective value it did before the password field was wrapped in
    /// `Secret`, including the existing env-overrides-CLI-overrides-file
    /// precedence.
    #[test]
    fn rtsp_password_resolves_identically_from_cli_file_and_env() {
        let _guard = env_lock().lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let _options_env = EnvVarGuard::set(
            "VIGIL_TEST_OPTIONS_JSON",
            tmp.path().join("absent-options.json"),
        );
        let _password_env = EnvVarGuard::remove("VIGIL_RTSP_PASSWORD");

        // CLI flag alone.
        let cli_only = load(vec![
            OsString::from("--rtsp-password"),
            OsString::from("secret-from-cli"),
        ])
        .expect("CLI-supplied password loads");
        assert_eq!(
            cli_only
                .rtsp_password
                .as_ref()
                .map(|password| password.expose_secret()),
            Some("secret-from-cli")
        );

        // TOML config file alone.
        let config_path = tmp.path().join("vigil.toml");
        fs::write(&config_path, "rtsp_password = \"secret-from-file\"\n")
            .expect("write TOML config file");
        let file_only = load(vec![
            OsString::from("--config"),
            OsString::from(config_path.clone()),
        ])
        .expect("file-supplied password loads");
        assert_eq!(
            file_only
                .rtsp_password
                .as_ref()
                .map(|password| password.expose_secret()),
            Some("secret-from-file")
        );

        // Environment variable alone.
        {
            let _password_env = EnvVarGuard::set("VIGIL_RTSP_PASSWORD", "secret-from-env");
            let env_only = load(Vec::<OsString>::new()).expect("env-supplied password loads");
            assert_eq!(
                env_only
                    .rtsp_password
                    .as_ref()
                    .map(|password| password.expose_secret()),
                Some("secret-from-env")
            );

            // Env still wins over both file and CLI, exactly as before Secret adoption.
            let env_over_file_and_cli = load(vec![
                OsString::from("--config"),
                OsString::from(config_path),
                OsString::from("--rtsp-password"),
                OsString::from("secret-from-cli"),
            ])
            .expect("all three sources supplied together still load");
            assert_eq!(
                env_over_file_and_cli
                    .rtsp_password
                    .as_ref()
                    .map(|password| password.expose_secret()),
                Some("secret-from-env"),
                "environment must still win over both a config file and a CLI flag"
            );
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        // Test-only fence that reads a variable's prior value so it can be
        // restored on drop; not a product read of an adjustable value.
        #[allow(clippy::disallowed_methods)]
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let guard = Self {
                key,
                previous: std::env::var_os(key),
            };
            unsafe {
                std::env::set_var(key, value);
            }
            guard
        }

        // Test-only fence that reads a variable's prior value so it can be
        // restored on drop; not a product read of an adjustable value.
        #[allow(clippy::disallowed_methods)]
        fn remove(key: &'static str) -> Self {
            let guard = Self {
                key,
                previous: std::env::var_os(key),
            };
            unsafe {
                std::env::remove_var(key);
            }
            guard
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            restore_env(self.key, self.previous.take());
        }
    }

    fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
