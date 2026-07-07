//! `vigil doctor acceleration`: read-only translation from host reality to
//! operator action. Current-process truth without sudo; root-vs-service-user
//! comparison with sudo; fixed vocabulary; never mutates; never flips intent.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use vigil::acceleration::{ActionKind, EvidenceKind, FailureCode, ProbeStatus};
use vigil::doctor::{
    DeviceFacts, DeviceOpenError, DoctorRequest, HostFacts, ServiceUserResolution,
    acceleration_report, classify_device_access, render_report, resolve_service_user,
};

/// Deterministic host-facts double. Returns what a real host would; call
/// counts let tests assert what the doctor did NOT touch.
struct FakeHost {
    euid: u32,
    gids: Vec<u32>,
    devices: Vec<PathBuf>,
    device_facts: BTreeMap<PathBuf, DeviceFacts>,
    open_results: BTreeMap<PathBuf, Result<(), DeviceOpenError>>,
    env: BTreeMap<String, String>,
    systemd_user: Option<String>,
    user_groups: BTreeMap<String, Vec<String>>,
    open_calls: Cell<u32>,
    probe_log: RefCell<Vec<String>>,
}

impl FakeHost {
    fn unprivileged_blocked() -> Self {
        let device = PathBuf::from("/dev/dri/renderD0");
        let mut device_facts = BTreeMap::new();
        device_facts.insert(
            device.clone(),
            DeviceFacts {
                owner: "root".to_string(),
                group: "render".to_string(),
                mode: "crw-rw----".to_string(),
            },
        );
        let mut open_results = BTreeMap::new();
        open_results.insert(device.clone(), Err(DeviceOpenError::PermissionDenied));
        Self {
            euid: 1000,
            gids: vec![1000],
            devices: vec![device],
            device_facts,
            open_results,
            env: BTreeMap::new(),
            systemd_user: None,
            user_groups: BTreeMap::new(),
            open_calls: Cell::new(0),
            probe_log: RefCell::new(Vec::new()),
        }
    }
}

impl HostFacts for FakeHost {
    fn effective_uid(&self) -> u32 {
        self.euid
    }
    fn effective_gids(&self) -> Vec<u32> {
        self.gids.clone()
    }
    fn visible_render_devices(&self) -> Vec<PathBuf> {
        self.devices.clone()
    }
    fn device_facts(&self, path: &Path) -> Option<DeviceFacts> {
        self.device_facts.get(path).cloned()
    }
    fn open_device(&self, path: &Path) -> Result<(), DeviceOpenError> {
        self.open_calls.set(self.open_calls.get() + 1);
        self.probe_log
            .borrow_mut()
            .push(format!("open:{}", path.display()));
        self.open_results
            .get(path)
            .cloned()
            .unwrap_or(Err(DeviceOpenError::NotFound))
    }
    fn env_var(&self, name: &str) -> Option<String> {
        self.env.get(name).cloned()
    }
    fn systemd_service_user(&self) -> Option<String> {
        self.systemd_user.clone()
    }
    fn user_groups(&self, user: &str) -> Option<Vec<String>> {
        self.user_groups.get(user).cloned()
    }
}

fn request(hardware_decoding: bool, accelerated_detection: bool) -> DoctorRequest {
    DoctorRequest {
        hardware_decoding,
        accelerated_detection,
        service_user_flag: None,
        detector_model_path: None,
    }
}

#[test]
fn blocked_device_classifies_permission_denied_with_ready_fix() {
    let host = FakeHost::unprivileged_blocked();
    let finding = classify_device_access(&host, Path::new("/dev/dri/renderD0"), "operator");

    assert_eq!(finding.failure_code, FailureCode::PermissionDenied);
    assert_eq!(finding.evidence_kind, EvidenceKind::DevicePath);
    assert_eq!(
        finding.evidence_fields.get("path").map(String::as_str),
        Some("/dev/dri/renderD0")
    );
    assert_eq!(
        finding.evidence_fields.get("group").map(String::as_str),
        Some("render"),
        "evidence carries the device's REAL group from stat"
    );
    assert_eq!(
        finding.evidence_fields.get("mode").map(String::as_str),
        Some("crw-rw----")
    );
    assert_eq!(finding.action_kind, ActionKind::RunCommand);
    let action = finding.action_payload.expect("a ready-to-run fix");
    assert!(
        action.contains("usermod") && action.contains("render") && action.contains("operator"),
        "the fix names the detected group and the affected user, ready to paste: {action}"
    );
}

#[test]
fn missing_device_is_no_device_visible_not_permission() {
    let mut host = FakeHost::unprivileged_blocked();
    host.devices.clear();
    host.device_facts.clear();
    host.open_results.clear();

    let finding = classify_device_access(&host, Path::new("/dev/dri/renderD0"), "operator");
    assert_eq!(finding.failure_code, FailureCode::NoDeviceVisible);
    assert_ne!(
        finding.action_kind,
        ActionKind::RunCommand,
        "no group fix can create a missing device"
    );
}

#[test]
fn no_sudo_reports_current_process_truth_only() {
    // Without sudo, doctor answers what THIS process can use. On a build
    // without the hardware decode feature the honest decode answer is the
    // artifact, not the host; detection on the CPU-only artifact is the
    // honest backend_not_compiled fallback.
    let host = FakeHost::unprivileged_blocked();
    let report = acceleration_report(&request(true, true), &host);

    assert!(
        report.decode.configured,
        "intent is preserved, never edited"
    );
    assert_eq!(report.decode.probe_status, ProbeStatus::Fallback);
    assert_eq!(
        report.decode.active_backend, "software",
        "the working software path is the active backend"
    );
    // Default test build compiles no hardware decode backend: the primary
    // truth is the artifact's.
    assert_eq!(
        report.decode.failure_code,
        FailureCode::UnsupportedByThisArtifact,
        "an artifact that cannot load native hardware runtimes says so"
    );
    assert_eq!(
        report.decode.action_kind,
        ActionKind::InstallSupportedArtifact
    );

    assert_eq!(
        report.detection.failure_code,
        FailureCode::BackendNotCompiled
    );
    assert_eq!(report.detection.active_backend, "burn-cpu");
    assert_eq!(report.detection.probe_status, ProbeStatus::Fallback);
}

#[test]
fn disabled_intent_skips_probes_and_reports_disabled() {
    let host = FakeHost::unprivileged_blocked();
    let report = acceleration_report(&request(false, false), &host);

    assert_eq!(report.decode.probe_status, ProbeStatus::Disabled);
    assert_eq!(report.detection.probe_status, ProbeStatus::Disabled);
    assert_eq!(report.decode.failure_code, FailureCode::None);
    assert_eq!(
        host.open_calls.get(),
        0,
        "hardware_decoding=false means NO device probes are attempted"
    );
}

#[test]
fn service_user_resolution_order_is_flag_env_systemd() {
    let mut host = FakeHost::unprivileged_blocked();
    host.env
        .insert("VIGIL_SERVICE_USER".to_string(), "env-user".to_string());
    host.systemd_user = Some("unit-user".to_string());

    assert_eq!(
        resolve_service_user(Some("flag-user"), &host),
        ServiceUserResolution::Flag("flag-user".to_string()),
        "--service-user wins over everything"
    );
    assert_eq!(
        resolve_service_user(None, &host),
        ServiceUserResolution::EnvVar("env-user".to_string()),
        "VIGIL_SERVICE_USER wins over the systemd unit"
    );
    host.env.clear();
    assert_eq!(
        resolve_service_user(None, &host),
        ServiceUserResolution::SystemdUnit("unit-user".to_string())
    );
    host.systemd_user = None;
    assert_eq!(
        resolve_service_user(None, &host),
        ServiceUserResolution::Unresolved,
        "no source → root capability cannot be translated into a service-user fix"
    );
}

#[test]
fn sudo_without_service_user_source_is_manual_action_required() {
    let mut host = FakeHost::unprivileged_blocked();
    host.euid = 0; // sudo mode
    host.gids = vec![0];

    let report = acceleration_report(&request(true, true), &host);
    let rendered = render_report(&report);
    assert!(
        rendered.contains("manual_action_required"),
        "sudo doctor with no resolvable runtime user must say manual_action_required \
         and must not translate root access into a fix: {rendered}"
    );
    assert!(
        !report.decode.hardware_accelerated,
        "root-only capability is never reported as runtime-active acceleration"
    );
}

#[test]
fn doctor_renders_receipts_in_fixed_format() {
    let host = FakeHost::unprivileged_blocked();
    let report = acceleration_report(&request(true, true), &host);
    let rendered = render_report(&report);

    let decode_at = rendered
        .find("[decode.hardware]")
        .expect("decode section present");
    let detect_at = rendered
        .find("[detect.acceleration]")
        .expect("detection section present");
    assert!(
        decode_at < detect_at,
        "sections render in fixed order: decode then detection"
    );
    for line in [
        "configured:",
        "status:",
        "attempted_backend:",
        "active_backend:",
        "failure_code:",
    ] {
        assert!(rendered.contains(line), "missing `{line}`:\n{rendered}");
    }
}

#[test]
fn doctor_cli_requires_the_acceleration_subcommand() {
    let error = vigil::doctor::run(vec![]).expect_err("bare `vigil doctor` explains usage");
    assert!(
        error.contains("acceleration"),
        "usage names the subcommand: {error}"
    );
    let error = vigil::doctor::run(vec![std::ffi::OsString::from("networking")])
        .expect_err("unknown doctor area is rejected");
    assert!(
        error.contains("acceleration"),
        "usage names the subcommand: {error}"
    );
}
