//! The canonical encoded-camera-track types shared by every source
//! producer (native RTSP, USB/UVC, CSI-2, HTTP/MJPEG) and every consumer
//! (analysis decode, recording, RTSP publishing, and later MoQ).
//!
//! [`EncodedAccessUnit`] is the one semantic unit every producer emits and
//! every consumer reads; `crate::decode` re-exports it so existing
//! `crate::decode::EncodedAccessUnit` call sites keep compiling unchanged.
//! [`CameraTrackHub`] (`crate::camera_hub`) is the bounded, per-consumer
//! fan-out over this type — no per-consumer deep copy of the encoded
//! payload, no unbounded queue, and an explicit per-subscriber loss
//! contract instead of one uniform broadcast policy.
//!
//! `data` and `codec_config` are [`bytes::Bytes`]: constructing one from a
//! freshly captured/decoded `Vec<u8>` is zero-copy (`Bytes::from(Vec<u8>)`
//! reuses the vector's own allocation), and every subsequent `clone()` —
//! handing the same unit to N hub subscribers, or cloning the whole struct
//! — shares that one allocation via a cheap refcounted handle rather than
//! deep-copying the bytes. `Arc<[u8]>` cannot offer the first half of that:
//! converting a `Vec<u8>` into `Arc<[u8]>` always allocates and copies, so
//! it would reintroduce a per-frame copy on the capture hot path that
//! `Bytes` avoids entirely.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::media_pipeline::VideoCodec;
use crate::workgraph::StreamId;

/// Durable per-camera identity: owning node plus durable hardware/config
/// identity (USB vendor/product/serial, CSI module identity, MJPEG
/// credential-stripped endpoint URL, or the RTSP URL identity for native
/// cameras). Never the operator's display name (`name` in `[[cameras]]` is
/// display-only: renaming a camera never changes this value).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CameraId(String);

/// Why a value could not become a durable [`CameraId`]. Every per-source-kind
/// constructor is fallible for exactly this reason: an input that cannot
/// yield a durable identity is refused, never silently accepted as a bad
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraIdError {
    /// The owning-node identity was empty (after trimming).
    EmptyNode,
    /// A USB/CSI value was empty, or looked like a transient `/dev/...`
    /// device path rather than a durable hardware identity.
    NotDurable { field: &'static str },
    /// An RTSP/MJPEG value did not parse as an absolute URL with one of the
    /// field's allowed schemes, or carried no host.
    InvalidUrl { field: &'static str },
}

impl std::fmt::Display for CameraIdError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CameraIdError::EmptyNode => {
                write!(
                    formatter,
                    "camera identity requires a non-empty owning-node identity"
                )
            }
            CameraIdError::NotDurable { field } => write!(
                formatter,
                "{field} is not a durable hardware identity (a transient /dev/ device path, or an empty value, cannot become a camera identity)"
            ),
            CameraIdError::InvalidUrl { field } => write!(
                formatter,
                "{field} did not parse as a valid URL with an allowed scheme"
            ),
        }
    }
}

impl std::error::Error for CameraIdError {}

/// Per-node secret key for the RTSP/MJPEG query digest folded into
/// [`CameraId`] (see [`CameraId::from_endpoint_url`]). Without a key, a
/// 16-hex-character digest of a query is an unkeyed fingerprint: 64 bits
/// derived from the raw query text, so an attacker who *guesses* the
/// query (e.g. a query-carried auth token) can confirm the guess offline
/// by recomputing the identical digest — weaker than the credential
/// stripping the rest of this type already does for userinfo. Keying the
/// digest with a value that never leaves this node closes that
/// confirmation path: reproducing the digest now requires reading this
/// salt off disk, which already requires the box compromise that makes
/// the config file holding the raw URL readable anyway.
///
/// **This salt is node state, not camera identity.** It lives beside the
/// node's other durable state (see [`CameraQuerySalt::load_or_generate`])
/// and its bytes never enter a `CameraId`, a receipt, or a sync payload —
/// only the keyed digest it produces ever leaves this type.
///
/// **Documented consequence: wiping the salt file re-keys every
/// query-distinguished camera identity on this node.** An RTSP/MJPEG
/// camera whose endpoint carries a query string gets a new `CameraId` the
/// next time it is resolved after the salt is regenerated (a camera with
/// no query, or a USB/CSI camera, is unaffected — neither depends on this
/// salt at all). This is greenfield behavior: there is no migration path
/// for a wiped salt, and none is promised. An operator who wipes node
/// state (or restores it from a backup taken before the salt existed)
/// accepts that any query-distinguished cameras re-enroll under new
/// identities.
pub struct CameraQuerySalt([u8; 32]);

impl CameraQuerySalt {
    /// The file this salt is persisted to, relative to a node's data
    /// root: a sibling of the node's other durable state (e.g. the fabric
    /// identity at `<data_dir>/fabric-identity.key`).
    const FILE_NAME: &'static str = "camera-query-salt";

    /// Load the salt persisted at `<data_dir>/camera-query-salt`, or
    /// generate one and persist it there (owner-only file permissions on
    /// unix). The same `data_dir` always yields the same salt across
    /// restarts, so a camera's query-digested identity is stable
    /// process-to-process as long as the data root survives.
    pub fn load_or_generate(data_dir: &Path) -> io::Result<Self> {
        let path = data_dir.join(Self::FILE_NAME);
        if path.exists() {
            return Self::load(&path);
        }
        let salt = Self::generate();
        salt.persist(&path)?;
        Ok(salt)
    }

    fn load(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        let salt: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "camera query salt at {path:?} is corrupt: expected 32 bytes, found {}",
                    bytes.len()
                ),
            )
        })?;
        Ok(Self(salt))
    }

    fn persist(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(&self.0)
    }

    /// Generate a fresh salt without persisting it. Production identity
    /// construction uses [`CameraQuerySalt::load_or_generate`] so the
    /// salt survives a restart; this ephemeral form is for callers that
    /// only need construction to succeed within a single process and
    /// never compare the resulting digest across process runs — e.g.
    /// config-load-time validation (`crate::config`) and tests.
    pub fn generate() -> Self {
        let mut salt = [0u8; 32];
        getrandom::fill(&mut salt).expect("operating system randomness must be available");
        Self(salt)
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl CameraId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Durable identity for a USB/UVC camera: the owning node plus the
    /// device's durable vendor/product/serial hardware identity — never a
    /// transient `/dev/videoN`-style path. Canonicalizing/fallible: see the
    /// module-level RED test file for the exact incidental-vs-significant
    /// rules this is specified against.
    pub fn from_usb(node_id: &str, usb_device: &str) -> Result<Self, CameraIdError> {
        Self::from_durable_hardware_value(node_id, usb_device, "usb_device")
    }

    /// Durable identity for a MIPI CSI-2 camera: the owning node plus the
    /// module's durable identity — never a transient `/dev/...`-style path.
    pub fn from_csi(node_id: &str, csi_module: &str) -> Result<Self, CameraIdError> {
        Self::from_durable_hardware_value(node_id, csi_module, "csi_module")
    }

    /// Shared USB/CSI construction: trim incidental surrounding whitespace,
    /// preserve case (a hardware serial can be genuinely case-sensitive,
    /// unlike a DNS host name), refuse an empty value, and refuse a
    /// transient `/dev/...` path.
    fn from_durable_hardware_value(
        node_id: &str,
        raw_value: &str,
        field: &'static str,
    ) -> Result<Self, CameraIdError> {
        let node_id = Self::require_non_empty_node(node_id)?;
        let value = raw_value.trim();
        if value.is_empty() || value.starts_with("/dev/") {
            return Err(CameraIdError::NotDurable { field });
        }
        Ok(Self(format!("{node_id}:{field}:{value}")))
    }

    /// Durable identity for a native RTSP camera: the owning node plus the
    /// endpoint's canonical `scheme://host:port/path?q=<digest>` — userinfo
    /// (credentials) never enters the identity, path is kept as
    /// significant, and the query keeps its distinguishing power through a
    /// short content digest rather than by carrying its raw text (see
    /// [`CameraId::from_endpoint_url`]), so two channels on one host/port
    /// that differ only by path or query (the ordinary NVR-behind-one-host
    /// deployment) resolve to distinct identities instead of colliding. A
    /// camera's distinct `rtsp_url` (analysis) and `live_rtsp_url` (live)
    /// roles are reconciled at the config-entry level, not here — see
    /// `crate::config`, which derives an entry's identity from its
    /// analysis endpoint alone.
    ///
    /// `salt` keys the query digest (see [`CameraQuerySalt`]); the same
    /// salt must be used for every identity that is later compared, or
    /// the identical endpoint resolved twice under two different salts
    /// will spuriously look like two different cameras.
    pub fn from_rtsp_url(
        node_id: &str,
        url: &str,
        salt: &CameraQuerySalt,
    ) -> Result<Self, CameraIdError> {
        Self::from_endpoint_url(
            node_id,
            url,
            "rtsp_url",
            &[("rtsp", 554), ("rtsps", 554)],
            salt,
        )
    }

    /// Durable identity for an HTTP/MJPEG camera: the owning node plus the
    /// endpoint's canonical `scheme://host:port/path?q=<digest>`, on the
    /// same credential-stripping, path-preserving, digested-query terms as
    /// [`CameraId::from_rtsp_url`]. See that method's note on `salt`.
    pub fn from_mjpeg_url(
        node_id: &str,
        url: &str,
        salt: &CameraQuerySalt,
    ) -> Result<Self, CameraIdError> {
        Self::from_endpoint_url(
            node_id,
            url,
            "mjpeg_url",
            &[("http", 80), ("https", 443)],
            salt,
        )
    }

    /// Shared RTSP/MJPEG construction: identity reduces to
    /// `scheme://host:port/path?q=<digest>`, where the scheme is
    /// significant (transport and TLS posture), host case and the scheme's
    /// own default port are incidental (folded together), and path is kept
    /// as significant — a real NVR distinguishes channels by path (e.g.
    /// `/ch1` vs `/ch2`), so discarding it would silently collapse two
    /// real, distinctly configured cameras into one identity.
    ///
    /// The query keeps that same distinguishing power (a real NVR also
    /// distinguishes channels by query, e.g. `?channel=1` vs `?channel=2`)
    /// but its raw text is never retained in the identity at all — copying
    /// a query verbatim is a credential-disclosure route, since a
    /// query-carried token would otherwise leak, in full, into every
    /// surface a `CameraId` reaches (`as_str`, its derived `Debug`, mounts,
    /// telemetry keys). Instead the query's parameters are sorted (so
    /// arrival order is incidental) into a canonical `key=value&...` form,
    /// then reduced to a 16-hex-character digest of that canonical form,
    /// rendered as `?q=<digest>`. An absent or empty query contributes no
    /// digest component at all, preserving the identity a query-less URL
    /// always had. Only userinfo (credentials) is dropped structurally
    /// rather than by redaction — it is simply never read out of the
    /// parsed URL into the identity string.
    fn from_endpoint_url(
        node_id: &str,
        raw_url: &str,
        field: &'static str,
        allowed_schemes_and_default_ports: &[(&str, u16)],
        salt: &CameraQuerySalt,
    ) -> Result<Self, CameraIdError> {
        let node_id = Self::require_non_empty_node(node_id)?;
        let parsed = url::Url::parse(raw_url).map_err(|_| CameraIdError::InvalidUrl { field })?;
        let scheme = parsed.scheme();
        let default_port = allowed_schemes_and_default_ports
            .iter()
            .find(|(allowed_scheme, _)| *allowed_scheme == scheme)
            .map(|(_, default_port)| *default_port)
            .ok_or(CameraIdError::InvalidUrl { field })?;
        let host = parsed
            .host_str()
            .ok_or(CameraIdError::InvalidUrl { field })?
            .to_ascii_lowercase();
        let port = parsed.port().unwrap_or(default_port);
        let path = parsed.path();
        let query_component = match parsed.query() {
            Some(query) if !query.is_empty() => {
                format!("?q={}", Self::canonical_query_digest(&parsed, salt))
            }
            _ => String::new(),
        };
        Ok(Self(format!(
            "{node_id}:{scheme}://{host}:{port}{path}{query_component}"
        )))
    }

    /// Percent-encodes exactly the three bytes that would otherwise be
    /// ambiguous once a decoded key/value is rejoined with `=`/`&` as
    /// delimiters: `&`, `=`, and `%` itself (so an already-percent-encoded
    /// sequence in the source text cannot be replayed to fake a
    /// delimiter). Every other byte — including multi-byte UTF-8 sequences
    /// — passes through unchanged; those bytes are never combined with the
    /// escaped ones in the output, so the transform never coalesces a
    /// non-ASCII byte with an escaped `%`.
    ///
    /// This is the fix for the collision defect: `query_pairs()` already
    /// *decodes* percent-encoding on read, so rejoining decoded pairs with
    /// raw `key=value&...` delimiters let a decoded `&`/`=` in one pair's
    /// value be replayed as a delimiter for a *different* pair — two
    /// genuinely different query strings (one pair containing a literal
    /// `&`, versus two separate pairs) canonicalised to byte-identical
    /// text and therefore the same identity. Re-escaping before rejoining
    /// makes every canonical string decode back to exactly the pairs it
    /// was built from.
    fn percent_encode_delimiter_bytes(value: &str) -> String {
        let mut out = Vec::with_capacity(value.len());
        for byte in value.bytes() {
            match byte {
                b'&' | b'=' | b'%' => {
                    out.push(b'%');
                    out.extend_from_slice(format!("{byte:02X}").as_bytes());
                }
                other => out.push(other),
            }
        }
        String::from_utf8(out)
            .expect("escaping only ASCII delimiter bytes preserves UTF-8 validity")
    }

    /// Sorts the URL's DECODED query parameters (so arrival order is
    /// incidental), re-escapes each key/value's `&`/`=`/`%` bytes (see
    /// [`Self::percent_encode_delimiter_bytes`], so two different decoded
    /// pair-sets can never rejoin to the same text), joins them into a
    /// canonical `key=value&key=value` form, then reduces that canonical
    /// form to 16 hex characters of its HMAC-SHA256 digest keyed by `salt`
    /// (see [`CameraQuerySalt`] — this is what makes the digest a keyed
    /// MAC rather than a guessable unkeyed fingerprint of a possible
    /// secret). The raw query text is consumed entirely by this function
    /// and never returned — the caller only ever sees the digest.
    fn canonical_query_digest(parsed: &url::Url, salt: &CameraQuerySalt) -> String {
        let mut pairs: Vec<(std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>)> =
            parsed.query_pairs().collect();
        pairs.sort();
        let canonical = pairs
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    Self::percent_encode_delimiter_bytes(key),
                    Self::percent_encode_delimiter_bytes(value)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let mut mac = Hmac::<Sha256>::new_from_slice(salt.as_bytes())
            .expect("HMAC-SHA256 accepts a key of any length");
        mac.update(canonical.as_bytes());
        let result = mac.finalize().into_bytes();
        result[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn require_non_empty_node(node_id: &str) -> Result<&str, CameraIdError> {
        let node_id = node_id.trim();
        if node_id.is_empty() {
            return Err(CameraIdError::EmptyNode);
        }
        Ok(node_id)
    }
}

/// Which role of a camera's media a unit belongs to. A camera with a
/// distinct `live_rtsp_url` carries both roles as separate, concurrently
/// open source sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceRole {
    Analysis,
    Live,
}

/// A rational time base for source presentation time, e.g. 1/90000 for
/// RTP-clocked RTSP media. Both parts are non-zero so a timestamp can
/// always be interpreted as a real duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeBase {
    pub numerator: std::num::NonZeroU32,
    pub denominator: std::num::NonZeroU32,
}

impl TimeBase {
    pub fn new(numerator: std::num::NonZeroU32, denominator: std::num::NonZeroU32) -> Self {
        Self {
            numerator,
            denominator,
        }
    }
}

/// Honest source media timing: presentation time and timebase, decode
/// order, and duration, all independent of wall-clock observation time.
/// Never derived from `EncodedAccessUnit::observed_at`, and never used to
/// derive it — the two are separate facts about the same unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaTiming {
    pub time_base: TimeBase,
    /// Presentation timestamp, in `time_base` units.
    pub pts: i64,
    /// Decode timestamp, when the codec's decode order differs from
    /// presentation order; `None` when decode order equals PTS order.
    pub dts: Option<i64>,
    /// This unit's duration, in `time_base` units, when the source states
    /// one.
    pub duration: Option<u64>,
}

/// One encoded access unit with everything a robust consumer needs: a
/// hardware decoder, the RTSP/MoQ publishers, and recording all read the
/// same shape. Raw bytes alone are not enough for any of them.
#[derive(Debug, Clone)]
pub struct EncodedAccessUnit {
    pub stream_id: StreamId,
    /// Stream epoch: bumped on reconnect/format-change. A decoder instance,
    /// and a hub subscriber's random-access rejoin, are each valid for
    /// exactly one epoch.
    pub stream_epoch: u64,
    pub codec: VideoCodec,
    /// Parameter-set bytes (SPS/PPS/VPS or equivalent) when this unit
    /// carries codec configuration; `None` otherwise. An immutable shared
    /// buffer — never copied per consumer.
    pub codec_config: Option<Bytes>,
    /// This unit carries (part of) a random-access/keyframe picture.
    pub keyframe: bool,
    /// The encoded access-unit payload. An immutable shared buffer: cloning
    /// this struct, or handing the same unit to N hub subscribers, clones
    /// the `Bytes` handle only — never the underlying bytes.
    pub data: Bytes,
    /// Honest source presentation timing (PTS/DTS/duration + timebase),
    /// independent of `observed_at`. `None` on producers that have not yet
    /// filled real source timing: the RTSP path does not yet supply source
    /// timing, so this is `None` there; it is filled when the source
    /// producer learns the camera's media timeline. It is never
    /// substituted with the wall clock.
    pub timing: Option<MediaTiming>,
    /// Wall-clock time Vigil observed (received) this unit — separate from
    /// `timing`'s source presentation time. Never used to derive `timing`,
    /// and never derived from it.
    pub observed_at: Option<DateTime<Utc>>,
    /// Which camera this unit belongs to (durable identity, never the
    /// display name).
    pub camera: CameraId,
    /// Which source role (`Analysis` or `Live`) produced this unit.
    pub source_role: SourceRole,
    /// Monotonic per-epoch sequence number.
    pub sequence: u64,
    /// Set on the first unit after a reconnect or timeline break.
    pub discontinuity: bool,
    /// Set when the carried parameter sets differ from the previous ones
    /// (resolution/format change boundary).
    pub format_change: bool,
    /// The segment this unit belongs to (segment assembly identity).
    pub segment_sequence: u64,
}
