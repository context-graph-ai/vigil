use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use quote::ToTokens;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, ExprCall, ExprLit, ExprMethodCall, ImplItemFn, Item, ItemFn, ItemImpl,
    ItemMod, ItemUse, Lit, Macro, Meta, UseTree,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    #[serde(default)]
    file: Vec<FileAllowance>,
    #[serde(default)]
    site: Vec<SiteAllowance>,
}

const DOCUMENTATION_CONTRACT_VERSION: u32 = 1;
const TEST_CONTRACT_VERSION: u32 = 1;
const TEST_BASELINE_VERSION: u32 = 1;
const SOURCE_SCAN_ALLOWLIST_VERSION: u32 = 1;
const NON_RUST_CONTRACT_VERSION: u32 = 2;
const NON_RUST_BASELINE_VERSION: u32 = 1;
const NEXTEST_INVENTORY_VERSION: u32 = 1;
const FROZEN_TEST_BASE_SHA: &str = "4e3020713317e041423c9d0f00b20f76d66b2939";
// Filled from the frozen-sha proposal after its vector has been independently inspected.
const FROZEN_TEST_VECTOR_SHA256: &str =
    "sha256:ea12df9996c7270498624f613b5e91ff233fe14b48e62669da1e457427583867";
const FROZEN_NON_RUST_VECTOR_SHA256: &str =
    "sha256:606432470d39f4b409c49748a11531ff368f61b2303eeab8503f66f870aa81c5";
const CLAIM_TAG_PREFIX: &str = "<!-- vigil-claim:";
const ENFORCEMENT_TAG_PREFIX: &str = "<!-- enforced by:";
const UNENFORCED_TAG_PREFIX: &str = "<!-- vigil-unenforced:";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentationContracts {
    version: u32,
    #[serde(default)]
    claim: Vec<DocumentationClaim>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentationClaim {
    id: String,
    document: String,
    paragraph_sha256: String,
    evidence_tier: EvidenceTier,
    tests: Vec<String>,
    reviewed: bool,
    reviewed_by: String,
    rationale: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestContracts {
    version: u32,
    contract: Vec<TestContract>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestContract {
    id: String,
    lineage: String,
    sources: Vec<String>,
    evidence_kind: TestEvidenceKind,
    gate: TestGate,
    tests: Vec<String>,
    #[serde(default)]
    deterministic_witness: Option<String>,
    #[serde(default)]
    explicit_run: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum TestEvidenceKind {
    BehavioralUnit,
    Integration,
    Acceptance,
    Structural,
    Physical,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum TestGate {
    PullRequest,
    DecodeFeature,
    AcceleratedDetection,
    CombinedAcceleration,
    Fabric,
    SlowAcceptance,
    Nightly,
    Physical,
}

impl TestGate {
    fn requires_witness(self) -> bool {
        matches!(self, Self::Nightly | Self::Physical)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestBaseline {
    version: u32,
    base_sha: String,
    vector_sha256: String,
    tests: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestReceipts {
    version: u32,
    #[serde(default)]
    transition: Vec<TestTransitionReceipt>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestTransitionReceipt {
    id: String,
    base_sha: String,
    before_vector_sha256: String,
    after_vector_sha256: String,
    removed_tests: Vec<String>,
    added_tests: Vec<String>,
    surviving_tests: Vec<String>,
    zero_lost_detection: String,
    rationale: String,
    reviewer: String,
    proof: Vec<TestTransitionProof>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum TestTransitionProof {
    OwnerRuling {
        removed_tests: Vec<String>,
        ruling: String,
        reviewer: String,
    },
    MutationComparison {
        removed_tests: Vec<String>,
        before_outcomes: String,
        before_sha256: String,
        after_outcomes: String,
        after_sha256: String,
        mutants: String,
        mutants_sha256: String,
        reviewer: String,
    },
    RedundantFaultDetection {
        removed_tests: Vec<String>,
        surviving_test: String,
        evidence: String,
        evidence_sha256: String,
        reviewer: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RedundantFaultEvidence {
    version: u32,
    planted_fault: PlantedFault,
    removed: FaultDetectionRun,
    survivor: FaultDetectionRun,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlantedFault {
    id: String,
    source_sha256: String,
    diff: String,
    diff_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FaultDetectionRun {
    test: String,
    outcome: String,
    command: String,
    exit_code: i32,
    duration_seconds: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceScanAllowlist {
    version: u32,
    #[serde(default)]
    file: Vec<SourceScanAllowance>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceScanAllowance {
    path: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NonRustContracts {
    version: u32,
    case: Vec<NonRustCase>,
    asset: Vec<NonRustAsset>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NonRustBaseline {
    version: u32,
    vector_sha256: String,
    cases: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NonRustReceipts {
    version: u32,
    #[serde(default)]
    transition: Vec<NonRustTransitionReceipt>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NonRustTransitionReceipt {
    id: String,
    before_vector_sha256: String,
    after_vector_sha256: String,
    removed_cases: Vec<String>,
    added_cases: Vec<String>,
    status_changes: Vec<NonRustStatusChange>,
    zero_lost_detection: String,
    rationale: String,
    reviewer: String,
    proof: Vec<NonRustTransitionProof>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
struct NonRustStatusChange {
    id: String,
    before: NonRustCaseStatus,
    after: NonRustCaseStatus,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum NonRustTransitionProof {
    OwnerRuling {
        cases: Vec<String>,
        ruling: String,
        reviewer: String,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NonRustCase {
    id: String,
    source: String,
    handler: String,
    gate: TestGate,
    status: NonRustCaseStatus,
    lineage: String,
    reviewed_by: String,
    blocker_reason: Option<String>,
    deterministic_witnesses: Vec<String>,
    explicit_run: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
enum NonRustCaseStatus {
    Active,
    ImplementationBlockedPhysical,
    StaleRejectedContract,
    FalseGreenPhysicalSkip,
}

impl NonRustCaseStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ImplementationBlockedPhysical => "implementation-blocked-physical",
            Self::StaleRejectedContract => "stale-rejected-contract",
            Self::FalseGreenPhysicalSkip => "false-green-physical-skip",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NonRustAsset {
    path: String,
    sha256: String,
    kind: NonRustAssetKind,
    lineage: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NextestInventories {
    version: u32,
    shape: Vec<NextestShape>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NextestShape {
    name: String,
    command: String,
    identities: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum NonRustAssetKind {
    ShellSuite,
    ShellHelper,
    BrowserHarness,
    PhysicalChecklist,
    ProbeSource,
    TraceImage,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TestRecord {
    identity: String,
    source: String,
    ignored: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum EvidenceTier {
    Acceptance,
    Integration,
    BehavioralUnit,
    StructuralContract,
}

impl EvidenceTier {
    fn as_str(self) -> &'static str {
        match self {
            Self::Acceptance => "acceptance",
            Self::Integration => "integration",
            Self::BehavioralUnit => "behavioral_unit",
            Self::StructuralContract => "structural_contract",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DocumentationBinding {
    id: String,
    document: String,
    paragraph: String,
    paragraph_sha256: String,
    tests: BTreeSet<String>,
    first_enforcement_tag: String,
    line: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAllowance {
    path: String,
    #[serde(default)]
    sleeps: usize,
    #[serde(default)]
    raw_clocks: usize,
    #[serde(default)]
    free_port_helpers: usize,
    #[serde(default)]
    elapsed_calls: usize,
    reason: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SiteAllowance {
    path: String,
    scope: String,
    kind: SiteKind,
    fingerprint: String,
    reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum SiteKind {
    Sleep,
    MacroSleep,
    RawClock,
    ElapsedCall,
    BindPortZero,
}

impl SiteKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::MacroSleep => "macro_sleep",
            Self::RawClock => "raw_clock",
            Self::ElapsedCall => "elapsed_call",
            Self::BindPortZero => "bind_port_zero",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Counts {
    sleeps: usize,
    raw_clocks: usize,
    free_port_helpers: usize,
    elapsed_calls: usize,
}

impl Counts {
    fn record(&mut self, kind: SiteKind) {
        match kind {
            SiteKind::Sleep | SiteKind::MacroSleep => self.sleeps += 1,
            SiteKind::RawClock => self.raw_clocks += 1,
            SiteKind::ElapsedCall => self.elapsed_calls += 1,
            SiteKind::BindPortZero => self.free_port_helpers += 1,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SiteKey {
    path: String,
    scope: String,
    kind: SiteKind,
    fingerprint: String,
}

#[derive(Clone, Debug)]
struct ObservedSite {
    key: SiteKey,
    syntax: String,
}

#[derive(Default)]
struct CheckOptions {
    docs: Option<PathBuf>,
    nextest_json: Vec<(String, PathBuf)>,
}

pub(crate) fn check(args: &[OsString]) -> Result<(), String> {
    let options = parse_args(args)?;
    let root = super::repo_root()?;
    let ledger_path = root.join(".config/test-estate-exceptions.toml");
    let ledger: Ledger = toml::from_str(
        &fs::read_to_string(&ledger_path)
            .map_err(|error| format!("read {}: {error}", ledger_path.display()))?,
    )
    .map_err(|error| format!("parse {}: {error}", ledger_path.display()))?;

    let mut violations = audit_rust(&root, &ledger)?;
    audit_nextest_config(&root, &mut violations)?;
    audit_ci_config(&root, &mut violations)?;
    audit_codeowners(&root, &mut violations)?;
    audit_test_contracts(&root, &mut violations)?;
    audit_non_rust_contracts(&root, &mut violations)?;
    audit_nextest_inventories(&root, &options.nextest_json, &mut violations)?;
    audit_source_scan_allowlist(&root, &mut violations)?;
    audit_fabric_worker_lease_wiring(&root, &mut violations)?;
    audit_correction_writer_boundary(&root, &mut violations)?;
    audit_silent_prerequisite_passes(&root, &mut violations)?;
    let contracts = read_documentation_contracts(&root)?;
    let docs_root = options
        .docs
        .or_else(|| root.join("README.md").is_file().then(|| root.clone()));
    if let Some(docs) = docs_root {
        audit_doc_bindings(&root, &docs, &contracts, &mut violations)?;
        audit_contract_paragraph_coverage(&docs, &mut violations)?;
    } else if !contracts.claim.is_empty() {
        violations.push(
            "documentation contracts are registered, but the repository README.md is missing"
                .to_string(),
        );
    }

    if violations.is_empty() {
        println!("test-estate discipline: OK");
        Ok(())
    } else {
        Err(format!(
            "test-estate discipline violations:\n- {}",
            violations.join("\n- ")
        ))
    }
}

fn read_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let content =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    toml::from_str(&content).map_err(|error| format!("parse {}: {error}", path.display()))
}

fn audit_test_contracts(root: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    let contracts_path = root.join(".config/test-contracts.toml");
    let baseline_path = root.join(".config/test-estate-baseline.toml");
    let receipts_path = root.join(".config/test-estate-receipts.toml");
    let contracts: TestContracts = read_toml(&contracts_path)?;
    let baseline: TestBaseline = read_toml(&baseline_path)?;
    let receipts: TestReceipts = read_toml(&receipts_path)?;
    if contracts.version != TEST_CONTRACT_VERSION {
        violations.push(format!(
            "{} uses version {}; expected {TEST_CONTRACT_VERSION}",
            contracts_path.display(),
            contracts.version
        ));
    }
    if baseline.version != TEST_BASELINE_VERSION {
        violations.push(format!(
            "{} uses version {}; expected {TEST_BASELINE_VERSION}",
            baseline_path.display(),
            baseline.version
        ));
    }
    if receipts.version != TEST_BASELINE_VERSION {
        violations.push(format!(
            "{} uses version {}; expected {TEST_BASELINE_VERSION}",
            receipts_path.display(),
            receipts.version
        ));
    }
    if baseline.base_sha != FROZEN_TEST_BASE_SHA {
        violations.push(format!(
            "frozen test baseline names base SHA {}; guard requires {FROZEN_TEST_BASE_SHA}",
            baseline.base_sha
        ));
    }
    let baseline_set = exact_identity_vector(&baseline.tests, "frozen baseline", violations);
    let observed_baseline_digest = identity_vector_digest(&baseline_set);
    if baseline.vector_sha256 != observed_baseline_digest {
        violations.push(format!(
            "frozen baseline vector digest is {}, but its exact sorted IDs digest to {observed_baseline_digest}",
            baseline.vector_sha256
        ));
    }
    if FROZEN_TEST_VECTOR_SHA256 != "PENDING_FROZEN_VECTOR"
        && baseline.vector_sha256 != FROZEN_TEST_VECTOR_SHA256
    {
        violations.push(format!(
            "frozen baseline digest changed: file says {}, compiled guard requires {FROZEN_TEST_VECTOR_SHA256}",
            baseline.vector_sha256
        ));
    }

    let observed_records = collect_test_records(root)?;
    let observed = observed_records
        .iter()
        .map(|record| record.identity.clone())
        .collect::<BTreeSet<_>>();
    let mut registered = BTreeMap::<String, (&TestContract, usize)>::new();
    let mut contract_ids = BTreeSet::new();
    for contract in &contracts.contract {
        if !valid_contract_id(&contract.id) {
            violations.push(format!(
                "test contract ID {:?} must be a descriptive lowercase hyphenated phrase",
                contract.id
            ));
        }
        if !contract_ids.insert(contract.id.clone()) {
            violations.push(format!("test contract `{}` is duplicated", contract.id));
        }
        if !valid_review_attestation(&contract.lineage) {
            violations.push(format!(
                "test contract `{}` has no concrete source/lineage",
                contract.id
            ));
        }
        if contract.tests.is_empty() {
            violations.push(format!("test contract `{}` has no tests", contract.id));
        }
        let sources = contract.sources.iter().cloned().collect::<BTreeSet<_>>();
        if sources.len() != contract.sources.len() || contract.sources.iter().ne(sources.iter()) {
            violations.push(format!(
                "test contract `{}` source paths must be unique and bytewise sorted",
                contract.id
            ));
        }
        let actual_sources = observed_records
            .iter()
            .filter(|record| contract.tests.contains(&record.identity))
            .map(|record| record.source.clone())
            .collect::<BTreeSet<_>>();
        if sources != actual_sources {
            violations.push(format!(
                "test contract `{}` source paths changed; registered={sources:?}, observed={actual_sources:?}",
                contract.id
            ));
        }
        for (index, test) in contract.tests.iter().enumerate() {
            if registered.insert(test.clone(), (contract, index)).is_some() {
                violations.push(format!(
                    "active test `{test}` is assigned to more than one contract"
                ));
            }
        }
        validate_physical_contract(contract, &observed_records, violations);
    }
    for record in &observed_records {
        if !registered.contains_key(&record.identity) {
            violations.push(format!(
                "active test `{}` from {} is unregistered",
                record.identity, record.source
            ));
        }
    }
    for test in registered.keys() {
        if !observed.contains(test) {
            violations.push(format!("registered test `{test}` is stale or renamed"));
        }
    }
    let registered_set = registered.keys().cloned().collect::<BTreeSet<_>>();
    audit_test_transitions(
        root,
        &baseline,
        &baseline_set,
        &registered_set,
        &receipts,
        violations,
    );
    println!(
        "test contracts checked: {} active identities in {} behavioral families",
        observed.len(),
        contracts.contract.len()
    );
    Ok(())
}

fn audit_nextest_inventories(
    root: &Path,
    observed_json: &[(String, PathBuf)],
    violations: &mut Vec<String>,
) -> Result<(), String> {
    let path = root.join(".config/nextest-inventories.toml");
    let inventories: NextestInventories = read_toml(&path)?;
    if inventories.version != NEXTEST_INVENTORY_VERSION {
        violations.push(format!(
            "{} uses version {}; expected {NEXTEST_INVENTORY_VERSION}",
            path.display(),
            inventories.version
        ));
    }
    let mut registered = BTreeMap::<String, BTreeSet<String>>::new();
    for shape in &inventories.shape {
        if shape.command.trim().is_empty() || !shape.command.contains("cargo nextest list") {
            violations.push(format!(
                "nextest shape `{}` must preserve its exact enumeration command",
                shape.name
            ));
        }
        let identities = exact_identity_vector(
            &shape.identities,
            &format!("nextest shape `{}`", shape.name),
            violations,
        );
        if identities.is_empty() {
            violations.push(format!("nextest shape `{}` has no identities", shape.name));
        }
        if registered.insert(shape.name.clone(), identities).is_some() {
            violations.push(format!("nextest shape `{}` is duplicated", shape.name));
        }
    }
    for required in [
        "default",
        "decode",
        "detect",
        "combined",
        "fabric",
        "production",
        "slow",
    ] {
        if !registered.contains_key(required) {
            violations.push(format!(
                "authoritative nextest registry is missing required feature shape `{required}`"
            ));
        }
    }

    let mut checked = BTreeSet::new();
    for (name, json_path) in observed_json {
        if !checked.insert(name.clone()) {
            violations.push(format!(
                "nextest shape `{name}` was supplied more than once"
            ));
            continue;
        }
        let Some(expected) = registered.get(name) else {
            violations.push(format!("CI supplied unregistered nextest shape `{name}`"));
            continue;
        };
        let observed = parse_nextest_json(json_path)?;
        if &observed != expected {
            let added = observed.difference(expected).cloned().collect::<Vec<_>>();
            let missing = expected.difference(&observed).cloned().collect::<Vec<_>>();
            violations.push(format!(
                "nextest shape `{name}` changed; added={added:?}, missing={missing:?}"
            ));
        }
        println!(
            "authoritative nextest cross-check: {} exact identities for `{name}`",
            observed.len()
        );
    }
    println!(
        "nextest inventories checked: {} frozen feature shapes",
        registered.len()
    );
    Ok(())
}

fn parse_nextest_json(path: &Path) -> Result<BTreeSet<String>, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("read nextest JSON {}: {error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|error| format!("parse nextest JSON {}: {error}", path.display()))?;
    let suites = value
        .get("rust-suites")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| format!("nextest JSON {} has no rust-suites object", path.display()))?;
    let mut identities = BTreeSet::new();
    for suite in suites.values() {
        let package = suite
            .get("package-name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("nextest JSON {} suite has no package-name", path.display()))?;
        let binary = suite
            .get("binary-name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("nextest JSON {} suite has no binary-name", path.display()))?;
        let is_test_binary = suite.get("kind").and_then(serde_json::Value::as_str) == Some("test");
        let testcases = suite
            .get("testcases")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| format!("nextest JSON {} suite has no testcases", path.display()))?;
        for test in testcases.keys() {
            let identity = if is_test_binary {
                format!("{package}::{binary}::{test}")
            } else {
                format!("{package}::{test}")
            };
            identities.insert(identity);
        }
    }
    Ok(identities)
}

fn audit_non_rust_contracts(root: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    let path = root.join(".config/non-rust-test-contracts.toml");
    let baseline_path = root.join(".config/non-rust-test-baseline.toml");
    let receipts_path = root.join(".config/non-rust-test-receipts.toml");
    let contracts: NonRustContracts = read_toml(&path)?;
    let baseline: NonRustBaseline = read_toml(&baseline_path)?;
    let receipts: NonRustReceipts = read_toml(&receipts_path)?;
    if contracts.version != NON_RUST_CONTRACT_VERSION {
        violations.push(format!(
            "{} uses version {}; expected {NON_RUST_CONTRACT_VERSION}",
            path.display(),
            contracts.version
        ));
    }
    if baseline.version != NON_RUST_BASELINE_VERSION {
        violations.push(format!(
            "{} uses version {}; expected {NON_RUST_BASELINE_VERSION}",
            baseline_path.display(),
            baseline.version
        ));
    }
    if receipts.version != NON_RUST_BASELINE_VERSION {
        violations.push(format!(
            "{} uses version {}; expected {NON_RUST_BASELINE_VERSION}",
            receipts_path.display(),
            receipts.version
        ));
    }
    let discovered = discover_non_rust_cases(root)?;
    let rust_records = collect_test_records(root)?;
    let mut registered = BTreeMap::<String, &NonRustCase>::new();
    let mut physical_not_run = 0usize;
    for case in &contracts.case {
        if registered.insert(case.id.clone(), case).is_some() {
            violations.push(format!(
                "non-Rust acceptance identity `{}` is duplicated",
                case.id
            ));
        }
        if !matches!(case.gate, TestGate::Nightly | TestGate::Physical) {
            violations.push(format!(
                "non-Rust acceptance identity `{}` must be classified nightly or physical",
                case.id
            ));
        }
        if !valid_review_attestation(&case.lineage) {
            violations.push(format!(
                "non-Rust acceptance identity `{}` has no concrete source/lineage",
                case.id
            ));
        }
        if !valid_completed_review_attestation(&case.reviewed_by) {
            violations.push(format!(
                "non-Rust acceptance identity `{}` has no completed independent physical-to-deterministic mapping review",
                case.id
            ));
        }
        if case.explicit_run.trim().is_empty()
            || !case.explicit_run.contains(&case.id)
            || case.explicit_run.contains("TODO")
        {
            violations.push(format!(
                "non-Rust acceptance identity `{}` has no exact explicit-run command",
                case.id
            ));
        }
        match case.status {
            NonRustCaseStatus::Active => {
                if case.blocker_reason.is_some() {
                    violations.push(format!(
                        "active non-Rust acceptance identity `{}` must not carry a physical blocker reason",
                        case.id
                    ));
                }
            }
            NonRustCaseStatus::ImplementationBlockedPhysical => {
                let reason = case.blocker_reason.as_deref().unwrap_or("").trim();
                if !valid_reason(reason) {
                    violations.push(format!(
                        "non-Rust acceptance identity `{}` has no exact physical blocker reason",
                        case.id
                    ));
                }
                physical_not_run += 1;
                eprintln!(
                    "non-Rust acceptance identity `{}` is NOT-RUN because its physical proof is implementation-blocked: {}",
                    case.id,
                    if reason.is_empty() {
                        "missing reason"
                    } else {
                        reason
                    }
                );
            }
            NonRustCaseStatus::StaleRejectedContract => violations.push(format!(
                "non-Rust acceptance identity `{}` still enforces a rejected product contract: {}",
                case.id, case.lineage
            )),
            NonRustCaseStatus::FalseGreenPhysicalSkip => violations.push(format!(
                "non-Rust acceptance identity `{}` can report a false physical PASS: {}",
                case.id, case.lineage
            )),
        }
        if !matches!(case.status, NonRustCaseStatus::Active)
            && !case.deterministic_witnesses.is_empty()
        {
            violations.push(format!(
                "non-active physical identity `{}` must not name a nearby deterministic test as a substitute",
                case.id
            ));
        }
        validate_non_rust_witnesses(case, &rust_records, violations);
        match discovered.get(&case.id) {
            Some((source, handler)) if source == &case.source && handler == &case.handler => {}
            Some((source, handler)) => violations.push(format!(
                "non-Rust acceptance identity `{}` moved: registry={}:{} observed={source}:{handler}",
                case.id, case.source, case.handler
            )),
            None => violations.push(format!(
                "registered non-Rust acceptance identity `{}` is stale or missing",
                case.id
            )),
        }
    }
    for (id, (source, handler)) in &discovered {
        if !registered.contains_key(id) {
            violations.push(format!(
                "non-Rust acceptance identity `{id}` at {source}:{handler} is unregistered"
            ));
        }
    }

    let observed_vector = contracts
        .case
        .iter()
        .map(|case| format!("{}|{}", case.id, case.status.as_str()))
        .collect::<BTreeSet<_>>();
    audit_non_rust_transitions(&baseline, &receipts, &observed_vector, violations);

    let mut assets = BTreeSet::new();
    for asset in &contracts.asset {
        if !assets.insert(asset.path.clone()) {
            violations.push(format!(
                "non-Rust acceptance asset `{}` is duplicated",
                asset.path
            ));
        }
        if !valid_completed_review_attestation(&asset.lineage) || !is_sha256(&asset.sha256) {
            violations.push(format!(
                "non-Rust acceptance asset `{}` has invalid lineage or SHA-256",
                asset.path
            ));
        }
        let asset_path = root.join(&asset.path);
        let bytes = fs::read(&asset_path)
            .map_err(|error| format!("read non-Rust asset {}: {error}", asset_path.display()))?;
        let digest = syntax_fingerprint_bytes(&bytes);
        if digest != asset.sha256 {
            violations.push(format!(
                "non-Rust acceptance asset `{}` changed: registry={}, observed={digest}",
                asset.path, asset.sha256
            ));
        }
        match asset.kind {
            NonRustAssetKind::BrowserHarness
                if asset_path.extension().and_then(|value| value.to_str()) != Some("html") =>
            {
                violations.push(format!(
                    "browser harness asset `{}` must be an HTML file",
                    asset.path
                ));
            }
            NonRustAssetKind::TraceImage
                if asset_path.extension().and_then(|value| value.to_str())
                    != Some("Dockerfile") =>
            {
                violations.push(format!(
                    "trace image asset `{}` must be a Dockerfile",
                    asset.path
                ));
            }
            _ => {}
        }
    }
    let haos = root.join("tests/ha-os-vm");
    for entry in fs::read_dir(&haos).map_err(|error| format!("read {}: {error}", haos.display()))? {
        let path = entry
            .map_err(|error| format!("read HA-OS asset entry: {error}"))?
            .path();
        if path.is_file() && haos_asset_requires_frozen_digest(&path) {
            let rel = relative(root, &path)?;
            if !assets.contains(&rel) {
                violations.push(format!(
                    "new HA-OS executable acceptance asset `{rel}` is not frozen in the non-Rust registry"
                ));
            }
        }
    }
    println!(
        "non-Rust contracts checked: {} named cases, {} frozen assets, {} honest physical NOT-RUN receipts",
        discovered.len(),
        assets.len(),
        physical_not_run
    );
    Ok(())
}

fn audit_non_rust_transitions(
    baseline: &NonRustBaseline,
    receipts: &NonRustReceipts,
    observed: &BTreeSet<String>,
    violations: &mut Vec<String>,
) {
    let baseline_set =
        exact_identity_vector(&baseline.cases, "non-Rust frozen baseline", violations);
    for entry in &baseline_set {
        if parse_non_rust_vector_entry(entry).is_none() {
            violations.push(format!(
                "non-Rust frozen baseline entry `{entry}` must be an exact ID|status pair"
            ));
        }
    }
    let baseline_digest = identity_vector_digest(&baseline_set);
    if baseline.vector_sha256 != baseline_digest {
        violations.push(format!(
            "non-Rust frozen baseline says {}, but its exact sorted vector digests to {baseline_digest}",
            baseline.vector_sha256
        ));
    }
    if baseline.vector_sha256 != FROZEN_NON_RUST_VECTOR_SHA256 {
        violations.push(format!(
            "non-Rust frozen baseline digest changed: file says {}, compiled guard requires {FROZEN_NON_RUST_VECTOR_SHA256}",
            baseline.vector_sha256
        ));
    }
    if baseline_set == *observed {
        if !receipts.transition.is_empty() {
            violations.push("non-Rust transition receipts exist although the observed vector equals the frozen baseline".to_string());
        }
        return;
    }

    let mut current = baseline_set;
    let mut current_digest = baseline.vector_sha256.clone();
    let mut receipt_ids = BTreeSet::new();
    for receipt in &receipts.transition {
        if !receipt_ids.insert(receipt.id.clone()) {
            violations.push(format!(
                "non-Rust transition receipt `{}` is duplicated",
                receipt.id
            ));
        }
        if receipt.before_vector_sha256 != current_digest {
            violations.push(format!(
                "non-Rust transition receipt `{}` starts at {}, but the exact prior vector is {current_digest}",
                receipt.id, receipt.before_vector_sha256
            ));
        }
        let removed = exact_receipt_list(
            &receipt.removed_cases,
            "removed_cases",
            &receipt.id,
            violations,
        );
        let added =
            exact_receipt_list(&receipt.added_cases, "added_cases", &receipt.id, violations);
        let changes = receipt
            .status_changes
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if changes.len() != receipt.status_changes.len()
            || receipt.status_changes.iter().ne(changes.iter())
        {
            violations.push(format!(
                "non-Rust transition receipt `{}` status_changes must be unique and bytewise sorted",
                receipt.id
            ));
        }
        let mut changed_ids = BTreeSet::new();
        for change in &changes {
            if change.before == change.after || !changed_ids.insert(change.id.clone()) {
                violations.push(format!(
                    "non-Rust transition receipt `{}` repeats or does not change status for `{}`",
                    receipt.id, change.id
                ));
            }
        }
        let added_ids = added
            .iter()
            .filter_map(|entry| parse_non_rust_vector_entry(entry).map(|(id, _)| id.to_string()))
            .collect::<BTreeSet<_>>();
        if added_ids.len() != added.len() {
            violations.push(format!(
                "non-Rust transition receipt `{}` added_cases must be exact ID|status pairs",
                receipt.id
            ));
        }
        if !removed.is_disjoint(&changed_ids)
            || !removed.is_disjoint(&added_ids)
            || !changed_ids.is_disjoint(&added_ids)
        {
            violations.push(format!(
                "non-Rust transition receipt `{}` overlaps removal, addition, or status-change identities",
                receipt.id
            ));
        }
        let current_by_id = current
            .iter()
            .filter_map(|entry| parse_non_rust_vector_entry(entry))
            .map(|(id, status)| (id.to_string(), status.to_string()))
            .collect::<BTreeMap<_, _>>();
        for id in &removed {
            let Some(status) = current_by_id.get(id) else {
                violations.push(format!(
                    "non-Rust transition receipt `{}` removes absent identity `{id}`",
                    receipt.id
                ));
                continue;
            };
            current.remove(&format!("{id}|{status}"));
        }
        for change in &changes {
            let before = change.before.as_str();
            if current_by_id.get(&change.id).map(String::as_str) != Some(before) {
                violations.push(format!(
                    "non-Rust transition receipt `{}` changes `{}` from {}, but that is not its exact prior status",
                    receipt.id, change.id, before
                ));
                continue;
            }
            current.remove(&format!("{}|{before}", change.id));
            current.insert(format!("{}|{}", change.id, change.after.as_str()));
        }
        for entry in &added {
            if let Some((id, _)) = parse_non_rust_vector_entry(entry) {
                if current_by_id.contains_key(id) {
                    violations.push(format!(
                        "non-Rust transition receipt `{}` adds existing identity `{id}`",
                        receipt.id
                    ));
                } else {
                    current.insert(entry.clone());
                }
            }
        }
        let after_digest = identity_vector_digest(&current);
        if receipt.after_vector_sha256 != after_digest {
            violations.push(format!(
                "non-Rust transition receipt `{}` after digest is {}, but its exact vector digests to {after_digest}",
                receipt.id, receipt.after_vector_sha256
            ));
        }
        for (field, value) in [
            ("zero_lost_detection", &receipt.zero_lost_detection),
            ("rationale", &receipt.rationale),
        ] {
            if !valid_review_attestation(value) {
                violations.push(format!(
                    "non-Rust transition receipt `{}` has no reviewed {field}",
                    receipt.id
                ));
            }
        }
        if !valid_completed_review_attestation(&receipt.reviewer) {
            violations.push(format!(
                "non-Rust transition receipt `{}` has no completed independent reviewer attestation",
                receipt.id
            ));
        }
        audit_non_rust_transition_proofs(receipt, &removed, &changed_ids, violations);
        current_digest = after_digest;
    }
    if current != *observed || current_digest != identity_vector_digest(observed) {
        violations.push(format!(
            "non-Rust registry changed from its frozen vector without a complete reviewed receipt chain; chain ends at {current_digest}, observed vector is {}",
            identity_vector_digest(observed)
        ));
    }
}

fn parse_non_rust_vector_entry(value: &str) -> Option<(&str, &str)> {
    let (id, status) = value.split_once('|')?;
    if id.is_empty()
        || status.is_empty()
        || status.contains('|')
        || !matches!(
            status,
            "active"
                | "implementation-blocked-physical"
                | "stale-rejected-contract"
                | "false-green-physical-skip"
        )
    {
        return None;
    }
    Some((id, status))
}

fn audit_non_rust_transition_proofs(
    receipt: &NonRustTransitionReceipt,
    removed: &BTreeSet<String>,
    changed: &BTreeSet<String>,
    violations: &mut Vec<String>,
) {
    let required = removed.union(changed).cloned().collect::<BTreeSet<_>>();
    let mut covered = BTreeSet::new();
    for proof in &receipt.proof {
        let NonRustTransitionProof::OwnerRuling {
            cases,
            ruling,
            reviewer,
        } = proof;
        if !valid_review_attestation(ruling) || !valid_completed_review_attestation(reviewer) {
            violations.push(format!(
                "non-Rust transition receipt `{}` has an owner-ruling proof without a concrete ruling and completed independent review",
                receipt.id
            ));
        }
        let proof_cases = exact_receipt_list(cases, "proof cases", &receipt.id, violations);
        for id in proof_cases {
            if !required.contains(&id) || !covered.insert(id.clone()) {
                violations.push(format!(
                    "non-Rust transition receipt `{}` has duplicate or unrelated proof for `{id}`",
                    receipt.id
                ));
            }
        }
    }
    if covered != required {
        let missing = required.difference(&covered).cloned().collect::<Vec<_>>();
        violations.push(format!(
            "non-Rust transition receipt `{}` has removals or status changes without machine-checked owner proof: {missing:?}",
            receipt.id
        ));
    }
}

fn validate_non_rust_witnesses(
    case: &NonRustCase,
    rust_records: &[TestRecord],
    violations: &mut Vec<String>,
) {
    if case.deterministic_witnesses.is_empty() && matches!(case.status, NonRustCaseStatus::Active) {
        violations.push(format!(
            "non-Rust acceptance identity `{}` has no deterministic witnesses",
            case.id
        ));
    }
    let mut unique_witnesses = BTreeSet::new();
    for witness_id in &case.deterministic_witnesses {
        if !unique_witnesses.insert(witness_id) {
            violations.push(format!(
                "non-Rust acceptance identity `{}` repeats deterministic witness `{witness_id}`",
                case.id
            ));
        }
        let Some(witness) = rust_records
            .iter()
            .find(|record| record.identity == *witness_id)
        else {
            violations.push(format!(
                "non-Rust acceptance identity `{}` names missing deterministic witness `{witness_id}`",
                case.id
            ));
            continue;
        };
        if witness.ignored {
            violations.push(format!(
                "non-Rust acceptance identity `{}` uses ignored deterministic witness `{witness_id}`",
                case.id
            ));
        }
    }
}

fn haos_asset_requires_frozen_digest(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("sh" | "html" | "Dockerfile")
    )
}

fn discover_non_rust_cases(root: &Path) -> Result<BTreeMap<String, (String, String)>, String> {
    let mut cases = BTreeMap::new();
    let suite_rel = "tests/ha-os-vm/run-th-suite.sh";
    let suite = fs::read_to_string(root.join(suite_rel))
        .map_err(|error| format!("read {suite_rel}: {error}"))?;
    validate_physical_correction_topic(&suite, suite_rel)?;
    validate_th23_correction_readback(&suite, suite_rel)?;
    let lines = suite.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let Some(handler) = trimmed.strip_suffix("() {") else {
            continue;
        };
        if handler.len() != 4
            || !handler.starts_with("th")
            || !handler[2..].chars().all(|ch| ch.is_ascii_digit())
        {
            continue;
        }
        let expected = format!("TH-{}", &handler[2..]);
        let nearby = lines[index + 1..lines.len().min(index + 6)].join("\n");
        if !nearby.contains(&format!("local id=\"{expected}\"")) {
            return Err(format!(
                "{suite_rel}:{} handler {handler} does not bind exact identity {expected}",
                index + 1
            ));
        }
        let dispatch = format!(") {handler} ;;");
        if !suite.lines().any(|line| line.trim().contains(&dispatch)) {
            return Err(format!(
                "{suite_rel} defines {handler}/{expected} without a run-list dispatch arm"
            ));
        }
        if cases
            .insert(
                expected.clone(),
                (suite_rel.to_string(), handler.to_string()),
            )
            .is_some()
        {
            return Err(format!("duplicate non-Rust identity {expected}"));
        }
    }

    let bootstrap_rel = "tests/ha-os-vm/bootstrap.sh";
    let bootstrap = fs::read_to_string(root.join(bootstrap_rel))
        .map_err(|error| format!("read {bootstrap_rel}: {error}"))?;
    if bootstrap.lines().any(|line| line.trim() == "id=\"TH-00\"") {
        cases.insert(
            "TH-00".to_string(),
            (bootstrap_rel.to_string(), "script".to_string()),
        );
    }

    let smoke_rel = "tests/ha-os-vm/ha-integration-smoke.sh";
    let smoke = fs::read_to_string(root.join(smoke_rel))
        .map_err(|error| format!("read {smoke_rel}: {error}"))?;
    validate_physical_correction_topic(&smoke, smoke_rel)?;
    for number in 1..=7 {
        let id = format!("HA-S{number}");
        if !smoke.contains(&format!("[{id}]")) {
            return Err(format!("{smoke_rel} is missing named physical step {id}"));
        }
        if cases
            .insert(id.clone(), (smoke_rel.to_string(), "section".to_string()))
            .is_some()
        {
            return Err(format!("duplicate non-Rust identity {id}"));
        }
    }
    validate_ha_smoke_fail_closed(&smoke, smoke_rel)?;

    Ok(cases)
}

fn validate_physical_correction_topic(source: &str, source_name: &str) -> Result<(), String> {
    let expected = "vigil_correction_topic=\"${VIGIL_CORRECTION_TOPIC:-vigil/commands/correct}\"";
    if !source.contains(expected) || source.contains("vigil/correction/command") {
        return Err(format!(
            "{source_name} must default correction acceptance to production topic vigil/commands/correct"
        ));
    }
    Ok(())
}

fn bounded_shell_section<'a>(
    source: &'a str,
    start: &str,
    end: &str,
    source_name: &str,
) -> Result<&'a str, String> {
    let after_start = source
        .split_once(start)
        .map(|(_, rest)| rest)
        .ok_or_else(|| format!("{source_name} is missing section boundary `{start}`"))?;
    after_start
        .split_once(end)
        .map(|(section, _)| section)
        .ok_or_else(|| format!("{source_name} is missing section boundary `{end}`"))
}

fn validate_th23_correction_readback(source: &str, source_name: &str) -> Result<(), String> {
    let th23 = bounded_shell_section(source, "th23() {", "\nth24() {", source_name)?;
    for required in [
        "curl_run -fsS --max-time 5",
        r#"${review_url%/}/why/$detection_id"#,
        ".label == $label",
        r#".correction_type == "FalseAlarm""#,
        ".anchored_detection_id == $detection_id",
    ] {
        if !th23.contains(required) {
            return Err(format!(
                "{source_name} TH-23 must validate exact correction JSON through the shipped /why route; missing `{required}`"
            ));
        }
    }
    if th23.contains("vigil_exec_cmd vigil why") {
        return Err(format!(
            "{source_name} TH-23 must not use the CLI formatter to validate corrections; query /why/<detection_id> JSON"
        ));
    }
    Ok(())
}

fn validate_ha_smoke_fail_closed(smoke: &str, smoke_rel: &str) -> Result<(), String> {
    if smoke.contains("skip_step") || smoke.contains("HA-S%s SKIP") {
        return Err(format!(
            "{smoke_rel} must not report required physical steps as SKIP"
        ));
    }
    for required in [
        "HA-S%s NOT-RUN",
        "status=1",
        "HA_S4_MANUAL_PROOF",
        "printf -v quoted '%q ' \"$@\"",
        "command -v curl >/dev/null 2>&1",
        "pass_step 7 \"the exact anchored correction object read back from cg through /why JSON",
    ] {
        if !smoke.contains(required) {
            return Err(format!(
                "{smoke_rel} is missing fail-closed physical-suite contract `{required}`"
            ));
        }
    }
    for forbidden in ["ss -tnp", "ESTAB", "nothing left the property", "\"$*\""] {
        if smoke.contains(forbidden) {
            return Err(format!(
                "{smoke_rel} must leave physical correction egress to TH-23, not `{forbidden}` polling"
            ));
        }
    }

    let ha_s7 = bounded_shell_section(smoke, "# ─── HA-S7:", "# ─── Summary", smoke_rel)?;
    for required in [
        "curl_ha --max-time 5",
        r#"${review_url%/}/why/$smoke_detection_id"#,
        ".label == $label",
        r#".correction_type == "FalseAlarm""#,
        ".anchored_detection_id == $detection_id",
    ] {
        if !ha_s7.contains(required) {
            return Err(format!(
                "{smoke_rel} HA-S7 must validate exact correction JSON through the shipped /why route; missing `{required}`"
            ));
        }
    }
    if ha_s7.contains("vigil_exec vigil why") {
        return Err(format!(
            "{smoke_rel} HA-S7 must not use the CLI formatter to validate corrections; query /why/<detection_id> JSON"
        ));
    }

    Ok(())
}

fn syntax_fingerprint_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

fn exact_identity_vector(
    tests: &[String],
    label: &str,
    violations: &mut Vec<String>,
) -> BTreeSet<String> {
    let set = tests.iter().cloned().collect::<BTreeSet<_>>();
    if set.len() != tests.len() {
        violations.push(format!("{label} repeats one or more test identities"));
    }
    if tests.iter().ne(set.iter()) {
        violations.push(format!("{label} identities must be bytewise sorted"));
    }
    set
}

fn identity_vector_digest(tests: &BTreeSet<String>) -> String {
    let mut vector = String::new();
    for test in tests {
        vector.push_str(test);
        vector.push('\n');
    }
    syntax_fingerprint(&vector)
}

fn valid_contract_id(id: &str) -> bool {
    id.len() >= 8
        && id.len() <= 120
        && id.split('-').count() >= 2
        && id.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        })
}

fn validate_physical_contract(
    contract: &TestContract,
    records: &[TestRecord],
    violations: &mut Vec<String>,
) {
    if !contract.gate.requires_witness() {
        if contract.deterministic_witness.is_some() || contract.explicit_run.is_some() {
            violations.push(format!(
                "ordinary test contract `{}` must not masquerade as a physical/nightly receipt",
                contract.id
            ));
        }
        return;
    }
    if contract.evidence_kind != TestEvidenceKind::Physical {
        violations.push(format!(
            "physical/nightly contract `{}` must use physical evidence_kind",
            contract.id
        ));
    }
    let Some(witness) = contract.deterministic_witness.as_deref() else {
        violations.push(format!(
            "physical/nightly contract `{}` has no deterministic per-change witness",
            contract.id
        ));
        return;
    };
    let Some(witness_record) = records.iter().find(|record| record.identity == witness) else {
        violations.push(format!(
            "physical/nightly contract `{}` names missing witness `{witness}`",
            contract.id
        ));
        return;
    };
    if witness_record.ignored {
        violations.push(format!(
            "physical/nightly contract `{}` uses ignored witness `{witness}`; the witness must run per change",
            contract.id
        ));
    }
    let Some(command) = contract.explicit_run.as_deref() else {
        violations.push(format!(
            "physical/nightly contract `{}` has no exact explicit-run command",
            contract.id
        ));
        return;
    };
    let physical_leafs = contract
        .tests
        .iter()
        .filter_map(|test| test.rsplit("::").next())
        .collect::<Vec<_>>();
    if !command.contains("--ignored")
        || !command.contains("--exact")
        || !physical_leafs.iter().any(|leaf| command.contains(*leaf))
    {
        violations.push(format!(
            "physical/nightly contract `{}` explicit-run command must select its named test exactly with --ignored --exact",
            contract.id
        ));
    }
}

fn audit_test_transitions(
    root: &Path,
    baseline: &TestBaseline,
    baseline_set: &BTreeSet<String>,
    active: &BTreeSet<String>,
    receipts: &TestReceipts,
    violations: &mut Vec<String>,
) {
    let active_digest = identity_vector_digest(active);
    if baseline_set == active {
        if !receipts.transition.is_empty() {
            violations.push("test transition receipts exist although the active vector equals the frozen baseline".to_string());
        }
        return;
    }
    let mut current = baseline_set.clone();
    let mut current_digest = baseline.vector_sha256.clone();
    let mut seen_ids = BTreeSet::new();
    for receipt in &receipts.transition {
        if !seen_ids.insert(receipt.id.clone()) {
            violations.push(format!(
                "test transition receipt `{}` is duplicated",
                receipt.id
            ));
        }
        if receipt.base_sha != FROZEN_TEST_BASE_SHA {
            violations.push(format!(
                "test transition receipt `{}` names base SHA {}; expected {FROZEN_TEST_BASE_SHA}",
                receipt.id, receipt.base_sha
            ));
        }
        if receipt.before_vector_sha256 != current_digest {
            violations.push(format!(
                "test transition receipt `{}` starts at {}, but the exact prior vector is {current_digest}",
                receipt.id, receipt.before_vector_sha256
            ));
        }
        let removed = exact_receipt_list(
            &receipt.removed_tests,
            "removed_tests",
            &receipt.id,
            violations,
        );
        let added =
            exact_receipt_list(&receipt.added_tests, "added_tests", &receipt.id, violations);
        let survivors = exact_receipt_list(
            &receipt.surviving_tests,
            "surviving_tests",
            &receipt.id,
            violations,
        );
        let actual_survivors = current
            .difference(&removed)
            .cloned()
            .collect::<BTreeSet<_>>();
        let additive_inherits_prior_vector = removed.is_empty() && survivors.is_empty();
        if !additive_inherits_prior_vector && survivors != actual_survivors {
            violations.push(format!(
                "test transition receipt `{}` does not enumerate the exact surviving vector",
                receipt.id
            ));
        }
        if !removed.is_subset(&current) || !added.is_disjoint(&current) {
            violations.push(format!(
                "test transition receipt `{}` removes an absent ID or adds an existing ID",
                receipt.id
            ));
        }
        current = actual_survivors.union(&added).cloned().collect();
        let after_digest = identity_vector_digest(&current);
        if receipt.after_vector_sha256 != after_digest {
            violations.push(format!(
                "test transition receipt `{}` after digest is {}, but its exact vector digests to {after_digest}",
                receipt.id, receipt.after_vector_sha256
            ));
        }
        for (field, value) in [
            ("zero_lost_detection", &receipt.zero_lost_detection),
            ("rationale", &receipt.rationale),
        ] {
            if !valid_review_attestation(value) {
                violations.push(format!(
                    "test transition receipt `{}` has no reviewed {field}",
                    receipt.id
                ));
            }
        }
        if !valid_completed_review_attestation(&receipt.reviewer) {
            violations.push(format!(
                "test transition receipt `{}` is executable inventory but has no completed independent reviewer attestation",
                receipt.id
            ));
        }
        audit_transition_proofs(root, receipt, &removed, violations);
        current_digest = after_digest;
    }
    if current != *active || current_digest != active_digest {
        violations.push(format!(
            "test registry changed from the frozen vector without a complete reviewed receipt chain; receipt chain ends at {current_digest}, active vector is {active_digest}"
        ));
    }
}

fn exact_receipt_list(
    values: &[String],
    field: &str,
    receipt: &str,
    violations: &mut Vec<String>,
) -> BTreeSet<String> {
    let set = values.iter().cloned().collect::<BTreeSet<_>>();
    if set.len() != values.len() || values.iter().ne(set.iter()) {
        violations.push(format!(
            "test transition receipt `{receipt}` {field} must be unique and bytewise sorted"
        ));
    }
    set
}

fn audit_transition_proofs(
    root: &Path,
    receipt: &TestTransitionReceipt,
    removed: &BTreeSet<String>,
    violations: &mut Vec<String>,
) {
    let mut covered = BTreeSet::new();
    for proof in &receipt.proof {
        let (proof_removed, reviewer) = match proof {
            TestTransitionProof::OwnerRuling {
                removed_tests,
                ruling,
                reviewer,
            } => {
                if !valid_review_attestation(ruling) {
                    violations.push(format!(
                        "test transition receipt `{}` has an owner-ruling proof without a concrete ruling",
                        receipt.id
                    ));
                }
                (removed_tests, reviewer)
            }
            TestTransitionProof::MutationComparison {
                removed_tests,
                before_outcomes,
                before_sha256,
                after_outcomes,
                after_sha256,
                mutants,
                mutants_sha256,
                reviewer,
            } => {
                audit_mutation_comparison(
                    root,
                    &receipt.id,
                    before_outcomes,
                    before_sha256,
                    after_outcomes,
                    after_sha256,
                    mutants,
                    mutants_sha256,
                    violations,
                );
                (removed_tests, reviewer)
            }
            TestTransitionProof::RedundantFaultDetection {
                removed_tests,
                surviving_test,
                evidence,
                evidence_sha256,
                reviewer,
            } => {
                audit_redundant_fault_detection(
                    root,
                    &receipt.id,
                    removed_tests,
                    surviving_test,
                    evidence,
                    evidence_sha256,
                    violations,
                );
                (removed_tests, reviewer)
            }
        };
        if !valid_completed_review_attestation(reviewer) {
            violations.push(format!(
                "test transition receipt `{}` has a proof without a completed independent reviewer attestation",
                receipt.id
            ));
        }
        let proof_set = exact_receipt_list(
            proof_removed,
            "proof removed_tests",
            &receipt.id,
            violations,
        );
        for test in proof_set {
            if !removed.contains(&test) {
                violations.push(format!(
                    "test transition receipt `{}` proof covers `{test}`, which the receipt does not remove",
                    receipt.id
                ));
            }
            if !covered.insert(test.clone()) {
                violations.push(format!(
                    "test transition receipt `{}` removed test `{test}` is covered by more than one proof",
                    receipt.id
                ));
            }
        }
    }
    if covered != *removed {
        let missing = removed.difference(&covered).cloned().collect::<Vec<_>>();
        violations.push(format!(
            "test transition receipt `{}` has removed tests without machine-checked proof: {missing:?}",
            receipt.id
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn audit_mutation_comparison(
    root: &Path,
    receipt_id: &str,
    before_path: &str,
    before_sha256: &str,
    after_path: &str,
    after_sha256: &str,
    mutants_path: &str,
    mutants_sha256: &str,
    violations: &mut Vec<String>,
) {
    let result = (|| -> Result<(), String> {
        let before = checked_evidence_bytes(root, before_path, before_sha256)?;
        let after = checked_evidence_bytes(root, after_path, after_sha256)?;
        let mutants = checked_evidence_bytes(root, mutants_path, mutants_sha256)?;
        let before = mutation_outcomes(&before)?;
        let after = mutation_outcomes(&after)?;
        let manifest = mutation_manifest_names(&mutants)?;
        if before.keys().cloned().collect::<BTreeSet<_>>() != manifest
            || after.keys().cloned().collect::<BTreeSet<_>>() != manifest
        {
            return Err("mutation manifest and before/after outcome identities differ".to_string());
        }
        if !before.values().any(|outcome| outcome == "CaughtMutant") {
            return Err("mutation comparison contains no caught mutant".to_string());
        }
        for (name, before_outcome) in &before {
            let after_outcome = &after[name];
            if before_outcome == "CaughtMutant" && after_outcome != "CaughtMutant" {
                return Err(format!(
                    "caught mutant became {after_outcome} after consolidation: {name}"
                ));
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        violations.push(format!(
            "test transition receipt `{receipt_id}` mutation proof is invalid: {error}"
        ));
    }
}

fn audit_redundant_fault_detection(
    root: &Path,
    receipt_id: &str,
    removed_tests: &[String],
    surviving_test: &str,
    evidence_path: &str,
    evidence_sha256: &str,
    violations: &mut Vec<String>,
) {
    let result = (|| -> Result<(), String> {
        if removed_tests.len() != 1 {
            return Err("redundant-fault proof must cover exactly one removed test".to_string());
        }
        let bytes = checked_evidence_bytes(root, evidence_path, evidence_sha256)?;
        let evidence: RedundantFaultEvidence = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse {evidence_path}: {error}"))?;
        if evidence.version != 1 {
            return Err(format!("evidence version {} is not 1", evidence.version));
        }
        if !valid_review_attestation(&evidence.planted_fault.id)
            || !is_sha256(&evidence.planted_fault.source_sha256)
        {
            return Err("planted fault lacks a concrete ID or source hash".to_string());
        }
        checked_evidence_bytes(
            root,
            &evidence.planted_fault.diff,
            &evidence.planted_fault.diff_sha256,
        )?;
        if evidence.removed.test != removed_tests[0] {
            return Err("evidence removed-test identity does not match its receipt".to_string());
        }
        if evidence.survivor.test != surviving_test {
            return Err("evidence survivor identity does not match its receipt".to_string());
        }
        for (role, run) in [
            ("removed", &evidence.removed),
            ("survivor", &evidence.survivor),
        ] {
            if run.outcome != "CAUGHT" || run.exit_code == 0 {
                return Err(format!(
                    "{role} test did not catch the planted fault with a nonzero exit"
                ));
            }
            if !run.duration_seconds.is_finite()
                || run.duration_seconds <= 0.0
                || !run.command.contains("--exact")
                || !run
                    .command
                    .contains(run.test.rsplit("::").next().unwrap_or(&run.test))
            {
                return Err(format!(
                    "{role} test run lacks an exact bounded command receipt"
                ));
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        violations.push(format!(
            "test transition receipt `{receipt_id}` redundant-fault proof is invalid: {error}"
        ));
    }
}

fn checked_evidence_bytes(root: &Path, relative: &str, expected: &str) -> Result<Vec<u8>, String> {
    let path = Path::new(relative);
    if path.is_absolute()
        || !relative.starts_with(".config/test-estate-evidence/")
        || path.components().any(|component| {
            !matches!(component, std::path::Component::Normal(_))
                && !matches!(component, std::path::Component::CurDir)
        })
    {
        return Err(format!(
            "evidence path `{relative}` is outside the guarded evidence tree"
        ));
    }
    if !is_sha256(expected) {
        return Err(format!(
            "evidence path `{relative}` has an invalid SHA-256 receipt"
        ));
    }
    let bytes = fs::read(root.join(path))
        .map_err(|error| format!("read evidence path `{relative}`: {error}"))?;
    let actual = syntax_fingerprint_bytes(&bytes);
    if actual != expected {
        return Err(format!(
            "evidence path `{relative}` hashes to {actual}, expected {expected}"
        ));
    }
    Ok(bytes)
}

fn mutation_outcomes(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("parse mutation outcomes: {error}"))?;
    let outcomes = value
        .get("outcomes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "mutation outcomes has no outcomes array".to_string())?;
    let baseline_ok = outcomes.iter().any(|entry| {
        entry.get("scenario").and_then(serde_json::Value::as_str) == Some("Baseline")
            && entry.get("summary").and_then(serde_json::Value::as_str) == Some("Success")
    });
    if !baseline_ok {
        return Err("mutation baseline did not succeed".to_string());
    }
    let mut result = BTreeMap::new();
    for entry in outcomes {
        let Some(name) = entry
            .pointer("/scenario/Mutant/name")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let summary = entry
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("mutation `{name}` has no string summary"))?;
        if result
            .insert(name.to_string(), summary.to_string())
            .is_some()
        {
            return Err(format!("mutation outcome repeats `{name}`"));
        }
    }
    if result.is_empty() {
        return Err("mutation outcomes contains no mutants".to_string());
    }
    Ok(result)
}

fn mutation_manifest_names(bytes: &[u8]) -> Result<BTreeSet<String>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| format!("parse mutant manifest: {error}"))?;
    let entries = value
        .as_array()
        .ok_or_else(|| "mutant manifest is not an array".to_string())?;
    let names = entries
        .iter()
        .map(|entry| {
            entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "mutant manifest entry has no name".to_string())
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if names.len() != entries.len() || names.is_empty() {
        return Err("mutant manifest is empty or repeats an identity".to_string());
    }
    Ok(names)
}

fn read_documentation_contracts(root: &Path) -> Result<DocumentationContracts, String> {
    let path = root.join(".config/documentation-contracts.toml");
    let content =
        fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let contracts: DocumentationContracts =
        toml::from_str(&content).map_err(|error| format!("parse {}: {error}", path.display()))?;
    if contracts.version != DOCUMENTATION_CONTRACT_VERSION {
        return Err(format!(
            "{} uses documentation contract version {}; expected {}",
            path.display(),
            contracts.version,
            DOCUMENTATION_CONTRACT_VERSION
        ));
    }
    Ok(contracts)
}

pub(crate) fn propose_ledger(args: &[OsString]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("usage: cargo xtask test-estate-proposal".to_string());
    }
    let root = super::repo_root()?;
    let observed = collect_observed_sites(&root)?;
    let mut totals = BTreeMap::<String, Counts>::new();
    for site in &observed {
        totals
            .entry(site.key.path.clone())
            .or_default()
            .record(site.key.kind);
    }

    println!("# Proposed snapshot only; review every reason before editing the ledger.");
    for (path, counts) in totals {
        println!("\n[[file]]");
        println!("path = {path:?}");
        println!("sleeps = {}", counts.sleeps);
        println!("raw_clocks = {}", counts.raw_clocks);
        println!("free_port_helpers = {}", counts.free_port_helpers);
        println!("elapsed_calls = {}", counts.elapsed_calls);
        println!("reason = \"TODO: owner-reviewed per-file debt summary\"");
    }
    for site in observed {
        println!("\n[[site]]");
        println!("path = {:?}", site.key.path);
        println!("scope = {:?}", site.key.scope);
        println!("kind = {:?}", site.key.kind.as_str());
        println!("fingerprint = {:?}", site.key.fingerprint);
        println!("reason = \"TODO: owner-reviewed exact-site reason\"");
        println!("# syntax: {}", site.syntax.replace('\n', " "));
    }
    Ok(())
}

pub(crate) fn propose_documentation_contracts(args: &[OsString]) -> Result<(), String> {
    let options = parse_docs_proposal_args(args)?;
    let root = super::repo_root()?;
    let docs_root = options.unwrap_or_else(|| root.clone());
    let tests = collect_test_identities(&root)?;
    let integration_targets = collect_integration_test_targets(&root)?;
    let mut violations = Vec::new();
    let groups = collect_documentation_groups(&docs_root, false, &mut violations)?;
    if !violations.is_empty() {
        return Err(format!(
            "cannot propose documentation contracts:\n- {}",
            violations.join("\n- ")
        ));
    }
    if groups.is_empty() {
        return Err(format!(
            "no enforcement tags found below {}",
            docs_root.display()
        ));
    }

    println!("# REVIEW PROPOSAL ONLY. Nothing below is approved or written automatically.");
    println!("# Add each claim tag to its document, then ratify its tier, tests, and rationale.");
    println!("version = {DOCUMENTATION_CONTRACT_VERSION}");
    let mut used_ids = BTreeSet::new();
    for group in groups {
        let id = if group.id.is_empty() {
            proposed_claim_id(&group.document, &group.paragraph, &mut used_ids)
        } else {
            used_ids.insert(group.id.clone());
            group.id.clone()
        };
        println!("\n# {}:{}", group.document, group.line);
        println!("# Insert before the existing enforced-by tags:");
        println!("# <!-- vigil-claim: `{id}` -->");
        println!("# first-tag: {}", group.first_enforcement_tag);
        println!("[[claim]]");
        println!("id = {id:?}");
        println!("document = {:?}", group.document);
        println!("paragraph_sha256 = {:?}", group.paragraph_sha256);
        println!(
            "evidence_tier = {:?} # suggested; reviewer must confirm",
            suggested_evidence_tier(&group.tests, &integration_targets).as_str()
        );
        println!("tests = [");
        for test in &group.tests {
            let marker = if tests.contains(test) {
                ""
            } else {
                " # UNKNOWN TEST"
            };
            println!("  {test:?},{marker}");
        }
        println!("]");
        println!("reviewed = false");
        println!("reviewed_by = \"Independent semantic-mapping review pending\"");
        println!(
            "rationale = \"Executable mapping proposed; independent semantic review pending.\""
        );
    }
    Ok(())
}

pub(crate) fn propose_test_contracts(args: &[OsString]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("usage: cargo xtask test-contract-proposal".to_string());
    }
    let root = super::repo_root()?;
    let records = collect_test_records(&root)?;
    let mut by_family = BTreeMap::<(String, String, String, String), Vec<TestRecord>>::new();
    let mut physical = Vec::new();
    for record in records {
        if record.ignored && record.identity.contains("real_camera") {
            physical.push(record);
            continue;
        }
        let (id, lineage, kind, gate) = inferred_contract_family(&record.source);
        by_family
            .entry((
                id.to_string(),
                lineage.to_string(),
                kind.to_string(),
                gate.to_string(),
            ))
            .or_default()
            .push(record);
    }
    println!("# REVIEW PROPOSAL ONLY. This command never writes or approves registry entries.");
    println!("version = {TEST_CONTRACT_VERSION}");
    for ((id, lineage, kind, gate), mut records) in by_family {
        records.sort();
        print_contract_proposal(&id, &lineage, &kind, &gate, &records, None, None);
    }
    if !physical.is_empty() {
        physical.sort();
        print_contract_proposal(
            "real-camera-physical-frigate-replacement-journey",
            "Vigil test-estate inventory: installable and physical acceptance; owner-run real-camera window.",
            "physical",
            "physical",
            &physical,
            Some(
                "vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic",
            ),
            Some(
                "cargo test --features acceptance --test acceptance one_camera_acceptance::frigate_replacement_real_camera_event_lands_and_walks -- --ignored --exact",
            ),
        );
    }
    Ok(())
}

fn print_contract_proposal(
    id: &str,
    lineage: &str,
    kind: &str,
    gate: &str,
    records: &[TestRecord],
    witness: Option<&str>,
    command: Option<&str>,
) {
    println!("\n[[contract]]");
    println!("id = {id:?}");
    println!("lineage = {lineage:?}");
    let sources = records
        .iter()
        .map(|record| record.source.clone())
        .collect::<BTreeSet<_>>();
    println!("sources = [");
    for source in sources {
        println!("  {source:?},");
    }
    println!("]");
    println!("evidence_kind = {kind:?}");
    println!("gate = {gate:?}");
    if let Some(witness) = witness {
        println!("deterministic_witness = {witness:?}");
    }
    if let Some(command) = command {
        println!("explicit_run = {command:?}");
    }
    println!("tests = [");
    for record in records {
        println!("  {:?}, # {}", record.identity, record.source);
    }
    println!("]");
}

pub(crate) fn propose_test_baseline(args: &[OsString]) -> Result<(), String> {
    let root = if args.is_empty() {
        super::repo_root()?
    } else if args.len() == 2 && args[0] == "--root" {
        PathBuf::from(&args[1])
    } else {
        return Err("usage: cargo xtask test-baseline-proposal [--root PATH]".to_string());
    };
    let tests = collect_test_records(&root)?
        .into_iter()
        .map(|record| record.identity)
        .collect::<BTreeSet<_>>();
    println!("# REVIEW PROPOSAL ONLY. This command never writes or approves a baseline.");
    println!("version = {TEST_BASELINE_VERSION}");
    println!("base_sha = {FROZEN_TEST_BASE_SHA:?}");
    println!("vector_sha256 = {:?}", identity_vector_digest(&tests));
    println!("tests = [");
    for test in tests {
        println!("  {test:?},");
    }
    println!("]");
    Ok(())
}

fn inferred_contract_family(
    source: &str,
) -> (&'static str, &'static str, &'static str, &'static str) {
    let file = Path::new(source)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown");
    match file {
        "config" => (
            "operator-configuration-reaches-runtime",
            "Vigil inventory: inline production-module configuration tests; owner rulings and HTTP/recognition contracts.",
            "behavioral-unit",
            "pull-request",
        ),
        "decode" => (
            "codec-parameter-sets-complete-decoder-config",
            "Vigil inventory: inline decode tests; current SDP parameter-set defect work list.",
            "behavioral-unit",
            "pull-request",
        ),
        "clock" => (
            "injected-clock-controls-test-time",
            "Test-estate deterministic harness implementation: injected clock boundary.",
            "behavioral-unit",
            "pull-request",
        ),
        "ha_discovery" => (
            "home-assistant-discovery-events-and-corrections",
            "Accepted Home Assistant discovery and MQTT contract.",
            "behavioral-unit",
            "pull-request",
        ),
        "media_pipeline" => (
            "media-evidence-credentials-and-redaction",
            "Accepted HTTP/media and Home Assistant live-camera contracts.",
            "behavioral-unit",
            "pull-request",
        ),
        "runtime" => (
            "runtime-queue-camera-selection-and-detection-gates",
            "Vigil owner rulings for newest-wins queues, camera URLs, and stationary scanning.",
            "behavioral-unit",
            "pull-request",
        ),
        "supervisor" => (
            "supervisor-service-and-generic-camera-payloads",
            "Accepted Home Assistant Supervisor and Generic Camera integration contract.",
            "behavioral-unit",
            "pull-request",
        ),
        "first_light_loop" => (
            "camera-process-store-and-cli-journeys",
            "Accepted first-light process, store, RTSP, and CLI criteria.",
            "acceptance",
            "slow-acceptance",
        ),
        "ha_correction_seam" => (
            "corrections-are-authoritative-durable-and-readable",
            "Accepted Home Assistant correction durability contract.",
            "integration",
            "slow-acceptance",
        ),
        "ha_mqtt_broker" => (
            "mqtt-discovery-delivery-state-and-corrections",
            "Accepted live-broker Home Assistant integration contract.",
            "integration",
            "pull-request",
        ),
        "http_data_plane" => (
            "http-events-evidence-health-and-review-surfaces",
            "Accepted HTTP data-plane and media contract, superseded by same-origin owner ruling where applicable.",
            "integration",
            "pull-request",
        ),
        "recognition_slice" => (
            "recognition-enrollment-matching-honesty-and-deletion",
            "Accepted recognition enrollment, matching, unknown honesty, provenance, and deletion contract.",
            "integration",
            "pull-request",
        ),
        "deterministic_test_support" | "ha_test_support" => (
            "deterministic-harness-readiness-and-prerequisites",
            "Test-estate harness safety: held ports, bounded readiness, and mandatory prerequisites.",
            "behavioral-unit",
            "pull-request",
        ),
        "installable_substrate" => (
            "installable-binary-container-lifecycle-and-health",
            "Accepted installable substrate binary and container criteria.",
            "acceptance",
            "slow-acceptance",
        ),
        "one_camera_acceptance" => (
            "synthetic-camera-frigate-replacement-journey",
            "Accepted installable one-camera Frigate-replacement journey.",
            "acceptance",
            "slow-acceptance",
        ),
        "common" => (
            "acceptance-harness-process-port-and-network-safety",
            "Accepted first-light harness-safety criteria and the explicit acceptance-harness lineage review.",
            "behavioral-unit",
            "pull-request",
        ),
        "addon_config_surface"
        | "artifact_profile_honesty"
        | "oss_cleanliness_scan"
        | "probe_deadline_knob_surface"
        | "runtime_packages_manifest"
        | "source_scan_contract"
        | "transport_purity" => (
            "structural-packaging-and-source-contracts",
            "Vigil inventory structural-evidence section; replacement remains required before deleting source scans.",
            "structural",
            "pull-request",
        ),
        "boot_readiness"
        | "cameraless_worker"
        | "capability_delivery"
        | "capability_lifecycle"
        | "dead_capability_steal"
        | "detector_worker"
        | "fabric_enrollment"
        | "fabric_worker_lease_knob"
        | "kill_worker_fallback"
        | "promotion_readvertise"
        | "provenance_receipts"
        | "result_join_authority"
        | "symmetry"
        | "two_process_fabric"
        | "worker_intent_loudness" => (
            "distributed-worker-lifecycle-routing-and-receipts",
            "Accepted distributed-compute worker, ledger, routing, fallback, and receipt criteria.",
            "integration",
            "fabric",
        ),
        "detector_workclass_schema"
        | "fabric_config_defaults"
        | "offload_policy"
        | "runtime_offload_seam"
        | "workgraph_contract" => (
            "distributed-schema-policy-and-workgraph-authority",
            "Accepted distributed-compute schema, pressure policy, workgraph, and visible-config criteria.",
            "behavioral-unit",
            "fabric",
        ),
        "acceleration_receipts"
        | "config_acceleration_intent"
        | "decode_backend_contract"
        | "detection_accel_backend"
        | "detection_fallback_action_truth"
        | "detection_probe_completes_after_deadline"
        | "detection_probe_promotes_after_deadline"
        | "doctor_acceleration"
        | "shader_cache_persistence" => (
            "acceleration-selection-fallback-promotion-and-honesty",
            "Accepted acceleration selection, fallback, promotion, artifact, and receipt criteria.",
            "integration",
            "accelerated-detection",
        ),
        "health_watchdog_liveness" => (
            "health-watchdog-liveness-and-recovery",
            "Vigil inventory: health watchdog production-boundary regressions.",
            "integration",
            "pull-request",
        ),
        "privilege_supplemental_gids" => (
            "supplemental-group-privilege-ordering",
            "Current test-estate work list: privilege ordering is an untouchable regression.",
            "integration",
            "pull-request",
        ),
        _ => (
            "reviewed-vigil-behavioral-contract",
            "Vigil test-estate inventory; reviewer must replace this proposal family with its named contract.",
            "integration",
            "pull-request",
        ),
    }
}

fn collect_integration_test_targets(root: &Path) -> Result<BTreeSet<String>, String> {
    let mut files = Vec::new();
    collect_rs(&root.join("crates/vigil/tests"), &mut files)?;
    Ok(files
        .into_iter()
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
        .collect())
}

fn suggested_evidence_tier(
    tests: &BTreeSet<String>,
    integration_targets: &BTreeSet<String>,
) -> EvidenceTier {
    if tests
        .iter()
        .any(|test| test.starts_with("vigil-acceptance::"))
    {
        return EvidenceTier::Acceptance;
    }
    let structural_targets = BTreeSet::from([
        "addon_config_surface",
        "artifact_profile_honesty",
        "oss_cleanliness_scan",
        "probe_deadline_knob_surface",
        "runtime_packages_manifest",
        "source_scan_contract",
        "transport_purity",
    ]);
    let targets = tests
        .iter()
        .filter_map(|test| test.split("::").nth(1))
        .collect::<Vec<_>>();
    if !targets.is_empty()
        && targets
            .iter()
            .all(|target| structural_targets.contains(target))
    {
        return EvidenceTier::StructuralContract;
    }
    if targets
        .iter()
        .any(|target| integration_targets.contains(*target))
    {
        EvidenceTier::Integration
    } else {
        EvidenceTier::BehavioralUnit
    }
}

fn parse_docs_proposal_args(args: &[OsString]) -> Result<Option<PathBuf>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    if args.len() == 2 && args[0] == "--docs" {
        return Ok(Some(PathBuf::from(&args[1])));
    }
    Err("usage: cargo xtask documentation-contract-proposal [--docs PATH]".to_string())
}

fn parse_args(args: &[OsString]) -> Result<CheckOptions, String> {
    let mut options = CheckOptions::default();
    let mut index = 0usize;
    while index < args.len() {
        if index + 1 >= args.len() {
            return Err("test-estate-check option is missing its path".to_string());
        }
        match args[index].to_str() {
            Some("--docs") if options.docs.is_none() => {
                options.docs = Some(PathBuf::from(&args[index + 1]));
            }
            Some("--nextest-json") => {
                let value = args[index + 1].to_string_lossy();
                let Some((shape, path)) = value.split_once('=') else {
                    return Err("--nextest-json requires SHAPE=PATH".to_string());
                };
                if shape.is_empty() || path.is_empty() {
                    return Err("--nextest-json requires non-empty SHAPE=PATH".to_string());
                }
                options
                    .nextest_json
                    .push((shape.to_string(), PathBuf::from(path)));
            }
            _ => {
                return Err(
                    "usage: cargo xtask test-estate-check [--docs PATH] [--nextest-json SHAPE=PATH]..."
                        .to_string(),
                );
            }
        }
        index += 2;
    }
    Ok(options)
}

fn audit_rust(root: &Path, ledger: &Ledger) -> Result<Vec<String>, String> {
    let mut expected = BTreeMap::new();
    let mut violations = Vec::new();
    for allowance in &ledger.file {
        if !valid_reason(&allowance.reason) {
            violations.push(format!(
                "{} has an empty or placeholder exception reason",
                allowance.path
            ));
        }
        if expected
            .insert(allowance.path.clone(), allowance.clone())
            .is_some()
        {
            violations.push(format!(
                "{} is duplicated in the exception ledger",
                allowance.path
            ));
        }
    }

    let actual_sites = collect_observed_sites(root)?;
    let mut actual_by_file = BTreeMap::<String, Counts>::new();
    for site in &actual_sites {
        actual_by_file
            .entry(site.key.path.clone())
            .or_default()
            .record(site.key.kind);
    }

    let mut files = Vec::new();
    collect_rs(&root.join("crates/vigil/tests"), &mut files)?;
    collect_rs(&root.join("tests"), &mut files)?;
    collect_rs(&root.join("crates/vigil/src"), &mut files)?;
    collect_rs(&root.join("xtask/src"), &mut files)?;
    files.sort();

    let mut seen = BTreeSet::new();
    for path in files {
        let rel = relative(root, &path)?;
        let actual = actual_by_file.get(&rel).copied().unwrap_or_default();
        let allowed = expected.get(&rel);
        let wanted = allowed.map_or(Counts::default(), |entry| Counts {
            sleeps: entry.sleeps,
            raw_clocks: entry.raw_clocks,
            free_port_helpers: entry.free_port_helpers,
            elapsed_calls: entry.elapsed_calls,
        });
        if actual != wanted {
            violations.push(format!(
                "{rel}: observed sleeps={}, raw clocks={}, free-port helpers={}, elapsed calls={}; ledger says {}, {}, {}, {}. Replace nondeterminism or update the reviewed exception downward",
                actual.sleeps,
                actual.raw_clocks,
                actual.free_port_helpers,
                actual.elapsed_calls,
                wanted.sleeps,
                wanted.raw_clocks,
                wanted.free_port_helpers,
                wanted.elapsed_calls,
            ));
        }
        if allowed.is_some() {
            seen.insert(rel);
        }
    }
    for path in expected.keys() {
        if !seen.contains(path) {
            violations.push(format!(
                "exception ledger path does not exist or is not audited: {path}"
            ));
        }
    }

    let actual_multiset = site_multiset(actual_sites.iter().map(|site| site.key.clone()));
    let mut allowed_sites = Vec::new();
    for site in &ledger.site {
        if !valid_reason(&site.reason) {
            violations.push(format!(
                "{} {} {} has an empty or placeholder exact-site reason",
                site.path,
                site.scope,
                site.kind.as_str()
            ));
        }
        allowed_sites.push(SiteKey {
            path: site.path.clone(),
            scope: site.scope.clone(),
            kind: site.kind,
            fingerprint: site.fingerprint.clone(),
        });
    }
    let allowed_multiset = site_multiset(allowed_sites);
    let keys = actual_multiset
        .keys()
        .chain(allowed_multiset.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in keys {
        let actual = actual_multiset.get(&key).copied().unwrap_or_default();
        let allowed = allowed_multiset.get(&key).copied().unwrap_or_default();
        if actual != allowed {
            violations.push(format!(
                "{} scope `{}` {} site fingerprint {} observed {actual} time(s), ledger allows {allowed}",
                key.path,
                key.scope,
                key.kind.as_str(),
                key.fingerprint
            ));
        }
    }
    Ok(violations)
}

struct AuditVisitor {
    path: String,
    modules: Vec<String>,
    functions: Vec<String>,
    aliases: BTreeMap<String, Vec<String>>,
    counts: Counts,
    sites: Vec<ObservedSite>,
}

impl AuditVisitor {
    fn new(path: String) -> Self {
        Self {
            path,
            modules: Vec::new(),
            functions: Vec::new(),
            aliases: BTreeMap::new(),
            counts: Counts::default(),
            sites: Vec::new(),
        }
    }

    fn scope(&self) -> String {
        let mut parts = self.modules.clone();
        parts.extend(self.functions.iter().cloned());
        if parts.is_empty() {
            "<module>".to_string()
        } else {
            parts.join("::")
        }
    }

    fn record(&mut self, kind: SiteKind, syntax: String) {
        self.counts.record(kind);
        self.sites.push(ObservedSite {
            key: SiteKey {
                path: self.path.clone(),
                scope: self.scope(),
                kind,
                fingerprint: syntax_fingerprint(&syntax),
            },
            syntax,
        });
    }

    fn expanded_path(&self, path: &syn::Path) -> Vec<String> {
        let mut parts = path
            .segments
            .iter()
            .map(|part| part.ident.to_string())
            .collect::<Vec<_>>();
        let alias_index = parts
            .iter()
            .position(|part| self.aliases.contains_key(part));
        if let Some(index) = alias_index
            && let Some(prefix) = self.aliases.get(&parts[index])
        {
            parts.splice(index..=index, prefix.clone());
        }
        parts
    }
}

impl<'ast> Visit<'ast> for AuditVisitor {
    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if let Expr::Path(path) = &*node.func {
            let segments = self.expanded_path(&path.path);
            let syntax = normalized_tokens(node);
            if segments.last().is_some_and(|part| part == "sleep") {
                self.record(SiteKind::Sleep, syntax.clone());
            }
            if segments.last().is_some_and(|part| part == "now")
                && segments
                    .iter()
                    .any(|part| matches!(part.as_str(), "SystemTime" | "Utc" | "Local" | "Instant"))
            {
                self.record(SiteKind::RawClock, syntax.clone());
            }
            if (suffix(&segments, &["TcpListener", "bind"])
                || suffix(&segments, &["UdpSocket", "bind"]))
                && node.args.iter().any(expr_binds_port_zero)
            {
                self.record(SiteKind::BindPortZero, syntax);
            }
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        if node.method == "elapsed" || node.method == "duration_since" {
            self.record(SiteKind::ElapsedCall, normalized_tokens(node));
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let aliases = self.aliases.clone();
        self.functions.push(node.sig.ident.to_string());
        visit::visit_item_fn(self, node);
        self.functions.pop();
        self.aliases = aliases;
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        let aliases = self.aliases.clone();
        self.functions.push(node.sig.ident.to_string());
        visit::visit_impl_item_fn(self, node);
        self.functions.pop();
        self.aliases = aliases;
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        let aliases = self.aliases.clone();
        self.modules
            .push(format!("impl {}", normalized_tokens(&*node.self_ty)));
        visit::visit_item_impl(self, node);
        self.modules.pop();
        self.aliases = aliases;
    }

    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        let aliases = self.aliases.clone();
        self.modules.push(node.ident.to_string());
        visit::visit_item_mod(self, node);
        self.modules.pop();
        self.aliases = aliases;
    }

    fn visit_item_use(&mut self, node: &'ast ItemUse) {
        collect_use_aliases(&node.tree, Vec::new(), &mut self.aliases);
        visit::visit_item_use(self, node);
    }

    fn visit_macro(&mut self, node: &'ast Macro) {
        let tokens = node.tokens.to_string();
        let compact = tokens
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        if compact.contains("sleep(") {
            self.record(SiteKind::MacroSleep, normalized_tokens(node));
        }
        visit::visit_macro(self, node);
    }
}

fn collect_observed_sites(root: &Path) -> Result<Vec<ObservedSite>, String> {
    let mut files = Vec::new();
    collect_rs(&root.join("crates/vigil/tests"), &mut files)?;
    collect_rs(&root.join("tests"), &mut files)?;
    collect_rs(&root.join("crates/vigil/src"), &mut files)?;
    collect_rs(&root.join("xtask/src"), &mut files)?;
    files.sort();
    let mut observed = Vec::new();
    for path in files {
        let rel = relative(root, &path)?;
        let source = fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let syntax = syn::parse_file(&source)
            .map_err(|error| format!("parse Rust syntax in {rel}: {error}"))?;
        let mut visitor = AuditVisitor::new(rel.clone());
        if rel.starts_with("crates/vigil/src/") {
            for item in &syntax.items {
                if let Item::Use(item_use) = item {
                    collect_use_aliases(&item_use.tree, Vec::new(), &mut visitor.aliases);
                }
            }
            visit_source_test_items(&syntax.items, &mut visitor);
        } else {
            visitor.visit_file(&syntax);
        }
        observed.extend(visitor.sites);
    }
    observed.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(observed)
}

fn site_multiset(entries: impl IntoIterator<Item = SiteKey>) -> BTreeMap<SiteKey, usize> {
    let mut counts = BTreeMap::new();
    for entry in entries {
        *counts.entry(entry).or_default() += 1;
    }
    counts
}

fn valid_reason(reason: &str) -> bool {
    let reason = reason.trim();
    !reason.is_empty() && !reason.to_ascii_lowercase().contains("todo")
}

fn visit_source_test_items(items: &[Item], visitor: &mut AuditVisitor) {
    for item in items {
        match item {
            Item::Mod(module) if is_cfg_test(&module.attrs) => visitor.visit_item_mod(module),
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    visitor.modules.push(module.ident.to_string());
                    visit_source_test_items(nested, visitor);
                    visitor.modules.pop();
                }
            }
            Item::Fn(function) if is_test(&function.attrs) || is_cfg_test(&function.attrs) => {
                visitor.visit_item_fn(function);
            }
            Item::Impl(item_impl) if is_cfg_test(&item_impl.attrs) => {
                visitor.visit_item_impl(item_impl);
            }
            _ => {}
        }
    }
}

fn collect_use_aliases(
    tree: &UseTree,
    mut prefix: Vec<String>,
    aliases: &mut BTreeMap<String, Vec<String>>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_use_aliases(&path.tree, prefix, aliases);
        }
        UseTree::Name(name) => {
            prefix.push(name.ident.to_string());
            aliases.insert(name.ident.to_string(), prefix);
        }
        UseTree::Rename(rename) => {
            prefix.push(rename.ident.to_string());
            aliases.insert(rename.rename.to_string(), prefix);
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_use_aliases(item, prefix.clone(), aliases);
            }
        }
        UseTree::Glob(_) => {}
    }
}

fn expr_binds_port_zero(expr: &Expr) -> bool {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) => value
            .value()
            .rsplit_once(':')
            .is_some_and(|(_, port)| port == "0"),
        Expr::Tuple(tuple) => tuple.elems.last().is_some_and(is_integer_zero),
        Expr::Reference(reference) => expr_binds_port_zero(&reference.expr),
        Expr::Paren(paren) => expr_binds_port_zero(&paren.expr),
        Expr::Group(group) => expr_binds_port_zero(&group.expr),
        _ => false,
    }
}

fn is_integer_zero(expr: &Expr) -> bool {
    matches!(expr, Expr::Lit(ExprLit { lit: Lit::Int(value), .. }) if value.base10_parse::<u64>().ok() == Some(0))
}

fn normalized_tokens(value: &impl ToTokens) -> String {
    value.to_token_stream().to_string()
}

fn syntax_fingerprint(syntax: &str) -> String {
    let digest = Sha256::digest(syntax.as_bytes());
    format!("sha256:{digest:x}")
}

fn suffix(parts: &[String], wanted: &[&str]) -> bool {
    parts.len() >= wanted.len()
        && parts[parts.len() - wanted.len()..]
            .iter()
            .map(String::as_str)
            .eq(wanted.iter().copied())
}

fn is_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        (attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
            && attr
                .meta
                .to_token_stream()
                .to_string()
                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                .any(|token| token == "test")
    })
}

fn audit_nextest_config(root: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    let path = root.join(".config/nextest.toml");
    let value: toml::Value = toml::from_str(
        &fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("parse {}: {error}", path.display()))?;
    let profile = value.get("profile").and_then(toml::Value::as_table);
    for name in ["pr", "ci-full"] {
        let table = profile
            .and_then(|profiles| profiles.get(name))
            .and_then(toml::Value::as_table);
        if table
            .and_then(|entry| entry.get("fail-fast"))
            .and_then(toml::Value::as_bool)
            != Some(false)
        {
            violations.push(format!("nextest profile {name} must set fail-fast = false"));
        }
        if table
            .and_then(|entry| entry.get("retries"))
            .and_then(toml::Value::as_integer)
            != Some(0)
        {
            violations.push(format!("nextest profile {name} must set retries = 0"));
        }
        if let Some(overrides) = table
            .and_then(|entry| entry.get("overrides"))
            .and_then(toml::Value::as_array)
        {
            for (index, override_value) in overrides.iter().enumerate() {
                if override_value.get("retries").is_some() {
                    violations.push(format!(
                        "nextest profile {name} override {index} must not re-enable retries"
                    ));
                }
            }
        }
    }
    for (name, expected) in [("pr", 2_i64), ("ci-full", 1_i64)] {
        let observed = profile
            .and_then(|profiles| profiles.get(name))
            .and_then(toml::Value::as_table)
            .and_then(|entry| entry.get("test-threads"))
            .and_then(toml::Value::as_integer);
        if observed != Some(expected) {
            violations.push(format!(
                "nextest profile {name} must set test-threads = {expected} for the 10 GiB development resource contract"
            ));
        }
    }
    Ok(())
}

fn audit_source_scan_allowlist(root: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    let path = root.join(".config/test-source-scan-allowlist.toml");
    let allowlist: SourceScanAllowlist = read_toml(&path)?;
    if allowlist.version != SOURCE_SCAN_ALLOWLIST_VERSION {
        violations.push(format!(
            "{} uses version {}; expected {SOURCE_SCAN_ALLOWLIST_VERSION}",
            path.display(),
            allowlist.version
        ));
    }
    let mut allowed = BTreeSet::new();
    for entry in &allowlist.file {
        if !valid_reason(&entry.reason) {
            violations.push(format!(
                "structural source-scan allowance {} has no reviewed reason",
                entry.path
            ));
        }
        if !allowed.insert(entry.path.clone()) {
            violations.push(format!(
                "structural source-scan allowance {} is duplicated",
                entry.path
            ));
        }
    }
    let mut files = Vec::new();
    collect_rs(&root.join("crates/vigil/tests"), &mut files)?;
    collect_rs(&root.join("tests/acceptance"), &mut files)?;
    let mut observed = BTreeSet::new();
    for file in files {
        let rel = relative(root, &file)?;
        let source = fs::read_to_string(&file)
            .map_err(|error| format!("read {}: {error}", file.display()))?;
        if is_production_source_text_scan(&source) {
            observed.insert(rel.clone());
            if !allowed.contains(&rel) {
                violations.push(format!(
                    "new production-source text scan file `{rel}` is outside the exact structural allowlist; use behavioral evidence or obtain reviewed structural lineage"
                ));
            }
        }
    }
    for path in allowed.difference(&observed) {
        violations.push(format!(
            "structural source-scan allowance `{path}` is stale: the file no longer matches the source-text scan definition"
        ));
    }
    Ok(())
}

fn audit_fabric_worker_lease_wiring(
    root: &Path,
    violations: &mut Vec<String>,
) -> Result<(), String> {
    let path = root.join("crates/vigil/src/fabric.rs");
    let source =
        fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    violations.extend(
        fabric_worker_lease_wiring_violations(&source)
            .map_err(|error| format!("parse {}: {error}", path.display()))?,
    );
    Ok(())
}

fn audit_correction_writer_boundary(
    root: &Path,
    violations: &mut Vec<String>,
) -> Result<(), String> {
    let correction_path = root.join("crates/vigil/src/correction.rs");
    let runtime_path = root.join("crates/vigil/src/runtime.rs");
    let correction_source = fs::read_to_string(&correction_path)
        .map_err(|error| format!("read {}: {error}", correction_path.display()))?;
    let runtime_source = fs::read_to_string(&runtime_path)
        .map_err(|error| format!("read {}: {error}", runtime_path.display()))?;
    violations.extend(
        correction_writer_boundary_violations(&correction_source, &runtime_source)
            .map_err(|error| format!("parse correction-writer boundary: {error}"))?,
    );
    Ok(())
}

#[derive(Default)]
struct CorrectionNetworkVisitor {
    violations: Vec<String>,
}

impl CorrectionNetworkVisitor {
    fn inspect_segments(&mut self, segments: &[String]) {
        let forbidden_namespace = segments.windows(2).any(|pair| {
            matches!(pair, [root, module] if (root == "std" || root == "tokio") && module == "net")
        });
        let forbidden_client = segments.iter().any(|segment| {
            matches!(
                segment.as_str(),
                "TcpStream" | "UdpSocket" | "reqwest" | "rumqtt" | "rumqttc"
            )
        });
        if forbidden_namespace || forbidden_client {
            self.violations.push(format!(
                "local correction authority imports or calls forbidden network path `{}`",
                segments.join("::")
            ));
        }
    }
}

impl<'ast> Visit<'ast> for CorrectionNetworkVisitor {
    fn visit_item_use(&mut self, node: &'ast ItemUse) {
        let mut paths = Vec::new();
        collect_use_paths(&node.tree, Vec::new(), &mut paths);
        for path in paths {
            self.inspect_segments(&path);
        }
        visit::visit_item_use(self, node);
    }

    fn visit_path(&mut self, node: &'ast syn::Path) {
        let segments = node
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        self.inspect_segments(&segments);
        visit::visit_path(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        if node.method == "connect" {
            self.violations.push(
                "local correction authority calls forbidden network method `connect`".to_string(),
            );
        }
        visit::visit_expr_method_call(self, node);
    }
}

fn collect_use_paths(tree: &UseTree, prefix: Vec<String>, paths: &mut Vec<Vec<String>>) {
    match tree {
        UseTree::Path(path) => {
            let mut next = prefix;
            next.push(path.ident.to_string());
            collect_use_paths(&path.tree, next, paths);
        }
        UseTree::Name(name) => {
            let mut complete = prefix;
            complete.push(name.ident.to_string());
            paths.push(complete);
        }
        UseTree::Rename(rename) => {
            let mut complete = prefix;
            complete.push(rename.ident.to_string());
            paths.push(complete);
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_use_paths(item, prefix.clone(), paths);
            }
        }
        UseTree::Glob(_) => paths.push(prefix),
    }
}

#[derive(Default)]
struct CorrectionWriterNameVisitor {
    exact_names: usize,
    exact_spawns: usize,
    network_violations: Vec<String>,
}

impl<'ast> Visit<'ast> for CorrectionWriterNameVisitor {
    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        let is_exact_writer_name = |call: &ExprMethodCall| {
            if call.method != "name" || call.args.len() != 1 {
                return false;
            }
            let Some(Expr::MethodCall(to_string)) = call.args.first() else {
                return false;
            };
            if to_string.method != "to_string" {
                return false;
            }
            let Expr::Lit(ExprLit {
                lit: Lit::Str(name),
                ..
            }) = &*to_string.receiver
            else {
                return false;
            };
            name.value() == "vigil-correct"
        };
        if is_exact_writer_name(node) {
            self.exact_names += 1;
        }
        if node.method == "spawn"
            && let Expr::MethodCall(name_call) = &*node.receiver
            && is_exact_writer_name(name_call)
            && let Some(Expr::Closure(writer)) = node.args.first()
        {
            self.exact_spawns += 1;
            let mut network = CorrectionNetworkVisitor::default();
            network.visit_expr(&writer.body);
            self.network_violations.extend(network.violations);
        }
        visit::visit_expr_method_call(self, node);
    }
}

fn correction_writer_boundary_violations(
    correction_source: &str,
    runtime_source: &str,
) -> Result<Vec<String>, syn::Error> {
    let correction = syn::parse_file(correction_source)?;
    let runtime = syn::parse_file(runtime_source)?;
    let mut network = CorrectionNetworkVisitor::default();
    network.visit_file(&correction);
    let mut writer = CorrectionWriterNameVisitor::default();
    writer.visit_file(&runtime);
    network.violations.extend(writer.network_violations);
    if writer.exact_names != 1 || writer.exact_spawns != 1 {
        network.violations.push(format!(
            "runtime must spawn exactly one `.name(\"vigil-correct\".to_string())` correction writer; observed {} names and {} attached spawn closures",
            writer.exact_names, writer.exact_spawns
        ));
    }
    Ok(network.violations)
}

#[derive(Default)]
struct FabricWorkerLeaseWiringVisitor {
    worker_configs: usize,
    resolver_wirings: usize,
}

impl<'ast> Visit<'ast> for FabricWorkerLeaseWiringVisitor {
    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if node
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "WorkerConfig")
        {
            self.worker_configs += 1;
            for field in &node.fields {
                let syn::Member::Named(member) = &field.member else {
                    continue;
                };
                if member != "lease_duration_ms" {
                    continue;
                }
                let Expr::Call(call) = &field.expr else {
                    continue;
                };
                let Expr::Path(function) = &*call.func else {
                    continue;
                };
                if function
                    .path
                    .segments
                    .last()
                    .is_none_or(|segment| segment.ident != "resolved_worker_lease_duration_ms")
                    || call.args.len() != 1
                {
                    continue;
                }
                let Some(Expr::Field(argument)) = call.args.first() else {
                    continue;
                };
                let syn::Member::Named(argument_member) = &argument.member else {
                    continue;
                };
                if argument_member == "worker_lease_ms"
                    && matches!(&*argument.base, Expr::Path(path) if path.path.is_ident("self"))
                {
                    self.resolver_wirings += 1;
                }
            }
        }
        visit::visit_expr_struct(self, node);
    }
}

fn fabric_worker_lease_wiring_violations(source: &str) -> Result<Vec<String>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let mut visitor = FabricWorkerLeaseWiringVisitor::default();
    visitor.visit_file(&syntax);

    let mut violations = Vec::new();
    if visitor.worker_configs != 1 {
        violations.push(format!(
            "fabric.rs must construct exactly one WorkerConfig; observed {}",
            visitor.worker_configs
        ));
    }
    if visitor.resolver_wirings != 1 {
        violations.push(format!(
            "fabric WorkerConfig.lease_duration_ms must be assigned exactly once from \
             resolved_worker_lease_duration_ms(self.worker_lease_ms); observed {} exact syntax-aware wiring(s)",
            visitor.resolver_wirings
        ));
    }
    Ok(violations)
}

fn is_production_source_text_scan(source: &str) -> bool {
    let reads_text = source.contains("read_to_string")
        || source.contains("include_str!")
        || source.contains("include_bytes!")
        || source.contains("fs::read(")
        || source.contains("read_repo_file")
        || source.contains("read_required");
    let Ok(syntax) = syn::parse_file(source) else {
        return false;
    };
    let mut strings = StringLiteralVisitor::default();
    strings.visit_file(&syntax);
    let production_path = [
        "src/",
        "Cargo.toml",
        "Dockerfile",
        ".github/workflows",
        "addons/",
        "scripts/",
        "runtime-packages",
    ]
    .iter()
    .any(|marker| strings.values.iter().any(|value| value.contains(marker)))
        || strings.values.iter().any(|value| value == "src");
    reads_text && production_path
}

#[derive(Default)]
struct StringLiteralVisitor {
    values: Vec<String>,
}

impl<'ast> Visit<'ast> for StringLiteralVisitor {
    fn visit_attribute(&mut self, _node: &'ast Attribute) {
        // Rust doc comments are lowered to #[doc = "..."]; they describe
        // source paths but do not make the test read production source text.
    }

    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.values.push(node.value());
    }
}

fn audit_silent_prerequisite_passes(
    root: &Path,
    violations: &mut Vec<String>,
) -> Result<(), String> {
    let records = collect_test_records(root)?;
    let ignored = records
        .iter()
        .filter(|record| record.ignored)
        .map(|record| {
            (
                record.source.clone(),
                record
                    .identity
                    .rsplit("::")
                    .next()
                    .unwrap_or("")
                    .to_string(),
            )
        })
        .collect::<BTreeSet<_>>();
    let mut files = Vec::new();
    collect_rs(&root.join("crates/vigil/tests"), &mut files)?;
    collect_rs(&root.join("tests/acceptance"), &mut files)?;
    collect_rs(&root.join("crates/vigil/src"), &mut files)?;
    collect_rs(&root.join("xtask/src"), &mut files)?;
    for file in files {
        let rel = relative(root, &file)?;
        let source = fs::read_to_string(&file)
            .map_err(|error| format!("read {}: {error}", file.display()))?;
        let syntax = syn::parse_file(&source)
            .map_err(|error| format!("parse Rust syntax in {rel}: {error}"))?;
        let mut tests = Vec::new();
        collect_test_functions(&syntax.items, &mut tests);
        for function in tests {
            if ignored.contains(&(rel.clone(), function.sig.ident.to_string())) {
                continue;
            }
            let mut visitor = SilentPrerequisiteVisitor {
                source: &rel,
                test: function.sig.ident.to_string(),
                violations,
            };
            visitor.visit_block(&function.block);
        }
    }
    Ok(())
}

fn collect_test_functions<'a>(items: &'a [Item], functions: &mut Vec<&'a ItemFn>) {
    for item in items {
        match item {
            Item::Fn(function) if is_test(&function.attrs) => functions.push(function),
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_test_functions(nested, functions);
                }
            }
            _ => {}
        }
    }
}

struct SilentPrerequisiteVisitor<'a> {
    source: &'a str,
    test: String,
    violations: &'a mut Vec<String>,
}

impl<'ast> Visit<'ast> for SilentPrerequisiteVisitor<'_> {
    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        let condition = normalized_tokens(&node.cond);
        let branch = normalized_tokens(&node.then_branch);
        if prerequisite_condition(&condition)
            && branch_has_bare_return(&branch)
            && !branch_fails_loudly(&branch)
        {
            self.violations.push(format!(
                "mandatory test `{}` in {} can return green when prerequisite condition `{condition}` is met",
                self.test, self.source
            ));
        }
        visit::visit_expr_if(self, node);
    }

    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        let condition = normalized_tokens(&node.expr);
        if prerequisite_condition(&condition) {
            for arm in &node.arms {
                let branch = normalized_tokens(&arm.body);
                if branch_has_bare_return(&branch) && !branch_fails_loudly(&branch) {
                    self.violations.push(format!(
                        "mandatory test `{}` in {} can return green from prerequisite match `{condition}`",
                        self.test, self.source
                    ));
                }
            }
        }
        visit::visit_expr_match(self, node);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if let Some(init) = &node.init
            && let Some((_, diverge)) = &init.diverge
        {
            let condition = normalized_tokens(&init.expr);
            let branch = normalized_tokens(diverge);
            if prerequisite_condition(&condition)
                && branch_has_bare_return(&branch)
                && !branch_fails_loudly(&branch)
            {
                self.violations.push(format!(
                    "mandatory test `{}` in {} can return green from missing prerequisite `{condition}`",
                    self.test, self.source
                ));
            }
        }
        visit::visit_local(self, node);
    }
}

fn prerequisite_condition(tokens: &str) -> bool {
    [
        "env :: var",
        "std :: env",
        "Command :: new",
        "required_tool",
        "which",
    ]
    .iter()
    .any(|marker| tokens.contains(marker))
}

fn branch_has_bare_return(tokens: &str) -> bool {
    tokens.split_whitespace().any(|token| token == "return")
}

fn branch_fails_loudly(tokens: &str) -> bool {
    [
        "assert !",
        "assert_eq !",
        "panic !",
        "expect (",
        "unwrap (",
        "return Err",
    ]
    .iter()
    .any(|marker| tokens.contains(marker))
}

fn audit_ci_config(root: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    let read_workflow = |name: &str| -> Result<String, String> {
        let path = root.join(".github/workflows").join(name);
        fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))
    };
    let change = read_workflow("ci.yml")?;
    let closeout = read_workflow("dev-closeout.yml")?;
    let integration = read_workflow("integrate-dev.yml")?;
    let release = read_workflow("release-qualification.yml")?;
    audit_ci_compartments(&change, &closeout, &release, violations);
    audit_dev_integration_workflow(&integration, violations);
    let launcher_path = root.join("scripts/verify");
    let launcher = fs::read_to_string(&launcher_path)
        .map_err(|error| format!("read {}: {error}", launcher_path.display()))?;
    audit_verify_launcher_text(&launcher, violations);
    let dockerignore_path = root.join(".dockerignore");
    let dockerignore = fs::read_to_string(&dockerignore_path)
        .map_err(|error| format!("read {}: {error}", dockerignore_path.display()))?;
    audit_dockerignore_text(&dockerignore, violations);
    Ok(())
}

fn audit_dockerignore_text(content: &str, violations: &mut Vec<String>) {
    let observed = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    let expected = [
        "*",
        "!Dockerfile",
        "!dist/",
        "dist/**",
        "!dist/docker/",
        "!dist/docker/amd64/",
        "!dist/docker/amd64/vigil",
        "!dist/docker/arm64/",
        "!dist/docker/arm64/vigil",
    ];
    if observed != expected {
        violations.push(
            "static Docker context must remain the exact Dockerfile-plus-two-dist-binaries allowlist; target trees and repository source are forbidden"
                .to_string(),
        );
    }
}

fn audit_codeowners(root: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    let path = root.join(".github/CODEOWNERS");
    let content =
        fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    audit_codeowners_text(&content, violations);
    Ok(())
}

fn audit_codeowners_text(content: &str, violations: &mut Vec<String>) {
    for required in [
        "/xtask/src/test_estate.rs @lucidprogrammer",
        "/.config/test-*.toml @lucidprogrammer",
        "/.config/nextest*.toml @lucidprogrammer",
        "/.config/non-rust-test-*.toml @lucidprogrammer",
        "/.config/documentation-contracts.toml @lucidprogrammer",
        "/.github/CODEOWNERS @lucidprogrammer",
        "/.github/workflows/ci.yml @lucidprogrammer",
        "/.github/workflows/dev-closeout.yml @lucidprogrammer",
        "/.github/workflows/integrate-dev.yml @lucidprogrammer",
        "/.github/workflows/release-qualification.yml @lucidprogrammer",
        "/.config/dev-closeout-impact.toml @lucidprogrammer",
        "/xtask/src/verify.rs @lucidprogrammer",
        "/xtask/src/closeout_impact.rs @lucidprogrammer",
        "/scripts/verify @lucidprogrammer",
        "/AGENTS.md @lucidprogrammer",
        "/tests/ha-os-vm/ @lucidprogrammer",
    ] {
        if !content.lines().any(|line| line.trim() == required) {
            violations.push(format!(
                "CODEOWNERS must preserve test-estate review rule `{required}`"
            ));
        }
    }
}

fn active_workflow_text(content: &str) -> String {
    content
        .lines()
        .map(|line| line.split_once('#').map_or(line, |(active, _)| active))
        .collect::<Vec<_>>()
        .join("\n")
}

fn require_workflow_text(
    tier: &str,
    active: &str,
    required: &[&str],
    violations: &mut Vec<String>,
) {
    for contract in required {
        if !active.contains(contract) {
            violations.push(format!(
                "{tier} workflow must preserve executable contract `{contract}`"
            ));
        }
    }
}

fn audit_ci_compartments(
    change: &str,
    closeout: &str,
    release: &str,
    violations: &mut Vec<String>,
) {
    let change = active_workflow_text(change);
    let closeout = active_workflow_text(closeout);
    let release = active_workflow_text(release);
    let change_scripts = workflow_run_scripts(&change);
    let closeout_scripts = workflow_run_scripts(&closeout);
    let release_scripts = workflow_run_scripts(&release);
    let change_commands = workflow_run_command_lines(&change_scripts);
    let closeout_commands = workflow_run_command_lines(&closeout_scripts);
    let release_commands = workflow_run_command_lines(&release_scripts);
    let closeout_verified = verified_workflow_commands(
        "development closeout",
        &closeout_scripts,
        "./scripts/verify dev-closeout \\",
        violations,
    );
    let release_verified = verified_workflow_commands(
        "release qualification",
        &release_scripts,
        "./scripts/verify release \\",
        violations,
    );
    let all = [&change, &closeout, &release];

    for content in all {
        if content.lines().any(|line| line.contains("--retries")) {
            violations.push("verification workflows must not retry failed tests".to_string());
        }
        if content.contains("pull_request_target")
            || content.contains("secrets.CG_CI_TOKEN || github.token")
        {
            violations.push(
                "verification workflows must fail closed at the private dependency boundary"
                    .to_string(),
            );
        }
        for line in content.lines() {
            let normalized = line.to_ascii_lowercase().replace(['\'', '"'], "");
            let routes_build_output = normalized.contains("cargo_target_dir")
                || normalized.contains("--target-dir")
                || normalized.contains("target-dir");
            let routes_to_temp = normalized.contains("/tmp")
                || normalized.contains("$runner_temp")
                || normalized.contains("runner.temp");
            if routes_build_output && routes_to_temp {
                violations.push(format!(
                    "verification workflows must not route Cargo/build targets to tmpfs: `{}`",
                    line.trim()
                ));
            }
        }
    }

    let bounded_apt = "sudo timeout --kill-after=30s 5m apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30";
    if all.iter().any(|content| {
        content
            .lines()
            .filter(|line| line.contains("apt-get"))
            .any(|line| line.matches("apt-get").count() != line.matches(bounded_apt).count())
    }) {
        violations.push(
            "workflow package installation must bound retries, network waits, and total apt runtime"
                .to_string(),
        );
    }

    require_workflow_text(
        "change",
        &change,
        &[
            "pull_request:",
            "branches: [dev]",
            "github.event.pull_request.head.repo.full_name != github.repository",
            "token: ${{ secrets.CG_CI_TOKEN }}",
            "persist-credentials: false",
            "cargo nextest list --locked --workspace -T json > target/nextest-default.json",
            "cargo xtask test-estate-check --nextest-json default=target/nextest-default.json",
            "cargo nextest run --locked --profile pr --workspace --test-threads 2",
            "cargo clippy --locked --workspace --all-targets -- -D warnings",
            "name: Change qualification receipt",
        ],
        violations,
    );
    for forbidden in [
        "workflow_dispatch:",
        "cargo nextest archive",
        "docker build",
        "docker buildx",
        "--features decode-gstreamer",
        "--features detect-burn-wgpu",
        "--features fabric",
        "--profile ci-full",
    ] {
        if change_commands.iter().any(|line| line.contains(forbidden)) {
            violations.push(format!(
                "change workflow must remain the cheap default-shape gate; `{forbidden}` belongs in a later compartment"
            ));
        }
    }

    require_workflow_text(
        "development closeout",
        &closeout,
        &[
            "workflow_dispatch:",
            "vigil_sha:",
            "dev_base_sha:",
            "context_graph_sha:",
            "contextdb_sha:",
            "statuses: write",
            "test \"$GITHUB_REF\" = \"refs/heads/dev\"",
            "test \"$GITHUB_SHA\" = \"$DEV_BASE_SHA\"",
            "repos/context-graph-ai/context-graph/git/ref/heads/dev",
            "repos/context-graph-ai/contextdb/git/ref/heads/dev",
            "git -C vigil rev-parse origin/dev",
            "git -C vigil merge-base \"$vigil_sha\" \"$dev_sha\"",
            "normal feature closeout cannot change trusted verification control plane",
            ".github/workflows/*",
            "Cargo.lock|Cargo.toml",
            "xtask/*",
            "read -r -d '' path",
            "--name-only --no-renames -z --diff-filter=ACDMRTUXB",
            "cargo xtask closeout-impact",
            "./scripts/verify dev-closeout",
            "jq -r '.impact'",
            "install-expanded",
            "--cache-state mixed",
            "if: needs.impact.outputs.install_expanded == 'true'",
            "--nextest-json \"${{ matrix.shape }}=target/nextest-${{ matrix.shape }}.json\"",
            "--lane slow-preflight",
            "cargo nextest archive --locked --release --workspace --features first-light-acceptance,acceptance",
            "--lane slow-shard",
            "-E 'not test(vigil_container_)'",
            "--lane install-smoke",
            "-E 'test(vigil_container_)'",
            "VIGIL_ACCEPTANCE_BIN:",
            "VIGIL_ACCEPTANCE_IMAGE:",
            "fast_forward_only:true",
            "context=vigil/dev-closeout",
        ],
        violations,
    );
    for (shape, feature) in [
        ("decode", "decode-gstreamer"),
        ("detect", "detect-burn-wgpu"),
        ("combined", "decode-gstreamer,detect-burn-wgpu"),
        ("fabric", "fabric"),
        ("production", "decode-gstreamer,detect-burn-wgpu,fabric"),
    ] {
        let mapping = format!("shape: {shape}\n            features: {feature}");
        if !closeout.contains(&mapping) {
            violations.push(format!(
                "development closeout must map feature shape `{shape}` to exact features `{feature}`"
            ));
        }
    }
    if closeout_verified
        .iter()
        .filter(|line| line.starts_with("-- cargo nextest archive "))
        .count()
        != 1
    {
        violations.push(
            "development closeout must compile the slow acceptance archive exactly once"
                .to_string(),
        );
    }
    if closeout.contains("cache_state:") || closeout.contains("inputs.cache_state") {
        violations.push(
            "development closeout must derive observed cache state in each receipt; callers cannot label the run warm or cold"
                .to_string(),
        );
    }
    let gate_position = closeout.find("dev-closeout-gate:");
    let status_permission_position = closeout.find("statuses: write");
    if closeout.matches("statuses: write").count() != 1
        || gate_position.is_none()
        || status_permission_position <= gate_position
    {
        violations.push(
            "development closeout must grant status-write only to the trusted final receipt job; candidate-executing jobs remain read-only"
                .to_string(),
        );
    }
    if closeout_verified
        .iter()
        .filter(|line| line.starts_with("-- docker build --tag \"vigil-closeout:"))
        .count()
        != 1
    {
        violations.push(
            "install-expanded closeout must build one prebuilt acceptance image exactly once"
                .to_string(),
        );
    }
    for forbidden in [
        "pull_request:",
        "docker buildx",
        "--push",
        "target: vigil-hw-",
    ] {
        if closeout_commands
            .iter()
            .any(|line| line.contains(forbidden))
        {
            violations.push(format!(
                "development closeout must not perform release qualification work `{forbidden}`"
            ));
        }
    }

    require_workflow_text(
        "release qualification",
        &release,
        &[
            "workflow_dispatch:",
            "main_base_sha:",
            "closeout_run_id:",
            "actions: read",
            "run-id: ${{ inputs.closeout_run_id }}",
            "./scripts/verify release",
            "github-token: ${{ github.token }}",
            "git -C vigil rev-parse origin/main",
            "git -C vigil merge-base \"$vigil_sha\" \"$main_sha\"",
            "test ! -e vigil/target",
            "--cache-state cold",
            "--cache-state mixed",
            "--target x86_64-unknown-linux-musl --features fabric",
            "--target aarch64-unknown-linux-musl --features fabric",
            "vigil-static-fallback.oci.tar",
            "vigil-hw-binary-amd64",
            "vigil-hw-binary-arm64",
            "vigil-generic-docker-hw-amd64",
            "vigil-generic-docker-hw-arm64",
            "vigil-addon-hw-amd64",
            "vigil-addon-hw-aarch64",
            "Install and smoke the exact artifact outputs",
            "published:false",
        ],
        violations,
    );
    if release_verified.is_empty() {
        violations.push(
            "release qualification must execute its material commands through the repository verifier"
                .to_string(),
        );
    }
    for forbidden in ["pull_request:", "--push", "docker push", "cargo publish"] {
        if release_commands.iter().any(|line| line.contains(forbidden)) {
            violations.push(format!(
                "release qualification must remain non-publishing; forbidden command `{forbidden}` found"
            ));
        }
    }
    if release.contains("cache_state:") || release.contains("inputs.cache_state") {
        violations.push(
            "release qualification must record observed cache state rather than accept a caller-supplied label"
                .to_string(),
        );
    }
}

fn workflow_run_scripts(content: &str) -> Vec<Vec<String>> {
    let lines = content.lines().collect::<Vec<_>>();
    let mut scripts = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start().trim_start_matches("- ");
        let Some(value) = trimmed.strip_prefix("run:") else {
            index += 1;
            continue;
        };
        let value = value.trim();
        if !value.is_empty() && !matches!(value, "|" | ">" | "|-" | ">-") {
            scripts.push(vec![value.to_string()]);
            index += 1;
            continue;
        }
        index += 1;
        let mut commands = Vec::new();
        while index < lines.len() {
            let command_line = lines[index];
            let command_indent = command_line.len() - command_line.trim_start().len();
            if !command_line.trim().is_empty() && command_indent <= indent {
                break;
            }
            let command = command_line.trim();
            if !command.is_empty() {
                commands.push(command.to_string());
            }
            index += 1;
        }
        scripts.push(commands);
    }
    scripts
}

fn workflow_run_command_lines(scripts: &[Vec<String>]) -> Vec<String> {
    scripts.iter().flatten().cloned().collect()
}

fn verified_workflow_commands(
    tier: &str,
    scripts: &[Vec<String>],
    launcher: &str,
    violations: &mut Vec<String>,
) -> Vec<String> {
    let mut commands = Vec::new();
    for script in scripts {
        let launcher_positions = script
            .iter()
            .enumerate()
            .filter(|(_, line)| line.as_str() == launcher)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if !launcher_positions.is_empty() && launcher_positions != [0] {
            violations.push(format!(
                "{tier} verifier must be the first and only command launcher in its run block"
            ));
            continue;
        }
        let mut consumed_command_starts = BTreeSet::new();
        for (position_index, launcher_position) in launcher_positions.iter().enumerate() {
            let end = launcher_positions
                .get(position_index + 1)
                .copied()
                .unwrap_or(script.len());
            let body = &script[launcher_position + 1..end];
            let Some(relative_command) = body
                .iter()
                .position(|line| line.starts_with("-- cargo ") || line.starts_with("-- docker "))
            else {
                violations.push(format!("{tier} verifier invocation has no exact command"));
                continue;
            };
            let command_index = launcher_position + 1 + relative_command;
            let options = &script[launcher_position + 1..command_index];
            if !options
                .iter()
                .all(|line| line.starts_with("--") && line.ends_with('\\'))
            {
                violations.push(format!(
                    "{tier} verifier invocation has shell statements inside its option continuation"
                ));
                continue;
            }
            let mut command_end = command_index;
            while command_end + 1 < end && script[command_end].ends_with('\\') {
                command_end += 1;
            }
            if script[command_end].ends_with('\\') || command_end + 1 != end {
                violations.push(format!(
                    "{tier} verifier material command must consume the remainder of its run block"
                ));
                continue;
            }
            consumed_command_starts.insert(command_index);
            commands.push(script[command_index].clone());
        }
        for (index, line) in script.iter().enumerate() {
            if (line.starts_with("-- cargo ") || line.starts_with("-- docker "))
                && !consumed_command_starts.contains(&index)
            {
                violations.push(format!(
                    "{tier} has a material command continuation outside a verified argv"
                ));
            }
        }
    }
    commands
}

fn audit_verify_launcher_text(content: &str, violations: &mut Vec<String>) {
    for required in [
        "git -C \"${repo_root}\" rev-parse --git-common-dir",
        "flock -n 9",
        "date +%s%N",
        "nanoseconds / 1000000",
        "/usr/bin/time",
        "CARGO_BUILD_JOBS=2",
        "VIGIL_VERIFY_BOOTSTRAP_TARGET_BYTES_BEFORE",
        "VIGIL_VERIFY_BOOTSTRAP_DISK_AVAILABLE_AFTER",
        "${repo_root}/.vigil-tools-target/debug/xtask\" verify",
    ] {
        if !content.contains(required) {
            violations.push(format!(
                "verification launcher must preserve resource and bootstrap measurement contract `{required}`"
            ));
        }
    }
    for forbidden in ["date +%s%3N", "cargo xtask verify"] {
        if content.contains(forbidden) {
            violations.push(format!(
                "verification launcher contains bypass or invalid timing form `{forbidden}`"
            ));
        }
    }
}

fn audit_dev_integration_workflow(content: &str, violations: &mut Vec<String>) {
    let active = active_workflow_text(content);
    require_workflow_text(
        "dev integration",
        &active,
        &[
            "workflow_dispatch:",
            "actions: read",
            "contents: write",
            "test \"$GITHUB_REF\" = \"refs/heads/dev\"",
            "test \"$GITHUB_SHA\" = \"$DEV_BASE_SHA\"",
            ".default_branch')\" = dev",
            ".enforce_admins.enabled == true",
            ".allow_force_pushes.enabled == false",
            ".required_linear_history.enabled == true",
            ".context == \"vigil/dev-closeout\" and (.app_id // 0) > 0",
            "actions/workflows/dev-closeout.yml",
            ".workflow_id == $workflow_id and .event == \"workflow_dispatch\"",
            ".head_branch == \"dev\" and .head_sha == $dev_base_sha and .conclusion == \"success\"",
            "run-id: ${{ inputs.closeout_run_id }}",
            "name: dev-closeout-sources",
            "name: dev-closeout-impact",
            "name: dev-closeout-qualified-${{ inputs.vigil_sha }}",
            ".qualified == true and .fast_forward_only == true",
            "git merge-base \"$DEV_BASE_SHA\" \"$VIGIL_SHA\"",
            "normal feature integration cannot change trusted verification control plane",
            ".github/workflows/*",
            "Cargo.lock|Cargo.toml",
            "xtask/*",
            "read -r -d '' path",
            "--name-only --no-renames -z --diff-filter=ACDMRTUXB",
            "repos/context-graph-ai/context-graph/git/ref/heads/dev",
            "repos/context-graph-ai/contextdb/git/ref/heads/dev",
            "repos/${GITHUB_REPOSITORY}/git/refs/heads/dev",
            "--field force=false",
        ],
        violations,
    );
    for forbidden in ["pull_request:", "--field force=true", "git push --force"] {
        if active.contains(forbidden) {
            violations.push(format!(
                "dev integration must remain receipt-bound and non-forcing; forbidden form `{forbidden}` found"
            ));
        }
    }
}

#[cfg(test)]
fn audit_ci_text(content: &str, violations: &mut Vec<String>) {
    let active_lines = content
        .lines()
        .map(|line| line.split_once('#').map_or(line, |(active, _)| active))
        .collect::<Vec<_>>();
    let active = active_lines.join("\n");
    if !active.contains(
        "cargo xtask test-estate-check --nextest-json default=target/nextest-default.json",
    ) {
        violations.push(
            "CI fast lane must compare exact default nextest identities with `--nextest-json default=target/nextest-default.json`"
                .to_string(),
        );
    }
    if !active.contains("cargo nextest list --workspace -T json > target/nextest-default.json") {
        violations.push("CI fast lane must emit authoritative default nextest JSON".to_string());
    }
    if !active.contains("cargo nextest list -p vigil --features ${{ matrix.features }} -T json > target/nextest-${{ matrix.shape }}.json")
        || !active.contains("--nextest-json ${{ matrix.shape }}=target/nextest-${{ matrix.shape }}.json")
    {
        violations.push(
            "CI feature lanes must emit and compare exact nextest identities for each named matrix shape"
                .to_string(),
        );
    }
    let feature_inventory_command = "cargo nextest list -p vigil --features ${{ matrix.features }} -T json > target/nextest-${{ matrix.shape }}.json";
    if let Some(command_index) = active_lines
        .iter()
        .position(|line| line.contains(feature_inventory_command))
    {
        let step_start = (0..=command_index)
            .rev()
            .find(|index| active_lines[*index].trim_start().starts_with("- "))
            .unwrap_or(command_index);
        let conditionally_skippable = active_lines[step_start..=command_index].iter().any(|line| {
            line.trim_start()
                .trim_start_matches("- ")
                .trim_start()
                .starts_with("if:")
        });
        if conditionally_skippable {
            violations.push(
                "CI must enumerate and compare the exact production-union identities; the shipped feature shape cannot skip its frozen registry check"
                    .to_string(),
            );
        }
    }
    if !active.contains("cargo nextest list --workspace --features first-light-acceptance,acceptance -T json > target/nextest-slow.json")
        || !active.contains("--nextest-json slow=target/nextest-slow.json")
    {
        violations.push("CI slow lane must emit and compare its exact nextest identity union".to_string());
    }
    if active_lines.iter().any(|line| line.contains("--retries")) {
        violations.push("CI must not pass a nextest `--retries` override".to_string());
    }
    let bounded_apt = "sudo timeout --kill-after=30s 5m apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30";
    if active_lines
        .iter()
        .filter(|line| line.contains("apt-get"))
        .any(|line| line.matches("apt-get").count() != line.matches(bounded_apt).count())
    {
        violations.push(
            "CI package installation must bound mirror retries, network waits, and total apt runtime so a hosted-runner mirror cannot wedge a job"
                .to_string(),
        );
    }
    for required in [
        "name: Private dependency boundary",
        "github.event.pull_request.head.repo.full_name != github.repository",
        "token: ${{ secrets.CG_CI_TOKEN }}",
        "persist-credentials: false",
        "name: PR test-estate gate",
        "needs: [private-dependency-boundary, fast, feature-lanes]",
        "test \"${{ needs.private-dependency-boundary.result }}\" = \"success\"",
        "test \"${{ needs.fast.result }}\" = \"success\"",
        "test \"${{ needs.feature-lanes.result }}\" = \"success\"",
    ] {
        if !active.contains(required) {
            violations.push(format!(
                "CI must preserve the stable always-run aggregate gate contract `{required}`"
            ));
        }
    }
    let rust_cache_steps = active.matches("uses: Swatinem/rust-cache@v2").count();
    let epoch_keyed_rust_caches = active
        .matches("key: ${{ inputs.cache_epoch || 'rolling' }}")
        .count();
    if !active.contains("cache_epoch:")
        || !active.contains("cache_epoch:$cache_epoch")
        || rust_cache_steps == 0
        || epoch_keyed_rust_caches != rust_cache_steps
        || !active
            .contains("scope=vigil-hw-${{ matrix.arch }}-${{ inputs.cache_epoch || 'rolling' }}")
    {
        violations.push(
            "manual CI must record one explicit cache epoch and apply it to every Rust and hardware-image cache so cold/warm measurements are reproducible"
                .to_string(),
        );
    }
    if active.contains("secrets.CG_CI_TOKEN || github.token")
        || active.contains("pull_request_target")
    {
        violations.push(
            "CI must fail closed at the private dependency boundary; it must not fall back to github.token or execute fork code through pull_request_target"
                .to_string(),
        );
    }
    for required in [
        "path: vigil/dist/downloaded",
        "dist/docker/amd64/vigil",
        "dist/docker/arm64/vigil",
    ] {
        if !active.contains(required) {
            violations.push(format!(
                "CI must preserve the bounded Docker build-context contract `{required}`"
            ));
        }
    }
    let hardware_start = active_lines
        .iter()
        .position(|line| line.trim() == "hardware-images:");
    let hardware_build = hardware_start.and_then(|start| {
        active_lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, line)| line.contains("docker buildx build"))
            .map(|(index, _)| (start, index))
    });
    let bounded_hardware_context = hardware_build.is_some_and(|(start, build)| {
        let scoped = active_lines[start..].join("\n");
        let has_scoped_contract = [
            "--exclude='*/target'",
            "--exclude='*/target/**'",
            "--exclude='*/.git/**'",
            "--exclude='*/tests/fixtures/.cache/**'",
            "--file vigil/Dockerfile.hardware",
        ]
        .iter()
        .all(|required| scoped.contains(required));
        let tar_pipe = active_lines[start..build].iter().rev().find(|line| {
            let trimmed = line.trim();
            trimmed.contains("-cf - vigil context-graph contextdb") && trimmed.ends_with('|')
        });
        let mut last_argument = build;
        for (index, line) in active_lines.iter().enumerate().skip(build + 1) {
            let trimmed = line.trim();
            if trimmed.starts_with("--") || trimmed == "-" || trimmed == "." {
                last_argument = index;
            } else {
                break;
            }
        }
        has_scoped_contract
            && tar_pipe.is_some()
            && active_lines[last_argument].trim() == "-"
            && !active_lines[build..=last_argument]
                .iter()
                .any(|line| line.trim() == ".")
    });
    if !bounded_hardware_context {
        violations.push(
            "hardware Docker build must consume the explicitly filtered Vigil/Context Graph/ContextDB tar stream from stdin; a workspace `.` context is forbidden"
                .to_string(),
        );
    }
    if active.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("docker_config") && lower.contains("target")
            || lower.contains("target") && lower.contains("cli-plugins/docker-buildx")
    }) {
        violations.push(
            "CI must not store Docker buildx discovery state under disposable Cargo target directories"
                .to_string(),
        );
    }
    for line in &active_lines {
        let normalized = line.to_ascii_lowercase().replace(['\'', '"'], "");
        let routes_build_output = normalized.contains("cargo_target_dir")
            || normalized.contains("--target-dir")
            || normalized.contains("target-dir");
        let routes_to_temp = normalized.contains("/tmp")
            || normalized.contains("$runner_temp")
            || normalized.contains("runner.temp");
        if routes_build_output && routes_to_temp {
            violations.push(format!(
                "CI must not route Cargo/build targets to tmpfs or runner temp: `{}`",
                line.trim()
            ));
        }
    }

    let feature_section = ci_job_section(&active, "feature-lanes", "test-estate-gate");
    let disk_check =
        feature_section.find("available_kb=\"$(df --output=avail vigil/target | tail -n 1)\"");
    let feature_build = active.find("cargo nextest run --profile pr -p vigil --features");
    if !has_cache_aware_disk_check(feature_section)
        || disk_check.is_none()
        || feature_build.is_none()
        || active
            .find("feature-lanes:")
            .zip(disk_check)
            .map(|(start, check)| start + check)
            >= feature_build
    {
        violations.push(
            "CI feature builds require a cache-aware post-restore disk check with at least 16 GiB real scratch and 30 GiB free-plus-restored-target capacity"
                .to_string(),
        );
    }
    for (job, next_job, build) in [
        (
            "full-tests",
            "static-musl",
            "cargo build -p vigil --release",
        ),
        (
            "static-musl",
            "docker",
            "cargo build --release --target x86_64-unknown-linux-musl",
        ),
    ] {
        let section = ci_job_section(&active, job, next_job);
        let check = section.find("available_kb=\"$(df --output=avail vigil/target | tail -n 1)\"");
        let build = section.find(build);
        if !has_cache_aware_disk_check(section)
            || check.is_none()
            || build.is_none()
            || check >= build
        {
            violations.push(format!(
                "CI `{job}` release builds require a cache-aware post-restore disk check with at least 16 GiB real scratch and 30 GiB free-plus-restored-target capacity"
            ));
        }
    }

    let features = ci_feature_matrix(&active_lines);
    for required in [
        "decode-gstreamer",
        "detect-burn-wgpu",
        "decode-gstreamer,detect-burn-wgpu",
        "fabric",
        "decode-gstreamer,detect-burn-wgpu,fabric",
    ] {
        if !features.contains(required) {
            violations.push(format!(
                "CI feature matrix must include the exact `{required}` shape"
            ));
        }
    }
    for (shape, feature) in [
        ("decode", "decode-gstreamer"),
        ("detect", "detect-burn-wgpu"),
        ("combined", "decode-gstreamer,detect-burn-wgpu"),
        ("fabric", "fabric"),
        ("production", "decode-gstreamer,detect-burn-wgpu,fabric"),
    ] {
        let mapped = active_lines.windows(2).any(|lines| {
            lines[0].trim().trim_start_matches("- ") == format!("shape: {shape}")
                && lines[1].trim() == format!("features: {feature}")
        });
        if !mapped {
            violations.push(format!(
                "CI feature matrix must map nextest shape `{shape}` to exact features `{feature}`"
            ));
        }
    }

    for target in [
        "vigil-hw-binary-amd64",
        "vigil-hw-binary-arm64",
        "vigil-generic-docker-hw-amd64",
        "vigil-generic-docker-hw-arm64",
        "vigil-addon-hw-amd64",
        "vigil-addon-hw-aarch64",
    ] {
        if !active_lines.iter().any(|line| {
            let line = line.trim().trim_start_matches("- ").trim();
            line == target
                || line == format!("target: {target}")
                || line.ends_with(&format!("_target: {target}"))
        }) {
            violations.push(format!(
                "CI must build the exact named hardware image target `{target}`"
            ));
        }
    }
    let has_buildx_target = active_lines.windows(8).any(|window| {
        window
            .iter()
            .any(|line| line.contains("docker buildx build"))
            && window.iter().any(|line| {
                line.contains("--target ${{ matrix.target }}")
                    || line.contains("--target vigil-generic-docker-hw-")
                    || line.contains("--target \"${target}\"")
            })
    });
    if !has_buildx_target {
        violations.push(
            "CI must pass `--target` to `docker buildx build`; release-note identifiers are not image-build evidence"
                .to_string(),
        );
    }

    let static_section = ci_job_section(&active, "static-musl", "docker");
    for target in ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"] {
        let build = static_section
            .lines()
            .find(|line| line.contains("cargo build --release --target") && line.contains(target));
        if build.is_none_or(|line| {
            !line.contains("--features fabric")
                || line.contains("decode-gstreamer")
                || line.contains("detect-burn-wgpu")
        }) {
            violations.push(format!(
                "static musl artifact `{target}` must compile fabric while remaining free of decode-gstreamer and detect-burn-wgpu"
            ));
        }
    }

    let hardware_section = ci_job_section(&active, "hardware-images", "");
    for required in [
        "type=local,dest=${output_dir}/binary",
        "vigil-generic-docker-hw-${{ matrix.arch }}.oci.tar",
        "vigil-addon-hw-${{ matrix.arch }}.oci.tar",
        "name: vigil-production-${{ matrix.arch }}",
        "if-no-files-found: error",
    ] {
        if !hardware_section.contains(required) {
            violations.push(format!(
                "hardware release CI must export and upload the exact production artifact contract `{required}`"
            ));
        }
    }
    if hardware_section.contains("addons/vigil/vigil") {
        violations.push(
            "hardware release CI must build the Home Assistant OCI image from the deterministic production-binary target, not an externally staged addons/vigil/vigil file"
                .to_string(),
        );
    }
}

#[cfg(test)]
fn ci_job_section<'a>(content: &'a str, job: &str, next_job: &str) -> &'a str {
    let Some((_, tail)) = content.split_once(&format!("  {job}:")) else {
        return "";
    };
    tail.split_once(&format!("  {next_job}:"))
        .map_or(tail, |(section, _)| section)
}

#[cfg(test)]
fn has_cache_aware_disk_check(section: &str) -> bool {
    [
        "du -sh vigil/target",
        "target_kb=\"$(du -sk vigil/target | awk '{print $1}')\"",
        "available_kb=\"$(df --output=avail vigil/target | tail -n 1)\"",
        "effective_capacity_kb=$((available_kb + target_kb))",
        "scratch_floor_kb=16777216",
        "capacity_floor_kb=31457280",
        "[ \"${available_kb}\" -lt \"${scratch_floor_kb}\" ]",
        "[ \"${effective_capacity_kb}\" -lt \"${capacity_floor_kb}\" ]",
    ]
    .iter()
    .all(|required| section.contains(required))
}

#[cfg(test)]
fn ci_feature_matrix(lines: &[&str]) -> BTreeSet<String> {
    let mut features = BTreeSet::new();
    let mut matrix_indent = None::<usize>;
    let mut features_indent = None::<usize>;
    for line in lines {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(base_indent) = features_indent {
            if indent > base_indent {
                if let Some(value) = trimmed.strip_prefix("- ") {
                    features.insert(value.trim_matches(['\'', '"']).to_string());
                }
                continue;
            }
            features_indent = None;
        }
        if let Some(base_indent) = matrix_indent
            && indent <= base_indent
        {
            matrix_indent = None;
        }
        if trimmed == "matrix:" {
            matrix_indent = Some(indent);
        } else if let Some(value) = trimmed.strip_prefix("features:")
            && matrix_indent.is_some_and(|base_indent| indent > base_indent)
            && !value.trim().is_empty()
        {
            features.insert(value.trim().trim_matches(['\'', '"']).to_string());
        } else if trimmed == "features:"
            && matrix_indent.is_some_and(|base_indent| indent > base_indent)
        {
            features_indent = Some(indent);
        }
    }
    features
}

fn audit_doc_bindings(
    root: &Path,
    docs_root: &Path,
    contracts: &DocumentationContracts,
    violations: &mut Vec<String>,
) -> Result<(), String> {
    let tests = collect_test_identities(root)?;
    let groups = collect_documentation_groups(docs_root, true, violations)?;
    if groups.is_empty() {
        violations.push(format!(
            "no enforcement tags found below {}",
            docs_root.display()
        ));
        return Ok(());
    }

    for binding in &groups {
        for test in &binding.tests {
            if !tests.contains(test) {
                violations.push(format!(
                    "{}:{} binds unknown or non-canonical test identity `{test}`",
                    binding.document, binding.line
                ));
            }
        }
    }
    let claimed = groups
        .iter()
        .filter(|group| !group.id.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    audit_contract_mappings(&claimed, contracts, &tests, violations);

    let binding_count = groups.iter().map(|group| group.tests.len()).sum::<usize>();
    let pending = contracts
        .claim
        .iter()
        .filter(|claim| !claim.reviewed)
        .count();
    println!(
        "documentation contracts checked: {} active semantic mappings, {binding_count} canonical test bindings; {pending} independently review-pending",
        claimed.len()
    );
    Ok(())
}

fn audit_contract_mappings(
    groups: &[DocumentationBinding],
    contracts: &DocumentationContracts,
    tests: &BTreeSet<String>,
    violations: &mut Vec<String>,
) {
    let mut registry = BTreeMap::<String, &DocumentationClaim>::new();
    for claim in &contracts.claim {
        validate_registered_claim(claim, tests, violations);
        if registry.insert(claim.id.clone(), claim).is_some() {
            violations.push(format!(
                "documentation claim `{}` is duplicated in .config/documentation-contracts.toml",
                claim.id
            ));
        }
    }

    let mut observed = BTreeMap::<String, &DocumentationBinding>::new();
    for binding in groups {
        if observed.insert(binding.id.clone(), binding).is_some() {
            violations.push(format!(
                "documentation claim `{}` is bound more than once",
                binding.id
            ));
            continue;
        }
        let Some(claim) = registry.get(&binding.id) else {
            violations.push(format!(
                "{}:{} binds unregistered documentation claim `{}`",
                binding.document, binding.line, binding.id
            ));
            continue;
        };
        if binding.document != claim.document {
            violations.push(format!(
                "documentation claim `{}` moved to {}; registry requires {}",
                binding.id, binding.document, claim.document
            ));
        }
        if binding.paragraph_sha256 != claim.paragraph_sha256 {
            violations.push(format!(
                "{}:{} changed the paragraph for documentation claim `{}`: observed {}, registry requires {}",
                binding.document,
                binding.line,
                binding.id,
                binding.paragraph_sha256,
                claim.paragraph_sha256
            ));
        }
        let allowed = claim.tests.iter().cloned().collect::<BTreeSet<_>>();
        if binding.tests != allowed {
            let added = binding
                .tests
                .difference(&allowed)
                .cloned()
                .collect::<Vec<_>>();
            let missing = allowed
                .difference(&binding.tests)
                .cloned()
                .collect::<Vec<_>>();
            violations.push(format!(
                "{}:{} uses a disallowed test mapping for documentation claim `{}`; added={added:?}, missing={missing:?}",
                binding.document, binding.line, binding.id
            ));
        }
    }
    for id in registry.keys() {
        if !observed.contains_key(id) {
            violations.push(format!(
                "registered documentation claim `{id}` is missing from its active mapped paragraph"
            ));
        }
    }
}

fn validate_registered_claim(
    claim: &DocumentationClaim,
    canonical_tests: &BTreeSet<String>,
    violations: &mut Vec<String>,
) {
    if !valid_claim_id(&claim.id) {
        violations.push(format!(
            "documentation claim ID {:?} must use stable lowercase dotted words",
            claim.id
        ));
    }
    if !valid_document_path(&claim.document) {
        violations.push(format!(
            "documentation claim `{}` has unsafe or non-canonical document path {:?}",
            claim.id, claim.document
        ));
    }
    if !is_sha256(&claim.paragraph_sha256) {
        violations.push(format!(
            "documentation claim `{}` has invalid paragraph_sha256 {:?}",
            claim.id, claim.paragraph_sha256
        ));
    }
    if claim.reviewed && !valid_completed_review_attestation(&claim.reviewed_by) {
        violations.push(format!(
            "documentation claim `{}` claims independent review without a valid {} evidence-tier attestation",
            claim.id,
            claim.evidence_tier.as_str()
        ));
    } else if !claim.reviewed {
        violations.push(format!(
            "documentation claim `{}` has no completed independent semantic review",
            claim.id
        ));
        if !claim
            .reviewed_by
            .to_ascii_lowercase()
            .contains("review pending")
        {
            violations.push(format!(
                "documentation claim `{}` is executable but review-pending status is not explicit",
                claim.id
            ));
        }
    }
    if !valid_review_attestation(&claim.rationale) {
        violations.push(format!(
            "documentation claim `{}` has an empty or placeholder semantic-mapping rationale",
            claim.id
        ));
    }
    if claim.tests.is_empty() {
        violations.push(format!(
            "documentation claim `{}` allows no canonical tests",
            claim.id
        ));
    }
    let mut unique = BTreeSet::new();
    for test in &claim.tests {
        if !unique.insert(test) {
            violations.push(format!(
                "documentation claim `{}` repeats test identity `{test}`",
                claim.id
            ));
        }
        if !canonical_tests.contains(test) {
            violations.push(format!(
                "documentation claim `{}` allows renamed, missing, or non-canonical test `{test}`",
                claim.id
            ));
        }
    }
}

fn valid_review_attestation(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    !value.is_empty()
        && !["todo", "tbd", "placeholder", "unreviewed"]
            .iter()
            .any(|marker| value.contains(marker))
}

fn valid_completed_review_attestation(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    valid_review_attestation(value)
        && !["pending", "proposed", "proposal", "review required"]
            .iter()
            .any(|marker| normalized.contains(marker))
}

fn audit_contract_paragraph_coverage(
    docs_root: &Path,
    violations: &mut Vec<String>,
) -> Result<(), String> {
    let mut docs = Vec::new();
    collect_extension(docs_root, "md", &mut docs)?;
    docs.sort();
    let mut covered = 0usize;
    let mut explicitly_unenforced = 0usize;
    for path in docs {
        let document = relative(docs_root, &path)?;
        if document != "README.md" && !document.starts_with("docs/") {
            continue;
        }
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let lines = content.lines().collect::<Vec<_>>();
        let mut index = 0usize;
        let mut in_fence = false;
        while index < lines.len() {
            let trimmed = lines[index].trim();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
                index += 1;
                continue;
            }
            if in_fence || trimmed.is_empty() {
                index += 1;
                continue;
            }
            let start = index;
            while index < lines.len() && !lines[index].trim().is_empty() {
                if lines[index].trim().starts_with("```") || lines[index].trim().starts_with("~~~")
                {
                    break;
                }
                index += 1;
            }
            let block = &lines[start..index];
            if let Some(marker_index) = block
                .iter()
                .position(|line| line.trim_start().starts_with(UNENFORCED_TAG_PREFIX))
            {
                let marker_count = block
                    .iter()
                    .filter(|line| line.trim_start().starts_with(UNENFORCED_TAG_PREFIX))
                    .count();
                let inline_contract = contract_bearing_paragraph(&block[..marker_index]);
                if inline_contract && marker_count == 1 {
                    if parse_unenforced_tag(block[marker_index]).is_some() {
                        explicitly_unenforced += 1;
                    } else {
                        violations.push(format!(
                            "{document}:{} has malformed vigil-unenforced metadata; require `classification=<allowed>; reason=`specific reason``",
                            start + marker_index + 1
                        ));
                    }
                } else if inline_contract {
                    violations.push(format!(
                        "{document}:{} contract-bearing paragraph must have exactly one vigil-unenforced marker",
                        start + 1
                    ));
                }
                continue;
            }
            if block.iter().any(|line| is_contract_tag_line(line))
                || !contract_bearing_paragraph(block)
            {
                continue;
            }
            let mut tag = index;
            while tag < lines.len() && lines[tag].trim().is_empty() {
                tag += 1;
            }
            if tag < lines.len() && is_contract_tag_line(lines[tag]) {
                covered += 1;
                continue;
            }
            if tag < lines.len() && lines[tag].trim_start().starts_with(UNENFORCED_TAG_PREFIX) {
                if parse_unenforced_tag(lines[tag]).is_some() {
                    explicitly_unenforced += 1;
                } else {
                    violations.push(format!(
                        "{document}:{} has malformed vigil-unenforced metadata; require `classification=<allowed>; reason=`specific reason``",
                        tag + 1
                    ));
                }
                continue;
            }
            violations.push(format!(
                "{document}:{} contract-bearing paragraph has config/default/HTTP/schema/current/no/does-not language but no adjacent claim+enforced-by block or strict vigil-unenforced classification",
                start + 1
            ));
        }
    }
    println!(
        "documentation coverage checked: {covered} contract-bearing paragraphs enforced, {explicitly_unenforced} explicitly classified unenforced"
    );
    Ok(())
}

fn contract_bearing_paragraph(lines: &[&str]) -> bool {
    let trimmed = lines.iter().map(|line| line.trim()).collect::<Vec<_>>();
    if trimmed.is_empty()
        || trimmed.iter().all(|line| line.starts_with('#'))
        || trimmed
            .iter()
            .all(|line| line.starts_with("<!--") && line.ends_with("-->"))
    {
        return false;
    }
    let text = trimmed.join(" ").to_ascii_lowercase();
    let words = text
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-')
        .collect::<BTreeSet<_>>();
    text.contains("config")
        || text.contains("default")
        || text.contains("does not")
        || text.contains("does-not")
        || words.contains("http")
        || words.contains("schema")
        || words.contains("current")
        || words.contains("no")
        || words.contains("not")
        || words.contains("must")
        || words.contains("never")
        || words.contains("disabled")
        || words.contains("enabled")
        || words.contains("supports")
        || words.contains("unavailable")
        || words.contains("future")
        || words.contains("release")
}

fn parse_unenforced_tag(line: &str) -> Option<(&str, &str)> {
    let body = line
        .trim()
        .strip_prefix(UNENFORCED_TAG_PREFIX)?
        .strip_suffix("-->")?
        .trim();
    let (classification, reason) = body.split_once("; reason=")?;
    let classification = classification.strip_prefix("classification=")?.trim();
    if ![
        "documentation-gap",
        "external-procedure",
        "future-surface",
        "implementation-blocker",
        "non-contract-context",
        "product-decision",
    ]
    .contains(&classification)
    {
        return None;
    }
    let reason = reason.trim().strip_prefix('`')?.strip_suffix('`')?.trim();
    if reason.len() < 20 || !valid_review_attestation(reason) {
        return None;
    }
    Some((classification, reason))
}

fn collect_documentation_groups(
    docs_root: &Path,
    require_claim: bool,
    violations: &mut Vec<String>,
) -> Result<Vec<DocumentationBinding>, String> {
    let mut docs = Vec::new();
    collect_extension(docs_root, "md", &mut docs)?;
    docs.sort();
    if docs.is_empty() {
        violations.push(format!(
            "documentation root contains no Markdown: {}",
            docs_root.display()
        ));
        return Ok(Vec::new());
    }
    let mut bindings = Vec::new();
    for path in docs {
        let document = relative(docs_root, &path)?;
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let lines = content.lines().collect::<Vec<_>>();
        let mut index = 0usize;
        while index < lines.len() {
            if !is_contract_tag_line(lines[index]) {
                index += 1;
                continue;
            }
            let start = index;
            while index < lines.len() && is_contract_tag_line(lines[index]) {
                index += 1;
            }
            let block = &lines[start..index];
            let claim_ids = block
                .iter()
                .filter_map(|line| parse_claim_tag(line))
                .collect::<Vec<_>>();
            let raw_claim_tags = block
                .iter()
                .filter(|line| line.trim_start().starts_with(CLAIM_TAG_PREFIX))
                .count();
            if raw_claim_tags != claim_ids.len() {
                violations.push(format!(
                    "{document}:{} has a malformed claim tag; use exactly `<!-- vigil-claim: `stable.id` -->`",
                    start + 1
                ));
                continue;
            }
            let enforcement_lines = block
                .iter()
                .filter(|line| line.trim_start().starts_with(ENFORCEMENT_TAG_PREFIX))
                .copied()
                .collect::<Vec<_>>();
            if enforcement_lines.is_empty() {
                violations.push(format!(
                    "{document}:{} has a claim tag without adjacent enforced-by tags",
                    start + 1
                ));
                continue;
            }
            let Some(paragraph) = adjacent_paragraph(&lines, start) else {
                violations.push(format!(
                    "{document}:{} has enforcement tags without an immediately adjacent paragraph",
                    start + 1
                ));
                continue;
            };
            if require_claim && claim_ids.len() != 1 {
                violations.push(format!(
                    "{document}:{} must bind exactly one `<!-- vigil-claim: `stable.id` -->`; found {}",
                    start + 1,
                    claim_ids.len()
                ));
                continue;
            }
            if !require_claim && claim_ids.len() > 1 {
                violations.push(format!(
                    "{document}:{} binds more than one claim ID",
                    start + 1
                ));
                continue;
            }
            let mut mapped_tests = BTreeSet::new();
            for line in enforcement_lines {
                let trimmed = line.trim();
                if !trimmed.ends_with("-->") {
                    violations.push(format!(
                        "{document}:{} has an unterminated enforcement tag",
                        start + 1
                    ));
                    continue;
                }
                let names = backtick_values(trimmed);
                if names.is_empty() {
                    violations.push(format!(
                        "{document}:{} has an enforcement tag without backticked test identities",
                        start + 1
                    ));
                }
                for name in names {
                    if !mapped_tests.insert(name.clone()) {
                        violations.push(format!(
                            "{document}:{} repeats test identity `{name}` in one claim",
                            start + 1
                        ));
                    }
                }
            }
            let id = claim_ids.first().cloned().unwrap_or_default();
            bindings.push(DocumentationBinding {
                id,
                document: document.clone(),
                paragraph: paragraph.clone(),
                paragraph_sha256: paragraph_digest(&paragraph),
                tests: mapped_tests,
                first_enforcement_tag: block
                    .iter()
                    .find(|line| line.trim_start().starts_with(ENFORCEMENT_TAG_PREFIX))
                    .map(|line| line.to_string())
                    .unwrap_or_default(),
                line: start + 1,
            });
        }
    }
    Ok(bindings)
}

fn is_contract_tag_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with(CLAIM_TAG_PREFIX) || trimmed.starts_with(ENFORCEMENT_TAG_PREFIX)
}

fn parse_claim_tag(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let body = trimmed
        .strip_prefix(CLAIM_TAG_PREFIX)?
        .strip_suffix("-->")?
        .trim();
    let values = backtick_values(body);
    (values.len() == 1 && body == format!("`{}`", values[0])).then(|| values[0].clone())
}

fn adjacent_paragraph(lines: &[&str], tag_start: usize) -> Option<String> {
    if tag_start == 0 {
        return None;
    }
    let mut end = tag_start;
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let mut start = end - 1;
    while start > 0 && !lines[start - 1].trim().is_empty() {
        start -= 1;
    }
    let paragraph = lines[start..end].join("\n");
    let normalized = normalize_paragraph(&paragraph);
    (!normalized.is_empty()).then_some(normalized)
}

fn normalize_paragraph(paragraph: &str) -> String {
    paragraph.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn paragraph_digest(normalized_paragraph: &str) -> String {
    syntax_fingerprint(normalized_paragraph)
}

fn valid_claim_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && id.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        })
}

fn valid_document_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && Path::new(path).extension().and_then(|value| value.to_str()) == Some("md")
}

fn is_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.chars().all(|ch| ch.is_ascii_hexdigit()))
}

fn proposed_claim_id(document: &str, paragraph: &str, used: &mut BTreeSet<String>) -> String {
    let stem = document
        .trim_end_matches(".md")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let stem = stem.trim_matches('-').replace("--", "-");
    let subject = paragraph
        .split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|ch| ch.is_ascii_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .take(7)
        .collect::<Vec<_>>()
        .join("-");
    let subject = if subject.is_empty() {
        "reviewed-claim".to_string()
    } else {
        subject
    };
    let base = format!("vigil.{stem}.{subject}");
    let mut id = base.clone();
    let mut suffix = 2usize;
    while !used.insert(id.clone()) {
        id = format!("{base}-{suffix}");
        suffix += 1;
    }
    id
}

fn backtick_values(input: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else {
            break;
        };
        values.push(after[..end].trim().to_string());
        rest = &after[end + 1..];
    }
    values
}

fn collect_test_identities(root: &Path) -> Result<BTreeSet<String>, String> {
    Ok(collect_test_records(root)?
        .into_iter()
        .map(|record| record.identity)
        .collect())
}

fn collect_test_records(root: &Path) -> Result<Vec<TestRecord>, String> {
    let mut records = BTreeSet::new();
    let source = root.join("crates/vigil/src");
    for crate_root in [source.join("lib.rs"), source.join("main.rs")] {
        if crate_root.is_file() {
            collect_tests_from_module_file(
                root,
                &crate_root,
                vec!["vigil".to_string()],
                &mut records,
                &mut BTreeSet::new(),
            )?;
        }
    }

    let integration = root.join("crates/vigil/tests");
    let mut roots = fs::read_dir(&integration)
        .map_err(|error| format!("read {}: {error}", integration.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|part| part.to_str()) == Some("rs"))
        .collect::<Vec<_>>();
    roots.sort();
    for path in roots {
        let stem = path
            .file_stem()
            .and_then(|part| part.to_str())
            .ok_or_else(|| format!("non-UTF8 integration test path: {}", path.display()))?;
        collect_tests_from_module_file(
            root,
            &path,
            vec!["vigil".to_string(), stem.to_string()],
            &mut records,
            &mut BTreeSet::new(),
        )?;
    }

    let acceptance = root.join("tests/acceptance.rs");
    if acceptance.is_file() {
        collect_tests_from_module_file(
            root,
            &acceptance,
            vec!["vigil-acceptance".to_string(), "acceptance".to_string()],
            &mut records,
            &mut BTreeSet::new(),
        )?;
    }
    let xtask = root.join("xtask/src/main.rs");
    if xtask.is_file() {
        collect_tests_from_module_file(
            root,
            &xtask,
            vec!["xtask".to_string()],
            &mut records,
            &mut BTreeSet::new(),
        )?;
    }
    Ok(records.into_iter().collect())
}

fn collect_tests_from_module_file(
    root: &Path,
    path: &Path,
    modules: Vec<String>,
    records: &mut BTreeSet<TestRecord>,
    stack: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("resolve {}: {error}", path.display()))?;
    if !stack.insert(canonical.clone()) {
        return Err(format!(
            "Rust test module include cycle at {}",
            path.display()
        ));
    }
    let rel = relative(root, &canonical)?;
    let source = fs::read_to_string(&canonical)
        .map_err(|error| format!("read {}: {error}", canonical.display()))?;
    let syntax =
        syn::parse_file(&source).map_err(|error| format!("parse Rust syntax in {rel}: {error}"))?;
    collect_item_test_records(root, &canonical, &syntax.items, &modules, records, stack)?;
    stack.remove(&canonical);
    Ok(())
}

fn collect_item_test_records(
    root: &Path,
    containing_file: &Path,
    items: &[syn::Item],
    modules: &[String],
    records: &mut BTreeSet<TestRecord>,
    stack: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    for item in items {
        match item {
            syn::Item::Fn(function) if is_test(&function.attrs) => {
                let mut parts = modules.to_vec();
                parts.push(function.sig.ident.to_string());
                records.insert(TestRecord {
                    identity: parts.join("::"),
                    source: relative(root, containing_file)?,
                    ignored: has_ignore(&function.attrs),
                });
            }
            syn::Item::Mod(module) => {
                let mut parts = modules.to_vec();
                parts.push(module.ident.to_string());
                if let Some((_, nested)) = &module.content {
                    collect_item_test_records(
                        root,
                        containing_file,
                        nested,
                        &parts,
                        records,
                        stack,
                    )?;
                } else if let Some(path) = resolve_module_path(containing_file, module) {
                    collect_tests_from_module_file(root, &path, parts, records, stack)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn has_ignore(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident("ignore"))
}

fn resolve_module_path(containing_file: &Path, module: &ItemMod) -> Option<PathBuf> {
    let parent = containing_file.parent()?;
    if let Some(explicit) = module.attrs.iter().find_map(path_attribute) {
        return Some(parent.join(explicit));
    }
    let stem = containing_file.file_stem()?.to_str()?;
    let module_dir = if matches!(stem, "lib" | "main" | "mod") {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    let flat = module_dir.join(format!("{}.rs", module.ident));
    if flat.is_file() {
        Some(flat)
    } else {
        let nested = module_dir.join(module.ident.to_string()).join("mod.rs");
        nested.is_file().then_some(nested)
    }
}

fn path_attribute(attr: &Attribute) -> Option<String> {
    if !attr.path().is_ident("path") {
        return None;
    }
    match &attr.meta {
        Meta::NameValue(value) => match &value.value {
            Expr::Lit(ExprLit {
                lit: Lit::Str(path),
                ..
            }) => Some(path.value()),
            _ => None,
        },
        _ => None,
    }
}

fn is_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let parts: Vec<_> = attr
            .path()
            .segments
            .iter()
            .map(|part| part.ident.to_string())
            .collect();
        parts.last().is_some_and(|part| part == "test")
    })
}

fn collect_rs(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    collect_extension(directory, "rs", files)
}

fn collect_extension(
    directory: &Path,
    extension: &str,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("read directory {}: {error}", directory.display()))?
    {
        let path = entry
            .map_err(|error| format!("read directory entry in {}: {error}", directory.display()))?
            .path();
        if path.is_dir() {
            collect_extension(&path, extension, files)?;
        } else if path.extension().and_then(|part| part.to_str()) == Some(extension) {
            files.push(path);
        }
    }
    Ok(())
}

fn relative(root: &Path, path: &Path) -> Result<String, String> {
    path.strip_prefix(root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .map_err(|error| format!("{} is outside {}: {error}", path.display(), root.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect(source: &str) -> AuditVisitor {
        let syntax = syn::parse_file(source).expect("fixture parses");
        let mut visitor = AuditVisitor::new("fixture.rs".to_string());
        visitor.visit_file(&syntax);
        visitor
    }

    #[test]
    fn qualified_and_imported_sleep_calls_are_sites() {
        let visitor = inspect(
            r#"
            use std::thread::sleep as nap;
            fn contract() {
                nap(std::time::Duration::from_millis(1));
                tokio::time::sleep(std::time::Duration::from_millis(1));
            }
            "#,
        );
        assert_eq!(visitor.counts.sleeps, 2);
        assert!(
            visitor
                .sites
                .iter()
                .all(|site| site.key.scope == "contract")
        );

        let root = std::env::temp_dir().join(format!(
            "vigil-xtask-nondeterminism-scan-{}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root).expect("remove prior xtask scan fixture");
        }
        let xtask_src = root.join("xtask/src");
        fs::create_dir_all(&xtask_src).expect("create xtask fixture source");
        fs::write(
            xtask_src.join("main.rs"),
            r#"
            #[test]
            fn guard_contract() {
                std::thread::sleep(std::time::Duration::from_millis(1));
                let _ = std::time::SystemTime::now();
                let _ = std::net::TcpListener::bind("127.0.0.1:0");
            }
            "#,
        )
        .expect("write xtask nondeterminism fixture");
        let sites = collect_observed_sites(&root).expect("scan xtask fixture");
        assert_eq!(
            sites
                .iter()
                .filter(|site| site.key.path == "xtask/src/main.rs")
                .map(|site| site.key.kind)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([SiteKind::Sleep, SiteKind::RawClock, SiteKind::BindPortZero])
        );
        fs::remove_dir_all(&root).expect("remove xtask scan fixture");
    }

    #[test]
    fn sleep_hidden_in_macro_tokens_is_a_site() {
        let visitor = inspect(
            r#"
            fn contract() {
                opaque!(std::thread::sleep(std::time::Duration::from_millis(1)));
            }
            "#,
        );
        assert_eq!(visitor.counts.sleeps, 1);
        assert_eq!(visitor.sites[0].key.kind, SiteKind::MacroSleep);
    }

    #[test]
    fn wall_and_monotonic_clocks_and_elapsed_calls_are_sites() {
        let visitor = inspect(
            r#"
            use std::time::SystemTime as Clock;
            fn contract() {
                let _ = Clock::now();
                let start = std::time::Instant::now();
                let _ = chrono::Utc::now();
                let _ = chrono::Local::now();
                let _ = start.elapsed();
                let _ = Clock::now().duration_since(std::time::UNIX_EPOCH);
            }
            "#,
        );
        assert_eq!(visitor.counts.raw_clocks, 5);
        assert_eq!(visitor.counts.elapsed_calls, 2);
    }

    #[test]
    fn cfg_predicates_containing_test_are_audited() {
        let syntax = syn::parse_file(
            r#"
            use std::net::TcpListener as Available;
            #[cfg(all(test, feature = "fabric"))]
            mod tests {
                #[test]
                fn contract() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    let _ = super::Available::bind("127.0.0.1:0");
                }
            }
            "#,
        )
        .expect("fixture parses");
        let mut visitor = AuditVisitor::new("fixture.rs".to_string());
        for item in &syntax.items {
            if let Item::Use(item_use) = item {
                collect_use_aliases(&item_use.tree, Vec::new(), &mut visitor.aliases);
            }
        }
        visit_source_test_items(&syntax.items, &mut visitor);
        assert_eq!(visitor.counts.sleeps, 1);
        assert_eq!(visitor.counts.free_port_helpers, 1);
        assert_eq!(visitor.sites[0].key.scope, "tests::contract");
    }

    #[test]
    fn bind_to_port_zero_is_detected_independent_of_helper_name() {
        let visitor = inspect(
            r#"
            use std::net::{TcpListener as Listener, UdpSocket};
            fn available_address() {
                let _ = Listener::bind("127.0.0.1:0");
                let _ = UdpSocket::bind(("127.0.0.1", 0));
            }
            "#,
        );
        assert_eq!(visitor.counts.free_port_helpers, 2);
        assert!(
            visitor
                .sites
                .iter()
                .all(|site| site.key.kind == SiteKind::BindPortZero)
        );
    }

    #[test]
    fn relocating_same_syntax_changes_the_exact_site_identity() {
        let first =
            inspect("fn first() { std::thread::sleep(std::time::Duration::from_millis(1)); }");
        let second =
            inspect("fn second() { std::thread::sleep(std::time::Duration::from_millis(1)); }");
        assert_eq!(first.counts, second.counts);
        assert_ne!(first.sites[0].key, second.sites[0].key);
        assert_eq!(
            first.sites[0].key.fingerprint,
            second.sites[0].key.fingerprint
        );
    }

    #[test]
    fn ledger_rejects_unknown_fields() {
        let error = toml::from_str::<Ledger>(
            r#"
            [[file]]
            path = "fixture.rs"
            sleeps = 1
            typo = 1
            reason = "reviewed"
            "#,
        )
        .expect_err("unknown field must fail");
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn placeholder_reasons_are_not_approvals() {
        assert!(!valid_reason(""));
        assert!(!valid_reason("TODO: explain this debt"));
        assert!(valid_reason(
            "External broker handoff cannot inherit the reserved listener."
        ));
    }

    #[test]
    fn pending_receipt_review_is_not_a_completed_attestation() {
        assert!(!valid_completed_review_attestation(
            "Test-estate transition recorded; independent review pending"
        ));
        assert!(!valid_completed_review_attestation(
            "Proposed independent review"
        ));
        assert!(!valid_completed_review_attestation(
            "Existing physical suite; identity registration is review-pending"
        ));
        assert!(valid_completed_review_attestation(
            "Luna independent transition review, 2026-07-20"
        ));
    }

    #[test]
    fn ha_smoke_required_steps_cannot_skip_or_warn_green() {
        let fail_closed = r#"
printf 'HA-S%s NOT-RUN'
status=1
HA_S4_MANUAL_PROOF=x
printf -v quoted '%q ' "$@"
command -v curl >/dev/null 2>&1
pass_step 7 "the exact anchored correction object read back from cg through /why JSON"
# ─── HA-S7:
curl_ha --max-time 5 "${review_url%/}/why/$smoke_detection_id"
jq --arg label "$correction_label" --arg detection_id "$smoke_detection_id" \
  'any(.corrections[]?; .label == $label and .correction_type == "FalseAlarm" and .anchored_detection_id == $detection_id)'
# ─── Summary
"#;
        let false_green = format!("skip_step() {{ printf 'HA-S%s SKIP'; }}\n{fail_closed}");
        assert!(
            validate_ha_smoke_fail_closed(&false_green, "smoke.sh")
                .expect_err("SKIP must be rejected")
                .contains("must not report")
        );
        validate_ha_smoke_fail_closed(fail_closed, "smoke.sh")
            .expect("NOT-RUN plus suite failure is fail-closed");
        let socket_poll = format!("{fail_closed}ss -tnp | grep ESTAB\n");
        assert!(
            validate_ha_smoke_fail_closed(&socket_poll, "smoke.sh")
                .expect_err("ESTABLISHED polling must be rejected")
                .contains("must leave physical correction egress to TH-23")
        );

        let flattened_remote_args = fail_closed.replace(
            "printf -v quoted '%q ' \"$@\"",
            "printf -v remote_script 'curl -fsS %s' \"$*\"",
        );
        assert!(
            validate_ha_smoke_fail_closed(&flattened_remote_args, "smoke.sh")
                .expect_err("flattened SSH curl arguments must be rejected")
                .contains("missing fail-closed")
        );

        let cli_regression = fail_closed.replace(
            "curl_ha --max-time 5 \"${review_url%/}/why/$smoke_detection_id\"",
            "vigil_exec vigil why \"$smoke_detection_id\"",
        );
        assert!(
            validate_ha_smoke_fail_closed(&cli_regression, "smoke.sh")
                .expect_err("HA-S7 CLI correction polling must be rejected")
                .contains("shipped /why route")
        );

        assert!(haos_asset_requires_frozen_digest(Path::new(
            "tests/ha-os-vm/strace.Dockerfile"
        )));
        assert!(!haos_asset_requires_frozen_digest(Path::new(
            "tests/ha-os-vm/notes.txt"
        )));

        let correct =
            r#"vigil_correction_topic="${VIGIL_CORRECTION_TOPIC:-vigil/commands/correct}""#;
        validate_physical_correction_topic(correct, "fixture.sh")
            .expect("production correction topic must pass");
        let stale =
            r#"vigil_correction_topic="${VIGIL_CORRECTION_TOPIC:-vigil/correction/command}""#;
        let error = validate_physical_correction_topic(stale, "fixture.sh")
            .expect_err("the stale harness topic must fail");
        assert!(error.contains("production topic vigil/commands/correct"));

        let clean_boundary = correction_writer_boundary_violations(
            "fn record_correction() {}",
            r#"fn runtime() {
                std::thread::Builder::new()
                    .name("vigil-correct".to_string())
                    .spawn(|| {})
                    .unwrap();
            }"#,
        )
        .expect("clean boundary fixtures parse");
        assert!(clean_boundary.is_empty(), "{clean_boundary:?}");

        let network_regression = correction_writer_boundary_violations(
            r#"use std::net::TcpStream;
            fn record_correction() { let _ = TcpStream::connect("127.0.0.1:9"); }"#,
            r#"fn runtime() {
                std::thread::Builder::new()
                    .name("vigil-correct".to_string())
                    .spawn(|| {})
                    .unwrap();
            }"#,
        )
        .expect("network regression fixture parses");
        assert!(
            network_regression
                .iter()
                .any(|violation| violation.contains("forbidden network")),
            "a planted correction network client must fail: {network_regression:?}"
        );

        let writer_network_regression = correction_writer_boundary_violations(
            "fn record_correction() {}",
            r#"fn runtime() {
                std::thread::Builder::new()
                    .name("vigil-correct".to_string())
                    .spawn(|| { let _ = std::net::TcpStream::connect("127.0.0.1:9"); })
                    .unwrap();
            }"#,
        )
        .expect("writer network regression fixture parses");
        assert!(
            writer_network_regression
                .iter()
                .any(|violation| violation.contains("forbidden network")),
            "a planted network call inside the dedicated writer must fail: {writer_network_regression:?}"
        );

        let duplicate_writer = correction_writer_boundary_violations(
            "fn record_correction() {}",
            r#"fn runtime() {
                let _ = std::thread::Builder::new().name("vigil-correct".to_string());
                let _ = std::thread::Builder::new().name("vigil-correct".to_string());
            }"#,
        )
        .expect("duplicate writer fixture parses");
        assert!(
            duplicate_writer
                .iter()
                .any(|violation| violation.contains("exactly one")),
            "a planted duplicate trace boundary must fail: {duplicate_writer:?}"
        );
    }

    fn documentation_fixture() -> (
        DocumentationBinding,
        DocumentationContracts,
        BTreeSet<String>,
    ) {
        let test = "vigil::contract::behavior_is_proved".to_string();
        let paragraph_sha256 = paragraph_digest("The behavior is durable.");
        let binding = DocumentationBinding {
            id: "vigil.durability.behavior".to_string(),
            document: "docs/contract.md".to_string(),
            paragraph: "The behavior is durable.".to_string(),
            paragraph_sha256: paragraph_sha256.clone(),
            tests: BTreeSet::from([test.clone()]),
            first_enforcement_tag: "<!-- enforced by: `vigil::contract::behavior_is_proved` -->"
                .to_string(),
            line: 7,
        };
        let contracts = DocumentationContracts {
            version: DOCUMENTATION_CONTRACT_VERSION,
            claim: vec![DocumentationClaim {
                id: binding.id.clone(),
                document: binding.document.clone(),
                paragraph_sha256,
                evidence_tier: EvidenceTier::Integration,
                tests: vec![test.clone()],
                reviewed: true,
                reviewed_by: "independent contract review".to_string(),
                rationale: "The test restarts the store and reads the same record.".to_string(),
            }],
        };
        (binding, contracts, BTreeSet::from([test]))
    }

    #[test]
    fn documentation_contract_accepts_exact_reviewed_mapping() {
        let (binding, contracts, tests) = documentation_fixture();
        let mut violations = Vec::new();
        audit_contract_mappings(&[binding], &contracts, &tests, &mut violations);
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn documentation_contract_rejects_changed_or_missing_paragraph() {
        let (mut binding, contracts, tests) = documentation_fixture();
        binding.paragraph_sha256 = paragraph_digest("The behavior is usually durable.");
        let mut changed = Vec::new();
        audit_contract_mappings(&[binding], &contracts, &tests, &mut changed);
        assert!(
            changed
                .iter()
                .any(|item| item.contains("changed the paragraph"))
        );

        let mut missing = Vec::new();
        audit_contract_mappings(&[], &contracts, &tests, &mut missing);
        assert!(missing.iter().any(|item| item.contains("is missing")));
    }

    #[test]
    fn documentation_contract_rejects_unregistered_and_duplicate_claims() {
        let (mut binding, mut contracts, tests) = documentation_fixture();
        binding.id = "vigil.unregistered.claim".to_string();
        let mut unregistered = Vec::new();
        audit_contract_mappings(&[binding], &contracts, &tests, &mut unregistered);
        assert!(
            unregistered
                .iter()
                .any(|item| item.contains("unregistered documentation claim"))
        );

        contracts.claim.push(contracts.claim[0].clone());
        let mut duplicate = Vec::new();
        audit_contract_mappings(&[], &contracts, &tests, &mut duplicate);
        assert!(duplicate.iter().any(|item| item.contains("duplicated")));
    }

    #[test]
    fn documentation_contract_rejects_renamed_test_and_disallowed_mapping() {
        let (mut binding, contracts, tests) = documentation_fixture();
        let renamed = "vigil::contract::renamed_test".to_string();
        let mut renamed_contracts = contracts;
        renamed_contracts.claim[0].tests = vec![renamed.clone()];
        let mut renamed_violations = Vec::new();
        audit_contract_mappings(
            &[binding.clone()],
            &renamed_contracts,
            &tests,
            &mut renamed_violations,
        );
        assert!(
            renamed_violations
                .iter()
                .any(|item| item.contains("renamed, missing, or non-canonical"))
        );

        binding.tests.insert(renamed);
        let (_, exact_contracts, _) = documentation_fixture();
        let mut disallowed = Vec::new();
        audit_contract_mappings(&[binding], &exact_contracts, &tests, &mut disallowed);
        assert!(
            disallowed
                .iter()
                .any(|item| item.contains("disallowed test mapping"))
        );
    }

    #[test]
    fn documentation_contract_requires_honest_review_status_and_known_schema_values() {
        let (binding, mut contracts, tests) = documentation_fixture();
        contracts.claim[0].reviewed = false;
        let mut violations = Vec::new();
        audit_contract_mappings(&[binding], &contracts, &tests, &mut violations);
        assert!(
            violations
                .iter()
                .any(|item| item.contains("no completed independent semantic review"))
        );

        let (binding, mut placeholder_contracts, tests) = documentation_fixture();
        placeholder_contracts.claim[0].reviewed_by = "placeholder reviewer".to_string();
        let mut placeholder = Vec::new();
        audit_contract_mappings(&[binding], &placeholder_contracts, &tests, &mut placeholder);
        assert!(
            placeholder
                .iter()
                .any(|item| item.contains("claims independent review without"))
        );

        let error = toml::from_str::<DocumentationContracts>(
            r#"
            version = 1
            [[claim]]
            id = "vigil.example.claim"
            document = "README.md"
            paragraph_sha256 = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            evidence_tier = "wishful_thinking"
            tests = ["vigil::example::test"]
            reviewed = true
            reviewed_by = "reviewer"
            rationale = "behavioral proof"
            "#,
        )
        .expect_err("unknown evidence tiers must not parse");
        assert!(error.to_string().contains("unknown variant"), "{error}");
    }

    #[test]
    fn paragraph_normalization_is_whitespace_stable_but_word_sensitive() {
        let first = paragraph_digest(&normalize_paragraph("A durable\n  contract."));
        let second = paragraph_digest(&normalize_paragraph(" A durable contract. "));
        let changed = paragraph_digest(&normalize_paragraph("A best-effort contract."));
        assert_eq!(first, second);
        assert_ne!(first, changed);
    }

    #[test]
    fn every_enforced_paragraph_must_carry_one_well_formed_claim_id() {
        let directory = std::env::temp_dir().join(format!(
            "vigil-documentation-contract-parser-{}",
            std::process::id()
        ));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove prior exact parser fixture");
        }
        fs::create_dir_all(&directory).expect("create parser fixture");
        let document = directory.join("contract.md");
        fs::write(
            &document,
            "A durable behavior.\n\n<!-- enforced by: `vigil::contract::behavior_is_proved` -->\n",
        )
        .expect("write parser fixture");

        let mut missing = Vec::new();
        let groups = collect_documentation_groups(&directory, true, &mut missing)
            .expect("missing-claim fixture can be inspected");
        assert!(groups.is_empty());
        assert!(
            missing
                .iter()
                .any(|item| item.contains("must bind exactly one"))
        );

        fs::write(
            &document,
            "A durable behavior.\n\n<!-- vigil-claim: NOT-BACKTICKED -->\n<!-- enforced by: `vigil::contract::behavior_is_proved` -->\n",
        )
        .expect("write malformed parser fixture");
        let mut malformed = Vec::new();
        collect_documentation_groups(&directory, true, &mut malformed)
            .expect("malformed-claim fixture can be inspected");
        assert!(
            malformed
                .iter()
                .any(|item| item.contains("malformed claim tag"))
        );
        fs::remove_dir_all(&directory).expect("remove exact parser fixture");
    }

    #[test]
    fn ci_topology_accepts_required_shapes_and_discipline() {
        let workflow = r#"
on:
  workflow_dispatch:
    inputs:
      cache_epoch:
jobs:
  private-dependency-boundary:
    name: Private dependency boundary
    steps:
      - if: github.event.pull_request.head.repo.full_name != github.repository
        run: exit 1
  source-revisions:
    steps:
      - run: echo 'cache_epoch:$cache_epoch'
  fast:
    token: ${{ secrets.CG_CI_TOKEN }}
    persist-credentials: false
    steps:
      - uses: Swatinem/rust-cache@v2
        with:
          key: ${{ inputs.cache_epoch || 'rolling' }}
      - run: cargo nextest list --workspace -T json > target/nextest-default.json
      - run: cargo xtask test-estate-check --nextest-json default=target/nextest-default.json
      - run: cargo nextest run --profile pr --workspace
  feature-lanes:
    strategy:
      matrix:
        include:
          - shape: decode
            features: decode-gstreamer
          - shape: detect
            features: detect-burn-wgpu
          - shape: combined
            features: decode-gstreamer,detect-burn-wgpu
          - shape: fabric
            features: fabric
          - shape: production
            features: decode-gstreamer,detect-burn-wgpu,fabric
    steps:
      - uses: Swatinem/rust-cache@v2
        with:
          key: ${{ inputs.cache_epoch || 'rolling' }}
      - run: |
          du -sh vigil/target
          target_kb="$(du -sk vigil/target | awk '{print $1}')"
          available_kb="$(df --output=avail vigil/target | tail -n 1)"
          effective_capacity_kb=$((available_kb + target_kb))
          scratch_floor_kb=16777216
          capacity_floor_kb=31457280
          if [ "${available_kb}" -lt "${scratch_floor_kb}" ] || [ "${effective_capacity_kb}" -lt "${capacity_floor_kb}" ]; then
            exit 1
          fi
      - run: cargo nextest run --profile pr -p vigil --features ${{ matrix.features }}
      - run: |
          cargo nextest list -p vigil --features ${{ matrix.features }} -T json > target/nextest-${{ matrix.shape }}.json
          cargo xtask test-estate-check --nextest-json ${{ matrix.shape }}=target/nextest-${{ matrix.shape }}.json
  test-estate-gate:
    name: PR test-estate gate
    if: always()
    needs: [private-dependency-boundary, fast, feature-lanes]
    steps:
      - run: |
          test "${{ needs.fast.result }}" = "success"
          test "${{ needs.feature-lanes.result }}" = "success"
          test "${{ needs.private-dependency-boundary.result }}" = "success"
  full-tests:
    steps:
      - run: sudo timeout --kill-after=30s 5m apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 update
      - uses: Swatinem/rust-cache@v2
        with:
          key: ${{ inputs.cache_epoch || 'rolling' }}
      - run: |
          du -sh vigil/target
          target_kb="$(du -sk vigil/target | awk '{print $1}')"
          available_kb="$(df --output=avail vigil/target | tail -n 1)"
          effective_capacity_kb=$((available_kb + target_kb))
          scratch_floor_kb=16777216
          capacity_floor_kb=31457280
          if [ "${available_kb}" -lt "${scratch_floor_kb}" ] || [ "${effective_capacity_kb}" -lt "${capacity_floor_kb}" ]; then
            exit 1
          fi
      - run: cargo build -p vigil --release --features fabric
      - run: |
          cargo nextest list --workspace --features first-light-acceptance,acceptance -T json > target/nextest-slow.json
          cargo xtask test-estate-check --nextest-json slow=target/nextest-slow.json
  static-musl:
    steps:
      - uses: Swatinem/rust-cache@v2
        with:
          key: ${{ inputs.cache_epoch || 'rolling' }}
      - run: |
          du -sh vigil/target
          target_kb="$(du -sk vigil/target | awk '{print $1}')"
          available_kb="$(df --output=avail vigil/target | tail -n 1)"
          effective_capacity_kb=$((available_kb + target_kb))
          scratch_floor_kb=16777216
          capacity_floor_kb=31457280
          if [ "${available_kb}" -lt "${scratch_floor_kb}" ] || [ "${effective_capacity_kb}" -lt "${capacity_floor_kb}" ]; then
            exit 1
          fi
      - run: cargo build --release --target x86_64-unknown-linux-musl --features fabric
      - run: cargo build --release --target aarch64-unknown-linux-musl --features fabric
  docker:
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: vigil/dist/downloaded
      - run: |
          install x86 dist/docker/amd64/vigil
          install arm dist/docker/arm64/vigil
  hardware-images:
    strategy:
      matrix:
        include:
          - binary_target: vigil-hw-binary-amd64
            generic_target: vigil-generic-docker-hw-amd64
            addon_target: vigil-addon-hw-amd64
          - binary_target: vigil-hw-binary-arm64
            generic_target: vigil-generic-docker-hw-arm64
            addon_target: vigil-addon-hw-aarch64
    steps:
      - run: |
          output_dir="vigil/dist/production/${{ matrix.arch }}"
          tar --exclude='*/target' --exclude='*/target/**' --exclude='*/.git/**' --exclude='*/tests/fixtures/.cache/**' -cf - vigil context-graph contextdb |
            docker buildx build \
              --target "${target}" \
              --file vigil/Dockerfile.hardware \
              --cache-from type=gha,scope=vigil-hw-${{ matrix.arch }}-${{ inputs.cache_epoch || 'rolling' }} \
              --output "type=local,dest=${output_dir}/binary" \
              -
          touch "${output_dir}/vigil-generic-docker-hw-${{ matrix.arch }}.oci.tar"
          touch "${output_dir}/vigil-addon-hw-${{ matrix.arch }}.oci.tar"
      - uses: actions/upload-artifact@v4
        with:
          name: vigil-production-${{ matrix.arch }}
          if-no-files-found: error
"#;
        let mut violations = Vec::new();
        audit_ci_text(workflow, &mut violations);
        assert!(violations.is_empty(), "{violations:?}");

        let skipped_production_inventory = workflow.replacen(
            "      - run: |\n          cargo nextest list -p vigil --features ${{ matrix.features }} -T json",
            "      - if: matrix.shape != 'production'\n        run: |\n          cargo nextest list -p vigil --features ${{ matrix.features }} -T json",
            1,
        );
        assert_ne!(skipped_production_inventory, workflow);
        let mut skipped_production_violations = Vec::new();
        audit_ci_text(
            &skipped_production_inventory,
            &mut skipped_production_violations,
        );
        assert!(skipped_production_violations.iter().any(|violation| {
            violation.contains("exact production-union identities")
                && violation.contains("cannot skip")
        }));

        let unbounded_package_install = workflow.replacen(
            "sudo timeout --kill-after=30s 5m apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 update",
            "sudo timeout --kill-after=30s 5m apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 update && sudo apt-get install -y hidden-bypass",
            1,
        );
        assert_ne!(unbounded_package_install, workflow);
        let mut unbounded_package_violations = Vec::new();
        audit_ci_text(
            &unbounded_package_install,
            &mut unbounded_package_violations,
        );
        assert!(unbounded_package_violations.iter().any(|violation| {
            violation.contains("bound mirror retries") && violation.contains("cannot wedge")
        }));

        let cache_blind_disk_guard = workflow.replacen(
            "          effective_capacity_kb=$((available_kb + target_kb))\n",
            "",
            1,
        );
        assert_ne!(cache_blind_disk_guard, workflow);
        let mut cache_blind_violations = Vec::new();
        audit_ci_text(&cache_blind_disk_guard, &mut cache_blind_violations);
        assert!(cache_blind_violations.iter().any(|violation| {
            violation.contains("cache-aware post-restore disk check")
                && violation.contains("16 GiB real scratch")
        }));

        let unkeyed_cache = workflow.replacen(
            "          key: ${{ inputs.cache_epoch || 'rolling' }}\n",
            "",
            1,
        );
        assert_ne!(unkeyed_cache, workflow);
        let mut unkeyed_cache_violations = Vec::new();
        audit_ci_text(&unkeyed_cache, &mut unkeyed_cache_violations);
        assert!(unkeyed_cache_violations.iter().any(|violation| {
            violation.contains("cache epoch") && violation.contains("cold/warm measurements")
        }));

        let unsafe_workspace_context = workflow.replacen(
            "              --file vigil/Dockerfile.hardware \\\n              -",
            "              --file vigil/Dockerfile.hardware \\\n              .",
            1,
        );
        assert_ne!(unsafe_workspace_context, workflow);
        let mut unsafe_context_violations = Vec::new();
        audit_ci_text(&unsafe_workspace_context, &mut unsafe_context_violations);
        assert!(unsafe_context_violations.iter().any(|violation| {
            violation.contains("hardware Docker build must consume")
                && violation.contains("workspace `.` context is forbidden")
        }));

        let dockerignore = "*\n!Dockerfile\n!dist/\ndist/**\n!dist/docker/\n!dist/docker/amd64/\n!dist/docker/amd64/vigil\n!dist/docker/arm64/\n!dist/docker/arm64/vigil\n";
        let mut dockerignore_violations = Vec::new();
        audit_dockerignore_text(dockerignore, &mut dockerignore_violations);
        assert!(dockerignore_violations.is_empty());
        audit_dockerignore_text(
            &format!("{dockerignore}!target/**\n"),
            &mut dockerignore_violations,
        );
        assert!(
            dockerignore_violations
                .iter()
                .any(|violation| violation.contains("exact Dockerfile-plus-two-dist-binaries"))
        );

        let codeowners = r#"
/xtask/src/test_estate.rs @lucidprogrammer
/.config/test-*.toml @lucidprogrammer
/.config/nextest*.toml @lucidprogrammer
/.config/non-rust-test-*.toml @lucidprogrammer
/.config/documentation-contracts.toml @lucidprogrammer
/.github/CODEOWNERS @lucidprogrammer
/.github/workflows/ci.yml @lucidprogrammer
/.github/workflows/dev-closeout.yml @lucidprogrammer
/.github/workflows/integrate-dev.yml @lucidprogrammer
/.github/workflows/release-qualification.yml @lucidprogrammer
/.config/dev-closeout-impact.toml @lucidprogrammer
/xtask/src/verify.rs @lucidprogrammer
/xtask/src/closeout_impact.rs @lucidprogrammer
/scripts/verify @lucidprogrammer
/AGENTS.md @lucidprogrammer
/tests/ha-os-vm/ @lucidprogrammer
"#;
        audit_codeowners_text(codeowners, &mut violations);
        assert!(violations.is_empty(), "{violations:?}");

        let mut missing_owner = Vec::new();
        audit_codeowners_text(
            &codeowners.replace("/.github/CODEOWNERS @lucidprogrammer\n", ""),
            &mut missing_owner,
        );
        assert!(
            missing_owner
                .iter()
                .any(|item| item.contains("/.github/CODEOWNERS"))
        );
    }

    #[test]
    fn compartmented_workflows_reject_cross_tier_work_and_manual_downgrades() {
        let change = include_str!("../../.github/workflows/ci.yml");
        let closeout = include_str!("../../.github/workflows/dev-closeout.yml");
        let release = include_str!("../../.github/workflows/release-qualification.yml");
        let mut accepted = Vec::new();
        audit_ci_compartments(change, closeout, release, &mut accepted);
        assert!(accepted.is_empty(), "{accepted:?}");

        let expensive_change = format!(
            "{change}\njobs:\n  planted-heavy-change:\n    steps:\n      - run: cargo nextest archive\n"
        );
        let mut expensive = Vec::new();
        audit_ci_compartments(&expensive_change, closeout, release, &mut expensive);
        assert!(
            expensive
                .iter()
                .any(|item| item.contains("cheap default-shape"))
        );

        let downgraded_closeout =
            closeout.replacen("cargo xtask closeout-impact", "echo bypass", 1);
        let mut downgraded = Vec::new();
        audit_ci_compartments(change, &downgraded_closeout, release, &mut downgraded);
        assert!(
            downgraded
                .iter()
                .any(|item| item.contains("closeout-impact"))
        );
        let self_qualifying_closeout =
            closeout.replacen("Cargo.lock|Cargo.toml", "Cargo.none|Cargo.none", 1);
        let mut self_qualifying_violations = Vec::new();
        audit_ci_compartments(
            change,
            &self_qualifying_closeout,
            release,
            &mut self_qualifying_violations,
        );
        assert!(
            self_qualifying_violations
                .iter()
                .any(|item| item.contains("Cargo.lock|Cargo.toml"))
        );
        let archive_decoy = closeout.replacen(
            "-- cargo nextest archive",
            "echo '-- cargo nextest archive'",
            1,
        );
        let mut archive_decoy_violations = Vec::new();
        audit_ci_compartments(
            change,
            &archive_decoy,
            release,
            &mut archive_decoy_violations,
        );
        assert!(
            archive_decoy_violations
                .iter()
                .any(|item| item.contains("archive exactly once"))
        );
        let printed_launcher = closeout.replacen(
            "          ./scripts/verify dev-closeout \\",
            "          printf '%s\\n' \\\n          ./scripts/verify dev-closeout \\",
            1,
        );
        assert_ne!(printed_launcher, closeout);
        let mut printed_launcher_violations = Vec::new();
        audit_ci_compartments(
            change,
            &printed_launcher,
            release,
            &mut printed_launcher_violations,
        );
        assert!(
            printed_launcher_violations
                .iter()
                .any(|item| item.contains("first and only")),
            "{printed_launcher_violations:?}"
        );

        let publishing_release = format!(
            "{release}\njobs:\n  planted-publish:\n    steps:\n      - run: docker push forbidden\n"
        );
        let mut publishing = Vec::new();
        audit_ci_compartments(change, closeout, &publishing_release, &mut publishing);
        assert!(
            publishing
                .iter()
                .any(|item| item.contains("non-publishing"))
        );

        let launcher = include_str!("../../scripts/verify");
        let mut accepted_launcher = Vec::new();
        audit_verify_launcher_text(launcher, &mut accepted_launcher);
        assert!(accepted_launcher.is_empty(), "{accepted_launcher:?}");
        let unstable_clock = launcher.replace("date +%s%N", "date +%s%3N");
        let mut unstable = Vec::new();
        audit_verify_launcher_text(&unstable_clock, &mut unstable);
        assert!(unstable.iter().any(|item| item.contains("invalid timing")));

        let integration = include_str!("../../.github/workflows/integrate-dev.yml");
        let mut accepted_integration = Vec::new();
        audit_dev_integration_workflow(integration, &mut accepted_integration);
        assert!(accepted_integration.is_empty(), "{accepted_integration:?}");
        let forcing = integration.replacen("--field force=false", "--field force=true", 1);
        let mut forcing_violations = Vec::new();
        audit_dev_integration_workflow(&forcing, &mut forcing_violations);
        assert!(
            forcing_violations
                .iter()
                .any(|item| item.contains("non-forcing"))
        );
        for weakened in [
            integration.replacen(
                ".enforce_admins.enabled == true",
                ".enforce_admins.enabled == false",
                1,
            ),
            integration.replacen("(.app_id // 0) > 0", "(.app_id // 0) >= 0", 1),
            integration.replacen(
                ".workflow_id == $workflow_id",
                ".workflow_id != $workflow_id",
                1,
            ),
            integration.replacen(".github/workflows/*", ".github/workflows/none", 1),
            integration.replacen("Cargo.lock|Cargo.toml", "Cargo.none|Cargo.none", 1),
            integration.replacen("--name-only --no-renames -z", "--name-only", 1),
        ] {
            let mut weakened_violations = Vec::new();
            audit_dev_integration_workflow(&weakened, &mut weakened_violations);
            assert!(
                !weakened_violations.is_empty(),
                "integration guard accepted weakened protection: {weakened}"
            );
        }

        let status_leak = closeout.replacen(
            "permissions:\n  contents: read",
            "permissions:\n  contents: read\n  statuses: write",
            1,
        );
        let mut status_leak_violations = Vec::new();
        audit_ci_compartments(change, &status_leak, release, &mut status_leak_violations);
        assert!(
            status_leak_violations
                .iter()
                .any(|item| item.contains("candidate-executing jobs remain read-only"))
        );
    }

    #[test]
    fn ci_topology_rejects_removed_shape_cli_retry_and_missing_guard() {
        let workflow = r#"
decoy:
  features:
    - fabric
jobs:
  feature-lanes:
    strategy:
      matrix:
        features:
          - decode-gstreamer
          - detect-burn-wgpu
          - decode-gstreamer,detect-burn-wgpu
    steps:
      - run: cargo nextest run --profile pr --retries 2
"#;
        let mut violations = Vec::new();
        audit_ci_text(workflow, &mut violations);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("fast lane"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("--retries"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("`fabric`"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("hardware image target"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("docker buildx build"))
        );
    }

    #[test]
    fn release_note_hardware_names_do_not_substitute_for_named_target_builds() {
        let workflow = r#"
jobs:
  fast:
    steps:
      - run: cargo xtask test-estate-check
  notes-only:
    steps:
      - run: echo vigil-generic-docker-hw-amd64 vigil-generic-docker-hw-arm64
  feature-lanes:
    strategy:
      matrix:
        features:
          - decode-gstreamer
          - detect-burn-wgpu
          - decode-gstreamer,detect-burn-wgpu
          - fabric
"#;
        let mut violations = Vec::new();
        audit_ci_text(workflow, &mut violations);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("docker buildx build"))
        );
    }

    #[test]
    fn ci_rejects_cargo_targets_on_tmpfs_even_with_disk_capacity() {
        let workflow = r#"
jobs:
  feature-lanes:
    env:
      CARGO_TARGET_DIR: $RUNNER_TEMP/vigil-target
    steps:
      - run: |
          du -sh vigil/target
          available_kb="$(df --output=avail vigil/target | tail -n 1)"
          test "${available_kb}" -ge 31457280
      - run: cargo nextest run --profile pr -p vigil --features fabric
"#;
        let mut violations = Vec::new();
        audit_ci_text(workflow, &mut violations);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("must not route Cargo/build targets"))
        );
    }

    #[test]
    fn path_modules_contribute_their_real_harness_identity() {
        let directory = std::env::temp_dir().join(format!(
            "vigil-test-contract-path-module-{}",
            std::process::id()
        ));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove prior path fixture");
        }
        fs::create_dir_all(directory.join("support")).expect("create path fixture");
        fs::write(
            directory.join("root.rs"),
            "#[path = \"support/helper.rs\"] mod harness;",
        )
        .expect("write path root");
        fs::write(
            directory.join("support/helper.rs"),
            "#[cfg(test)] mod tests { #[test] fn held_port_is_exclusive() {} }",
        )
        .expect("write path child");
        let mut records = BTreeSet::new();
        collect_tests_from_module_file(
            &directory,
            &directory.join("root.rs"),
            vec!["vigil".to_string(), "broker".to_string()],
            &mut records,
            &mut BTreeSet::new(),
        )
        .expect("collect path module");
        assert!(records.iter().any(|record| {
            record.identity == "vigil::broker::harness::tests::held_port_is_exclusive"
                && record.source == "support/helper.rs"
        }));
        fs::remove_dir_all(&directory).expect("remove path fixture");
    }

    #[test]
    fn deleting_a_registry_identity_without_a_receipt_fails() {
        let baseline_set = BTreeSet::from(["vigil::family::keeper".to_string()]);
        let baseline = TestBaseline {
            version: 1,
            base_sha: FROZEN_TEST_BASE_SHA.to_string(),
            vector_sha256: identity_vector_digest(&baseline_set),
            tests: baseline_set.iter().cloned().collect(),
        };
        let active = BTreeSet::new();
        let receipts = TestReceipts {
            version: 1,
            transition: Vec::new(),
        };
        let mut violations = Vec::new();
        audit_test_transitions(
            Path::new("."),
            &baseline,
            &baseline_set,
            &active,
            &receipts,
            &mut violations,
        );
        assert!(
            violations
                .iter()
                .any(|item| item.contains("without a complete reviewed receipt chain"))
        );

        let non_rust_baseline_set = BTreeSet::from([
            "TH-CHANGE|active".to_string(),
            "TH-REMOVE|implementation-blocked-physical".to_string(),
        ]);
        let non_rust_observed =
            BTreeSet::from(["TH-CHANGE|implementation-blocked-physical".to_string()]);
        let non_rust_baseline = NonRustBaseline {
            version: NON_RUST_BASELINE_VERSION,
            vector_sha256: identity_vector_digest(&non_rust_baseline_set),
            cases: non_rust_baseline_set.iter().cloned().collect(),
        };
        let non_rust_receipts = NonRustReceipts {
            version: NON_RUST_BASELINE_VERSION,
            transition: vec![NonRustTransitionReceipt {
                id: "unproved-removal-and-status-change".to_string(),
                before_vector_sha256: identity_vector_digest(&non_rust_baseline_set),
                after_vector_sha256: identity_vector_digest(&non_rust_observed),
                removed_cases: vec!["TH-REMOVE".to_string()],
                added_cases: Vec::new(),
                status_changes: vec![NonRustStatusChange {
                    id: "TH-CHANGE".to_string(),
                    before: NonRustCaseStatus::Active,
                    after: NonRustCaseStatus::ImplementationBlockedPhysical,
                }],
                zero_lost_detection:
                    "The changed physical boundary retains an exact executable acceptance identity."
                        .to_string(),
                rationale:
                    "The fixture attempts a removal and status change without an owner ruling."
                        .to_string(),
                reviewer: "Independent completed fixture review".to_string(),
                proof: Vec::new(),
            }],
        };
        let mut non_rust_violations = Vec::new();
        audit_non_rust_transitions(
            &non_rust_baseline,
            &non_rust_receipts,
            &non_rust_observed,
            &mut non_rust_violations,
        );
        assert!(non_rust_violations.iter().any(|item| {
            item.contains("removals or status changes without machine-checked owner proof")
                && item.contains("TH-CHANGE")
                && item.contains("TH-REMOVE")
        }));
    }

    #[test]
    fn receipt_must_enumerate_the_exact_surviving_vector() {
        let baseline_set = BTreeSet::from([
            "vigil::family::deleted".to_string(),
            "vigil::family::keeper".to_string(),
        ]);
        let active = BTreeSet::from(["vigil::family::keeper".to_string()]);
        let baseline = TestBaseline {
            version: 1,
            base_sha: FROZEN_TEST_BASE_SHA.to_string(),
            vector_sha256: identity_vector_digest(&baseline_set),
            tests: baseline_set.iter().cloned().collect(),
        };
        let receipts = TestReceipts {
            version: 1,
            transition: vec![TestTransitionReceipt {
                id: "reviewed-deletion".to_string(),
                base_sha: FROZEN_TEST_BASE_SHA.to_string(),
                before_vector_sha256: identity_vector_digest(&baseline_set),
                after_vector_sha256: identity_vector_digest(&active),
                removed_tests: vec!["vigil::family::deleted".to_string()],
                added_tests: Vec::new(),
                surviving_tests: Vec::new(),
                zero_lost_detection:
                    "Mutation replay killed the deleted predicate through the keeper.".to_string(),
                rationale: "The keeper exercises the same behavior and boundary.".to_string(),
                reviewer: "Independent test-estate reviewer".to_string(),
                proof: vec![TestTransitionProof::OwnerRuling {
                    removed_tests: vec!["vigil::family::deleted".to_string()],
                    ruling: "Owner rejected the deleted contract and required the keeper contract."
                        .to_string(),
                    reviewer: "Independent owner-ruling review".to_string(),
                }],
            }],
        };
        let mut violations = Vec::new();
        audit_test_transitions(
            Path::new("."),
            &baseline,
            &baseline_set,
            &active,
            &receipts,
            &mut violations,
        );
        assert!(
            violations
                .iter()
                .any(|item| item.contains("exact surviving vector"))
        );
    }

    #[test]
    fn physical_contract_requires_active_nonignored_witness_and_exact_command() {
        let contract = TestContract {
            id: "real-camera-physical-journey".to_string(),
            lineage: "Owner real-camera acceptance window".to_string(),
            sources: vec!["tests/acceptance.rs".to_string()],
            evidence_kind: TestEvidenceKind::Physical,
            gate: TestGate::Physical,
            tests: vec!["vigil::acceptance::real_camera".to_string()],
            deterministic_witness: Some("vigil::acceptance::synthetic".to_string()),
            explicit_run: Some("cargo test real_camera".to_string()),
        };
        let records = vec![TestRecord {
            identity: "vigil::acceptance::synthetic".to_string(),
            source: "tests/acceptance.rs".to_string(),
            ignored: true,
        }];
        let mut violations = Vec::new();
        validate_physical_contract(&contract, &records, &mut violations);
        assert!(
            violations
                .iter()
                .any(|item| item.contains("witness must run per change"))
        );
        assert!(
            violations
                .iter()
                .any(|item| item.contains("--ignored --exact"))
        );

        let non_rust = toml::from_str::<NonRustContracts>(
            r#"version = 2
            asset = []

            [[case]]
            id = "TH-99"
            source = "tests/ha-os-vm/run-th-suite.sh"
            handler = "th99"
            gate = "physical"
            status = "active"
            lineage = "Physical case spans two independently named invariants."
            reviewed_by = "Independent closeout review accepted both mappings on 2026-07-21."
            deterministic_witnesses = [
              "vigil::acceptance::first_invariant",
              "vigil::acceptance::second_invariant",
            ]
            explicit_run = "TH_RUN_LIST=TH-99 tests/ha-os-vm/run-th-suite.sh"
            "#,
        )
        .expect("non-Rust physical schema accepts multiple deterministic witnesses");
        assert!(valid_completed_review_attestation(
            &non_rust.case[0].reviewed_by
        ));
        assert_eq!(non_rust.case[0].deterministic_witnesses.len(), 2);

        let witness_records = vec![
            TestRecord {
                identity: "vigil::acceptance::first_invariant".to_string(),
                source: "tests/acceptance.rs".to_string(),
                ignored: false,
            },
            TestRecord {
                identity: "vigil::acceptance::second_invariant".to_string(),
                source: "tests/acceptance.rs".to_string(),
                ignored: true,
            },
        ];
        let mut witness_violations = Vec::new();
        validate_non_rust_witnesses(&non_rust.case[0], &witness_records, &mut witness_violations);
        assert!(witness_violations.iter().any(|item| {
            item.contains("uses ignored deterministic witness") && item.contains("second_invariant")
        }));
        assert!(!valid_completed_review_attestation(
            "Independent physical-to-deterministic mapping review pending"
        ));
    }

    #[test]
    fn mandatory_prerequisite_branch_cannot_return_green() {
        let syntax = syn::parse_file(
            r#"
            #[test]
            fn broker_contract() {
                if std::env::var("BROKER").is_err() {
                    eprintln!("not installed");
                    return;
                }
            }
            #[test]
            fn docker_contract() {
                match std::env::var("DOCKER") {
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
            #[test]
            fn tool_contract() {
                let Some(_tool) = which("ffmpeg") else { return; };
            }
            "#,
        )
        .expect("fixture parses");
        let mut functions = Vec::new();
        collect_test_functions(&syntax.items, &mut functions);
        let mut violations = Vec::new();
        for function in functions {
            let mut visitor = SilentPrerequisiteVisitor {
                source: "fixture.rs",
                test: function.sig.ident.to_string(),
                violations: &mut violations,
            };
            visitor.visit_block(&function.block);
        }
        assert_eq!(
            violations
                .iter()
                .filter(|item| item.contains("can return green"))
                .count(),
            3
        );

        let root = std::env::temp_dir().join(format!(
            "vigil-xtask-prerequisite-scan-{}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root).expect("remove prior xtask prerequisite fixture");
        }
        fs::create_dir_all(root.join("crates/vigil/tests"))
            .expect("create empty integration fixture root");
        fs::create_dir_all(root.join("xtask/src")).expect("create xtask prerequisite fixture root");
        fs::write(
            root.join("xtask/src/main.rs"),
            r#"
            #[test]
            fn guard_contract() {
                if std::env::var("BROKER").is_err() {
                    return;
                }
            }
            "#,
        )
        .expect("write xtask prerequisite fixture");
        let mut xtask_violations = Vec::new();
        audit_silent_prerequisite_passes(&root, &mut xtask_violations)
            .expect("scan xtask prerequisite fixture");
        assert!(xtask_violations.iter().any(|violation| {
            violation.contains("xtask/src/main.rs") && violation.contains("can return green")
        }));
        fs::remove_dir_all(&root).expect("remove xtask prerequisite fixture");
    }

    #[test]
    fn source_scan_definition_ignores_doc_links_but_catches_executable_reads() {
        assert!(!is_production_source_text_scan(
            r#"//! See contextdb-server/src/work_ledger.rs
            fn read_socket() { let _ = stream.read_to_string(&mut response); }"#
        ));
        assert!(is_production_source_text_scan(
            r#"#[test] fn scan() {
                let source = std::fs::read_to_string("crates/vigil/src/runtime.rs").unwrap();
                assert!(source.contains("contract"));
            }"#
        ));

        let wired = fabric_worker_lease_wiring_violations(
            r#"
            fn spawn_worker_loop(&self) {
                let config = contextdb_server::work_ledger::WorkerConfig {
                    lease_duration_ms: resolved_worker_lease_duration_ms(self.worker_lease_ms),
                };
            }
            "#,
        )
        .expect("wired fixture parses");
        assert!(
            wired.is_empty(),
            "exact production wiring must pass: {wired:?}"
        );

        let hardcoded = fabric_worker_lease_wiring_violations(
            r#"
            fn spawn_worker_loop(&self) {
                let config = contextdb_server::work_ledger::WorkerConfig {
                    lease_duration_ms: 5 * 60_000,
                };
            }
            "#,
        )
        .expect("planted hardcode fixture parses");
        assert!(
            hardcoded.iter().any(|violation| {
                violation.contains("resolved_worker_lease_duration_ms(self.worker_lease_ms)")
            }),
            "a planted hardcode must be rejected by the central syntax-aware guard: {hardcoded:?}"
        );
    }

    #[test]
    fn all_test_estate_registries_reject_unknown_fields() {
        let contracts = toml::from_str::<TestContracts>(
            r#"version = 1
            typo = true
            contract = []"#,
        )
        .expect_err("test contract schema must reject typo");
        assert!(contracts.to_string().contains("unknown field"));

        let receipts = toml::from_str::<TestReceipts>(
            r#"version = 1
            typo = true"#,
        )
        .expect_err("receipt schema must reject typo");
        assert!(receipts.to_string().contains("unknown field"));

        let non_rust_baseline = toml::from_str::<NonRustBaseline>(
            r#"version = 1
            vector_sha256 = "sha256:fixture"
            cases = []
            typo = true"#,
        )
        .expect_err("non-Rust baseline schema must reject typo");
        assert!(non_rust_baseline.to_string().contains("unknown field"));

        let non_rust_receipts = toml::from_str::<NonRustReceipts>(
            r#"version = 1
            transition = []
            typo = true"#,
        )
        .expect_err("non-Rust receipt schema must reject typo");
        assert!(non_rust_receipts.to_string().contains("unknown field"));

        let allowlist = toml::from_str::<SourceScanAllowlist>(
            r#"version = 1
            typo = true"#,
        )
        .expect_err("allowlist schema must reject typo");
        assert!(allowlist.to_string().contains("unknown field"));
    }

    #[test]
    fn contract_paragraph_coverage_requires_binding_or_strict_unenforced_reason() {
        let directory = std::env::temp_dir().join(format!(
            "vigil-documentation-coverage-{}",
            std::process::id()
        ));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove prior coverage fixture");
        }
        fs::create_dir_all(&directory).expect("create coverage fixture");
        let document = directory.join("README.md");
        fs::write(&document, "The default HTTP port is 8080.\n").expect("write uncovered fixture");
        let mut uncovered = Vec::new();
        audit_contract_paragraph_coverage(&directory, &mut uncovered)
            .expect("inspect uncovered fixture");
        assert!(
            uncovered
                .iter()
                .any(|item| item.contains("contract-bearing"))
        );

        fs::write(&document, "The interval defaults to 30 seconds.\n")
            .expect("write defaults-only fixture");
        let mut defaults = Vec::new();
        audit_contract_paragraph_coverage(&directory, &mut defaults)
            .expect("inspect defaults-only fixture");
        assert!(
            defaults
                .iter()
                .any(|item| item.contains("contract-bearing"))
        );

        fs::write(&document, "Do not enable this release feature.\n")
            .expect("write plain-not fixture");
        let mut plain_not = Vec::new();
        audit_contract_paragraph_coverage(&directory, &mut plain_not)
            .expect("inspect plain-not fixture");
        assert!(
            plain_not
                .iter()
                .any(|item| item.contains("contract-bearing"))
        );

        fs::write(
            &document,
            "The default HTTP port is 8080.\n\n<!-- vigil-unenforced: classification=documentation-gap; reason=`No executable default-port witness is registered yet.` -->\n",
        )
        .expect("write classified fixture");
        let mut classified = Vec::new();
        audit_contract_paragraph_coverage(&directory, &mut classified)
            .expect("inspect classified fixture");
        assert!(classified.is_empty(), "{classified:?}");

        fs::write(
            &document,
            "The default HTTP port is 8080.\n<!-- vigil-unenforced: classification=documentation-gap; reason=`No executable default-port witness is registered yet.` -->\n",
        )
        .expect("write inline classified fixture");
        let mut inline_classified = Vec::new();
        audit_contract_paragraph_coverage(&directory, &mut inline_classified)
            .expect("inspect inline classified fixture");
        assert!(inline_classified.is_empty(), "{inline_classified:?}");

        fs::write(
            &document,
            "The current schema has no wildcard.\n\n<!-- vigil-unenforced: classification=maybe; reason=`TODO` -->\n",
        )
        .expect("write malformed fixture");
        let mut malformed = Vec::new();
        audit_contract_paragraph_coverage(&directory, &mut malformed)
            .expect("inspect malformed fixture");
        assert!(malformed.iter().any(|item| item.contains("malformed")));
        fs::remove_dir_all(&directory).expect("remove coverage fixture");
    }

    #[test]
    fn authoritative_nextest_json_preserves_exact_binary_identity() {
        let directory = std::env::temp_dir().join(format!(
            "vigil-nextest-identity-cross-check-{}",
            std::process::id()
        ));
        if directory.exists() {
            fs::remove_dir_all(&directory).expect("remove prior nextest fixture");
        }
        fs::create_dir_all(&directory).expect("create nextest fixture");
        let inventory = directory.join("nextest.json");
        fs::write(
            &inventory,
            r#"{
              "rust-suites": {
                "suite": {
                  "package-name": "vigil",
                  "binary-name": "contract",
                  "kind": "test",
                  "testcases": {"behavior_is_proved": {}}
                }
              }
            }"#,
        )
        .expect("write nextest fixture");
        let identities = parse_nextest_json(&inventory).expect("parse nextest fixture");
        assert_eq!(
            identities,
            BTreeSet::from(["vigil::contract::behavior_is_proved".to_string()])
        );
        fs::remove_dir_all(&directory).expect("remove nextest fixture");
    }
}
