//! `vigil doctor acceleration` — translate host reality into operator action.
//!
//! Read-only, always. Doctor runs the same decoder and detector probes the
//! runtime uses and explains their receipts. It never mutates the host,
//! never edits Vigil configuration, and never flips the acceleration intent
//! booleans. Without sudo it answers "what can THIS process use right now";
//! with sudo it may additionally compare root capability against the
//! configured service user and print the exact operator action — but it must
//! never report root-only access as runtime-active acceleration.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::acceleration::AccelerationReceipt;

/// Host facts the doctor reads. A trait so checks are deterministic under
/// test; the real implementation reads the live system.
pub trait HostFacts {
    /// Effective uid of the current process.
    fn effective_uid(&self) -> u32;
    /// Effective gids (primary + supplemental) of the current process.
    fn effective_gids(&self) -> Vec<u32>;
    /// Render/video device nodes visible on this host (e.g. /dev/dri/*).
    fn visible_render_devices(&self) -> Vec<PathBuf>;
    /// stat-level facts for one device path.
    fn device_facts(&self, path: &std::path::Path) -> Option<DeviceFacts>;
    /// Try to open the device read-write as the CURRENT process.
    fn open_device(&self, path: &std::path::Path) -> Result<(), DeviceOpenError>;
    /// Value of an environment variable.
    fn env_var(&self, name: &str) -> Option<String>;
    /// The `User=` field of an installed vigil systemd unit, if one exists.
    fn systemd_service_user(&self) -> Option<String>;
    /// Groups a named user belongs to, when resolvable.
    fn user_groups(&self, user: &str) -> Option<Vec<String>>;
}

#[derive(Debug, Clone)]
pub struct DeviceFacts {
    pub owner: String,
    pub group: String,
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceOpenError {
    PermissionDenied,
    NotFound,
    Other(String),
}

/// How the sudo path learned (or failed to learn) the service user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceUserResolution {
    Flag(String),
    EnvVar(String),
    SystemdUnit(String),
    /// No source identified the runtime user: root capability cannot be
    /// translated into a service-user fix.
    Unresolved,
}

/// Resolve the configured service user in the fixed order:
/// `--service-user` flag, then `VIGIL_SERVICE_USER`, then the systemd unit.
pub fn resolve_service_user(
    flag_value: Option<&str>,
    facts: &dyn HostFacts,
) -> ServiceUserResolution {
    let _ = (flag_value, facts);
    unimplemented!("scaffold: service-user resolution is not implemented yet")
}

/// One device-access finding: how the current process's access to a render
/// device classifies, with evidence and exactly one suggested action.
/// Independent of artifact capability so the classification is testable on
/// any build; the report composes it with artifact facts.
#[derive(Debug, Clone)]
pub struct DeviceAccessFinding {
    pub failure_code: crate::acceleration::FailureCode,
    pub evidence_kind: crate::acceleration::EvidenceKind,
    pub evidence_fields: BTreeMap<String, String>,
    pub action_kind: crate::acceleration::ActionKind,
    pub action_payload: Option<String>,
}

/// Classify the current process's access to one render device: visible and
/// openable → none; visible but unopenable → permission_denied with the
/// device's real owner/group/mode plus the ready-to-run group fix; absent →
/// no_device_visible. Never infers root-only capability as usable.
pub fn classify_device_access(
    facts: &dyn HostFacts,
    path: &std::path::Path,
    current_user: &str,
) -> DeviceAccessFinding {
    let _ = (facts, path, current_user);
    unimplemented!("scaffold: device access classification is not implemented yet")
}

/// The doctor's structured output: one receipt per checked stage, rendered
/// with the shared fixed format.
#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub decode: AccelerationReceipt,
    pub detection: AccelerationReceipt,
    /// Extra sudo-mode findings keyed by check name (service-user
    /// comparison, host-level capability), each also receipt-shaped fields.
    pub notes: BTreeMap<String, String>,
}

/// Inputs the doctor needs beyond host facts.
#[derive(Debug, Clone)]
pub struct DoctorRequest {
    pub hardware_decoding: bool,
    pub accelerated_detection: bool,
    pub service_user_flag: Option<String>,
    pub detector_model_path: Option<PathBuf>,
}

/// Build the acceleration report from host facts + real probes. Pure with
/// respect to `facts`; performs no host mutation ever.
pub fn acceleration_report(request: &DoctorRequest, facts: &dyn HostFacts) -> DoctorReport {
    let _ = (request, facts);
    unimplemented!("scaffold: doctor acceleration report is not implemented yet")
}

/// Render the report in the fixed operator format (receipt blocks).
pub fn render_report(report: &DoctorReport) -> String {
    let _ = report;
    unimplemented!("scaffold: doctor report rendering is not implemented yet")
}

/// CLI entry point for `vigil doctor acceleration`.
pub fn run(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let _ = args;
    unimplemented!("scaffold: doctor CLI is not implemented yet")
}
