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

/// Whether a named user can open a device, judged from the device's real
/// mode/owner/group: world-rw grants anyone; owner-rw grants the owning
/// user; group-rw grants members (primary or supplemental) of the owning
/// group. Group membership alone is NOT the access truth — a world-writable
/// device (common in containers) is accessible to a user in no groups.
pub(crate) fn user_can_access_device(
    facts: &dyn HostFacts,
    device: &std::path::Path,
    user: &str,
) -> bool {
    let Some(device_facts) = facts.device_facts(device) else {
        return false;
    };
    let mode: Vec<char> = device_facts.mode.chars().collect();
    let rw_at =
        |offset: usize| mode.get(offset) == Some(&'r') && mode.get(offset + 1) == Some(&'w');
    if rw_at(7) {
        return true; // world-rw
    }
    if device_facts.owner == user && rw_at(1) {
        return true; // owner-rw
    }
    rw_at(4)
        && facts
            .user_groups(user)
            .map(|groups| groups.iter().any(|group| group == &device_facts.group))
            .unwrap_or(false)
}

/// Resolve the configured service user in the fixed order:
/// `--service-user` flag, then `VIGIL_SERVICE_USER`, then the systemd unit.
pub fn resolve_service_user(
    flag_value: Option<&str>,
    facts: &dyn HostFacts,
) -> ServiceUserResolution {
    if let Some(user) = flag_value {
        return ServiceUserResolution::Flag(user.to_string());
    }
    if let Some(user) = facts.env_var("VIGIL_SERVICE_USER") {
        return ServiceUserResolution::EnvVar(user);
    }
    if let Some(user) = facts.systemd_service_user() {
        return ServiceUserResolution::SystemdUnit(user);
    }
    ServiceUserResolution::Unresolved
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
    use crate::acceleration::{ActionKind, EvidenceKind, FailureCode};

    let mut evidence_fields = BTreeMap::new();
    evidence_fields.insert("path".to_string(), path.display().to_string());

    let facts_for_device = facts.device_facts(path);
    let visible =
        facts.visible_render_devices().iter().any(|d| d == path) || facts_for_device.is_some();
    if !visible {
        return DeviceAccessFinding {
            failure_code: FailureCode::NoDeviceVisible,
            evidence_kind: EvidenceKind::DevicePath,
            evidence_fields,
            action_kind: ActionKind::ManualActionRequired,
            action_payload: Some(
                "no render/video device is visible to this process; check drivers, \
                 VM passthrough, or container device mapping"
                    .to_string(),
            ),
        };
    }
    if let Some(device) = &facts_for_device {
        evidence_fields.insert("owner".to_string(), device.owner.clone());
        evidence_fields.insert("group".to_string(), device.group.clone());
        evidence_fields.insert("mode".to_string(), device.mode.clone());
    }
    evidence_fields.insert(
        "effective_uid".to_string(),
        facts.effective_uid().to_string(),
    );
    evidence_fields.insert(
        "effective_gids".to_string(),
        facts
            .effective_gids()
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(","),
    );

    match facts.open_device(path) {
        Ok(()) => DeviceAccessFinding {
            failure_code: FailureCode::None,
            evidence_kind: EvidenceKind::DevicePath,
            evidence_fields,
            action_kind: ActionKind::NoAction,
            action_payload: None,
        },
        Err(DeviceOpenError::NotFound) => DeviceAccessFinding {
            failure_code: FailureCode::NoDeviceVisible,
            evidence_kind: EvidenceKind::DevicePath,
            evidence_fields,
            action_kind: ActionKind::ManualActionRequired,
            action_payload: Some(
                "the device path disappeared between listing and open".to_string(),
            ),
        },
        Err(DeviceOpenError::PermissionDenied) => {
            let group = facts_for_device
                .as_ref()
                .map(|device| device.group.clone())
                .unwrap_or_else(|| "render".to_string());
            DeviceAccessFinding {
                failure_code: FailureCode::PermissionDenied,
                evidence_kind: EvidenceKind::DevicePath,
                evidence_fields,
                action_kind: ActionKind::RunCommand,
                action_payload: Some(format!(
                    "sudo usermod -aG {group} {current_user}\nthen restart the vigil service (or log out and back in)"
                )),
            }
        }
        Err(DeviceOpenError::Other(error)) => DeviceAccessFinding {
            failure_code: FailureCode::ProbeFailed,
            evidence_kind: EvidenceKind::UpstreamError,
            evidence_fields,
            action_kind: ActionKind::ManualActionRequired,
            action_payload: Some(error),
        },
    }
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
    use crate::acceleration::{
        AccelStage, AccelerationReceipt, ActionKind, EvidenceKind, FailureCode, ProbeStatus,
    };

    let blank = |stage: AccelStage, configured: bool| AccelerationReceipt {
        stage,
        work_id: None,
        parent_work_id: None,
        stream_id: None,
        media_item: None,
        configured,
        attempted_backend: "none".to_string(),
        active_backend: match stage {
            AccelStage::Decode => "software".to_string(),
            AccelStage::Detection => "burn-cpu".to_string(),
        },
        hardware_accelerated: false,
        selected_device: None,
        codec: None,
        model_id: None,
        model_version: None,
        input_shape: None,
        probe_status: ProbeStatus::Disabled,
        failure_code: FailureCode::None,
        evidence_kind: None,
        evidence_fields: BTreeMap::new(),
        action_kind: ActionKind::NoAction,
        action_payload: None,
    };

    // ── decode ──
    let decode = if !request.hardware_decoding {
        blank(AccelStage::Decode, false)
    } else {
        #[cfg(feature = "decode-gstreamer")]
        {
            doctor_decode_receipt_with_hardware_backend(request, facts, &blank)
        }
        #[cfg(not(feature = "decode-gstreamer"))]
        {
            // This artifact ships no native hardware decode runtime; that is
            // the primary truth regardless of host devices. No device probe
            // is attempted because the artifact could not use it anyway.
            let mut receipt = blank(AccelStage::Decode, true);
            receipt.probe_status = ProbeStatus::Fallback;
            receipt.failure_code = FailureCode::UnsupportedByThisArtifact;
            receipt.evidence_kind = Some(EvidenceKind::SelectedBackend);
            receipt.evidence_fields.insert(
                "compiled_decode_backends".to_string(),
                "software".to_string(),
            );
            receipt.action_kind = ActionKind::InstallSupportedArtifact;
            receipt.action_payload =
                Some("install a hardware-enabled artifact, or keep software decode".to_string());
            receipt
        }
    };

    // ── detection ──
    let detection = if !request.accelerated_detection {
        blank(AccelStage::Detection, false)
    } else {
        // No accelerated Burn backend is compiled into this artifact today;
        // when one exists this branch probes it with a real model forward.
        let mut receipt = blank(AccelStage::Detection, true);
        receipt.input_shape = Some(crate::yolox_detector::MODEL_INPUT_SHAPE.to_string());
        receipt.probe_status = ProbeStatus::Fallback;
        receipt.failure_code = FailureCode::BackendNotCompiled;
        receipt.evidence_kind = Some(EvidenceKind::SelectedBackend);
        receipt
            .evidence_fields
            .insert("compiled_backends".to_string(), "burn-cpu".to_string());
        receipt.action_kind = ActionKind::InstallSupportedArtifact;
        receipt.action_payload = Some(
            "install a build with an accelerated detector backend, or keep CPU fallback"
                .to_string(),
        );
        receipt
    };

    // ── sudo-mode extras ──
    let mut notes = BTreeMap::new();
    if facts.effective_uid() == 0 {
        match resolve_service_user(request.service_user_flag.as_deref(), facts) {
            ServiceUserResolution::Unresolved => {
                notes.insert(
                    "service_user".to_string(),
                    "manual_action_required: no --service-user flag, VIGIL_SERVICE_USER, or \
                     vigil systemd unit User= identifies the runtime user; root capability \
                     cannot be translated into a service-user fix"
                        .to_string(),
                );
            }
            resolution => {
                let user = match &resolution {
                    ServiceUserResolution::Flag(user)
                    | ServiceUserResolution::EnvVar(user)
                    | ServiceUserResolution::SystemdUnit(user) => user.clone(),
                    ServiceUserResolution::Unresolved => unreachable!(),
                };
                for device in facts.visible_render_devices() {
                    let device_group = facts
                        .device_facts(&device)
                        .map(|facts| facts.group)
                        .unwrap_or_else(|| "render".to_string());
                    if !user_can_access_device(facts, &device, &user) {
                        notes.insert(
                            format!("service_user_access:{}", device.display()),
                            format!(
                                "run_command: sudo usermod -aG {device_group} {user} && \
                                 restart the vigil service"
                            ),
                        );
                    }
                }
            }
        }
    }

    DoctorReport {
        decode,
        detection,
        notes,
    }
}

#[cfg(feature = "decode-gstreamer")]
fn doctor_decode_receipt_with_hardware_backend(
    request: &DoctorRequest,
    facts: &dyn HostFacts,
    blank: &dyn Fn(
        crate::acceleration::AccelStage,
        bool,
    ) -> crate::acceleration::AccelerationReceipt,
) -> crate::acceleration::AccelerationReceipt {
    use crate::acceleration::{AccelStage, ActionKind, EvidenceKind, FailureCode, ProbeStatus};
    use crate::media_pipeline::VideoCodec;
    use crate::workgraph::StreamId;

    // Device access first: a blocked device is the actionable finding.
    let current_user = facts
        .env_var("USER")
        .unwrap_or_else(|| facts.effective_uid().to_string());
    let mut device_finding: Option<DeviceAccessFinding> = None;
    for device in facts.visible_render_devices() {
        let finding = classify_device_access(facts, &device, &current_user);
        if finding.failure_code == FailureCode::None {
            device_finding = Some(finding);
            break;
        }
        device_finding = Some(finding);
    }
    let devices = facts.visible_render_devices();

    let mut receipt = blank(AccelStage::Decode, true);
    receipt.attempted_backend = "gstreamer".to_string();

    if devices.is_empty() {
        receipt.probe_status = ProbeStatus::Fallback;
        receipt.failure_code = FailureCode::NoDeviceVisible;
        receipt.evidence_kind = Some(EvidenceKind::DevicePath);
        receipt.action_kind = ActionKind::ManualActionRequired;
        receipt.action_payload = Some(
            "no render/video device is visible; check drivers, VM passthrough, or \
             container device mapping"
                .to_string(),
        );
        return receipt;
    }
    if let Some(finding) = device_finding
        && finding.failure_code != FailureCode::None
    {
        receipt.probe_status = ProbeStatus::Fallback;
        receipt.failure_code = finding.failure_code;
        receipt.evidence_kind = Some(finding.evidence_kind);
        receipt.evidence_fields = finding.evidence_fields;
        receipt.action_kind = finding.action_kind;
        receipt.action_payload = finding.action_payload;
        return receipt;
    }

    // Device openable: run the same real decode probe the runtime uses, on a
    // synthetic H.264 sample encoded in-process.
    let sample = crate::decode_gstreamer::synthetic_h264_probe_sample();
    match crate::decode_gstreamer::GstreamerDecodeBackend::probe_and_build(
        StreamId::new("doctor-probe"),
        VideoCodec::H264,
        0,
        &sample,
    ) {
        Ok(selection) => {
            let mut probe_receipt = selection.receipt;
            if probe_receipt.selected_device.is_none() {
                // Element metadata did not expose the device; the doctor
                // KNOWS which device this process can open — say that one.
                probe_receipt.selected_device = facts
                    .visible_render_devices()
                    .into_iter()
                    .find(|device| facts.open_device(device).is_ok())
                    .map(|device| device.display().to_string());
            }
            probe_receipt.evidence_fields.insert(
                "effective_uid".to_string(),
                facts.effective_uid().to_string(),
            );
            probe_receipt.evidence_fields.insert(
                "effective_gids".to_string(),
                facts
                    .effective_gids()
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            );
            if facts.effective_uid() == 0 {
                // A root-run probe proves HOST capability only. Root-only
                // access is never reported as runtime-active: the receipt
                // stays active only when the resolved service user can also
                // open the device group.
                probe_receipt.evidence_fields.insert(
                    "probed_as".to_string(),
                    "root (host capability)".to_string(),
                );
                let service_user =
                    match resolve_service_user(request.service_user_flag.as_deref(), facts) {
                        ServiceUserResolution::Flag(user)
                        | ServiceUserResolution::EnvVar(user)
                        | ServiceUserResolution::SystemdUnit(user) => Some(user),
                        ServiceUserResolution::Unresolved => None,
                    };
                // The runtime needs ONE usable device (selection takes any
                // openable node); demoting a working setup because a SECOND
                // GPU is group-restricted would send the operator fixing a
                // non-problem. Blocked extras surface as notes below.
                let service_user_has_access = service_user.as_deref().is_some_and(|user| {
                    facts
                        .visible_render_devices()
                        .iter()
                        .any(|device| user_can_access_device(facts, device, user))
                });
                if !service_user_has_access {
                    probe_receipt.probe_status = ProbeStatus::Fallback;
                    probe_receipt.hardware_accelerated = false;
                    probe_receipt.active_backend = "software".to_string();
                    probe_receipt.failure_code = FailureCode::PermissionDenied;
                    probe_receipt.evidence_kind = Some(EvidenceKind::ProcessCredentials);
                    match service_user {
                        Some(user) => {
                            // Name the group of a device the user CANNOT
                            // open — the actual gap, not the first device.
                            let group = facts
                                .visible_render_devices()
                                .iter()
                                .find(|device| !user_can_access_device(facts, device, &user))
                                .and_then(|device| facts.device_facts(device))
                                .map(|facts| facts.group)
                                .unwrap_or_else(|| "render".to_string());
                            probe_receipt.action_kind = ActionKind::RunCommand;
                            probe_receipt.action_payload = Some(format!(
                                "sudo usermod -aG {group} {user}\nthen restart the vigil service"
                            ));
                        }
                        None => {
                            probe_receipt.action_kind = ActionKind::ManualActionRequired;
                            probe_receipt.action_payload = Some(
                                "host hardware decode works for root, but no service user \
                                 could be resolved to verify runtime access"
                                    .to_string(),
                            );
                        }
                    }
                }
            }
            probe_receipt
        }
        Err(fallback) => {
            receipt.probe_status = ProbeStatus::Fallback;
            receipt.failure_code = fallback.failure_code;
            receipt.evidence_kind = Some(fallback.evidence_kind);
            receipt.evidence_fields = fallback.evidence_fields;
            receipt.action_kind = fallback.action_kind;
            receipt.action_payload = fallback.action_payload;
            receipt
        }
    }
}

/// Render the report in the fixed operator format (receipt blocks).
pub fn render_report(report: &DoctorReport) -> String {
    let mut out = String::new();
    out.push_str(&crate::acceleration::render_receipt_block(&report.decode));
    out.push('\n');
    out.push_str(&crate::acceleration::render_receipt_block(
        &report.detection,
    ));
    for (check, note) in &report.notes {
        out.push('\n');
        out.push_str(&format!("[{check}]\n{note}\n"));
    }
    out
}

/// CLI entry point for `vigil doctor acceleration`.
pub fn run(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let mut area: Option<String> = None;
    let mut service_user_flag: Option<String> = None;
    let mut passthrough: Vec<std::ffi::OsString> = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy().to_string();
        match text.as_str() {
            "--service-user" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--service-user requires a value".to_string())?;
                service_user_flag = Some(value.to_string_lossy().to_string());
            }
            _ if area.is_none() && !text.starts_with('-') => area = Some(text),
            _ => passthrough.push(arg),
        }
    }
    match area.as_deref() {
        Some("acceleration") => {}
        _ => {
            return Err(
                "usage: vigil doctor acceleration [--service-user USER] [run options]".to_string(),
            );
        }
    }

    let config = crate::config::load(passthrough)?;
    let request = DoctorRequest {
        hardware_decoding: config.hardware_decoding,
        accelerated_detection: config.accelerated_detection,
        service_user_flag,
        detector_model_path: config.detector_model_path.clone(),
    };
    let facts = RealHostFacts;
    let mut report = acceleration_report(&request, &facts);
    // The detection receipt names the configured model identity. Stamped
    // here on the CLI surface: DoctorRequest's shape is pinned by frozen
    // contract tests, so the id rides the report rather than the request.
    if report.detection.model_id.is_none() {
        report.detection.model_id = Some(config.detector_model_id.clone());
    }
    println!("{}", render_report(&report));
    Ok(())
}

/// The live device-access finding for the CURRENT process, using the same
/// classification doctor renders. `None` means at least one render device
/// is usable; `Some` carries the classified reason (permission_denied /
/// no_device_visible / …) so the runtime's decode receipts tell the same
/// truth doctor does — a permission problem is never misreported as a
/// missing plugin.
#[cfg(feature = "decode-gstreamer")]
pub(crate) fn live_device_access_finding() -> Option<DeviceAccessFinding> {
    let facts = RealHostFacts;
    let devices = facts.visible_render_devices();
    let current_user = facts
        .env_var("USER")
        .unwrap_or_else(|| facts.effective_uid().to_string());
    if devices.is_empty() {
        let mut evidence_fields = BTreeMap::new();
        evidence_fields.insert("path".to_string(), "/dev/dri".to_string());
        return Some(DeviceAccessFinding {
            failure_code: crate::acceleration::FailureCode::NoDeviceVisible,
            evidence_kind: crate::acceleration::EvidenceKind::DevicePath,
            evidence_fields,
            action_kind: crate::acceleration::ActionKind::ManualActionRequired,
            action_payload: Some(
                "no render/video device is visible to this process; check drivers, \
                 VM passthrough, or container device mapping"
                    .to_string(),
            ),
        });
    }
    let _ = &current_user;
    let mut blocked = None;
    for device in devices {
        let finding = classify_device_access(&facts, &device, &current_user);
        if finding.failure_code == crate::acceleration::FailureCode::None {
            return None;
        }
        blocked = Some(finding);
    }
    blocked
}

/// The first render device the CURRENT process can actually open, for
/// backfilling `selected_device` when decoder element metadata does not
/// expose one.
#[cfg(feature = "decode-gstreamer")]
pub(crate) fn first_usable_render_device() -> Option<std::path::PathBuf> {
    let facts = RealHostFacts;
    facts
        .visible_render_devices()
        .into_iter()
        .find(|device| facts.open_device(device).is_ok())
}

/// Live host facts for the real doctor run. Read-only by construction.
struct RealHostFacts;

impl HostFacts for RealHostFacts {
    fn effective_uid(&self) -> u32 {
        #[cfg(unix)]
        unsafe {
            libc::geteuid()
        }
        #[cfg(not(unix))]
        0
    }

    fn effective_gids(&self) -> Vec<u32> {
        #[cfg(unix)]
        {
            // Size query first: a fixed buffer EINVALs on >64 groups and
            // would blank the evidence field.
            let needed = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
            let capacity = if needed > 0 { needed as usize } else { 64 };
            let mut gids = vec![0 as libc::gid_t; capacity];
            let count = unsafe { libc::getgroups(gids.len() as i32, gids.as_mut_ptr()) };
            if count >= 0 {
                gids.truncate(count as usize);
                let mut all: Vec<u32> = gids.into_iter().collect();
                let egid = unsafe { libc::getegid() } as u32;
                if !all.contains(&egid) {
                    all.push(egid);
                }
                return all;
            }
            Vec::new()
        }
        #[cfg(not(unix))]
        Vec::new()
    }

    fn visible_render_devices(&self) -> Vec<PathBuf> {
        let mut devices = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/dev/dri") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("renderD") {
                    devices.push(entry.path());
                }
            }
        }
        devices.sort();
        devices
    }

    fn device_facts(&self, path: &std::path::Path) -> Option<DeviceFacts> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(path).ok()?;
            Some(DeviceFacts {
                owner: lookup_user_name(metadata.uid())
                    .unwrap_or_else(|| metadata.uid().to_string()),
                group: lookup_group_name(metadata.gid())
                    .unwrap_or_else(|| metadata.gid().to_string()),
                mode: format_mode(metadata.mode()),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    }

    fn open_device(&self, path: &std::path::Path) -> Result<(), DeviceOpenError> {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                Err(DeviceOpenError::PermissionDenied)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(DeviceOpenError::NotFound)
            }
            Err(error) => Err(DeviceOpenError::Other(error.to_string())),
        }
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn systemd_service_user(&self) -> Option<String> {
        for unit_path in [
            "/etc/systemd/system/vigil.service",
            "/lib/systemd/system/vigil.service",
            "/usr/lib/systemd/system/vigil.service",
        ] {
            if let Ok(unit) = std::fs::read_to_string(unit_path) {
                for line in unit.lines() {
                    let line = line.trim();
                    if let Some(user) = line.strip_prefix("User=") {
                        let user = user.trim();
                        if !user.is_empty() {
                            return Some(user.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    fn user_groups(&self, user: &str) -> Option<Vec<String>> {
        let group_file = std::fs::read_to_string("/etc/group").ok()?;
        // The user's PRIMARY group (from passwd) counts too — membership is
        // not only the /etc/group member lists.
        let primary_gid = std::fs::read_to_string("/etc/passwd")
            .ok()
            .and_then(|passwd| {
                passwd.lines().find_map(|line| {
                    let mut fields = line.split(':');
                    (fields.next() == Some(user)).then(|| {
                        let _password = fields.next();
                        let _uid = fields.next();
                        fields.next().and_then(|gid| gid.parse::<u32>().ok())
                    })?
                })
            });
        let mut groups = Vec::new();
        for line in group_file.lines() {
            let mut fields = line.split(':');
            let Some(group_name) = fields.next() else {
                continue;
            };
            let _password = fields.next();
            let gid = fields.next().and_then(|gid| gid.parse::<u32>().ok());
            let members = fields.next().unwrap_or("");
            let is_member = members.split(',').any(|member| member.trim() == user);
            let is_primary = primary_gid.is_some() && gid == primary_gid;
            if is_member || is_primary {
                groups.push(group_name.to_string());
            }
        }
        Some(groups)
    }
}

#[cfg(unix)]
fn lookup_user_name(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next();
        if let Some(entry_uid) = fields.next()
            && entry_uid.parse::<u32>().ok() == Some(uid)
        {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(unix)]
fn lookup_group_name(gid: u32) -> Option<String> {
    let group_file = std::fs::read_to_string("/etc/group").ok()?;
    for line in group_file.lines() {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next();
        if let Some(entry_gid) = fields.next()
            && entry_gid.parse::<u32>().ok() == Some(gid)
        {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(unix)]
fn format_mode(mode: u32) -> String {
    let file_type = if mode & libc::S_IFMT == libc::S_IFCHR {
        'c'
    } else {
        '-'
    };
    let bits = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    let mut out = String::new();
    out.push(file_type);
    for (bit, ch) in bits {
        out.push(if mode & bit != 0 { ch } else { '-' });
    }
    out
}
