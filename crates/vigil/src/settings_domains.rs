//! Automatic-management domains: the clusters of values Vigil manages together,
//! the gate that runs before the author ranking, and the take-over instruction
//! that is the only other accepted way past it.
//!
//! A domain is itself an ordinary setting — on or off, authored and resolved by
//! the ordinary rules, defaulting to on. The gate is not a fourth author rank.

use crate::settings_backends::{DECODE_BACKEND_SETTING, DETECTION_BACKEND_SETTING};
use crate::settings_model::{Refusal, RefusalKind, Scope, SettingValue, Surface};
// Only the take-over path names these, and that path is compiled only under
// `test-support`.
#[cfg(feature = "test-support")]
use crate::settings_model::{ScopeTarget, SettingRecord, SettingsError};

/// The accelerated-detection domain switch.
pub const ACCELERATED_DETECTION_DOMAIN: &str = "accelerated_detection";

/// The hardware-decoding domain switch.
pub const HARDWARE_DECODING_DOMAIN: &str = "hardware_decoding";

/// One declared domain and the settings it governs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainDeclaration {
    /// The domain switch's own setting name.
    pub switch: &'static str,
    /// The settings this domain governs.
    pub members: Vec<&'static str>,
    /// The membership generation this entry was declared at. A pin written at
    /// an earlier generation predates the membership and is grandfathered.
    pub generation: u64,
}

/// Every domain that exists, with its real membership. Declared next to the
/// settings registry at compile time; the registry's coverage check extends
/// over it.
pub fn domain_roster() -> Vec<DomainDeclaration> {
    vec![
        DomainDeclaration {
            switch: ACCELERATED_DETECTION_DOMAIN,
            members: vec![DETECTION_BACKEND_SETTING],
            generation: 1,
        },
        DomainDeclaration {
            switch: HARDWARE_DECODING_DOMAIN,
            members: vec![DECODE_BACKEND_SETTING],
            generation: 1,
        },
    ]
}

/// The domain currently governing `setting`, if any.
pub fn governing_domain(setting: &str) -> Option<DomainDeclaration> {
    domain_roster()
        .into_iter()
        .find(|domain| domain.members.contains(&setting))
}

/// The membership generation a record written now carries: the generation of
/// the domain governing it today, or none at all when nothing governs it. A
/// record stamped below its domain's generation predates the membership.
pub fn membership_generation(setting: &str) -> u64 {
    governing_domain(setting)
        .map(|domain| domain.generation)
        .unwrap_or(0)
}

/// Whether a domain switch is itself a setting a domain governs. It is not:
/// the switch is the thing you turn off, so nothing may close it.
pub fn is_domain_switch(setting: &str) -> bool {
    domain_roster()
        .iter()
        .any(|domain| domain.switch == setting)
}

/// The refusal a write to a governed value gets: the domain that closed it,
/// and how to take the wheel.
pub fn governed_value_refusal(setting: &str, domain: &str) -> Refusal {
    Refusal {
        kind: RefusalKind::GovernedValue {
            domain: domain.to_string(),
        },
        cause: format!(
            "{setting} is currently managed by {domain}, so it is not open to being set."
        ),
        remedy: format!(
            "Turn {domain} off first — set {domain} to false — and then set {setting}; \
             opening {domain} shows everything else it governs before you do."
        ),
    }
}

/// What Vigil currently chooses for one governed setting, for the domain roster
/// rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct DomainMemberChoice {
    pub setting: String,
    pub current_choice: SettingValue,
    /// Why Vigil chose it — the derivation input, never a blank field.
    pub reason: String,
}

/// One domain as the operator surface renders it: what it governs, and what
/// Vigil currently chooses for each member.
#[derive(Debug, Clone, PartialEq)]
pub struct DomainView {
    pub switch: String,
    /// Whether the domain is currently on.
    pub on: bool,
    pub members: Vec<DomainMemberChoice>,
}

/// What the gate decided about one write.
#[derive(Debug, Clone, PartialEq)]
pub enum GateDecision {
    /// The setting is not inside a domain that is on; the ranking applies.
    Open,
    /// The setting is governed; the write is refused naming the domain and how
    /// to turn it off.
    Refused(Refusal),
    /// A record that predates the domain's membership stays effective; the gate
    /// closes for new writes, not for values already standing.
    Grandfathered { domain: String },
}

/// Run the gate for one proposed write.
pub fn gate_write(
    setting: &str,
    _scope: &Scope,
    domain_states: &[(String, bool)],
    record_generation: Option<u64>,
) -> GateDecision {
    let Some(domain) = governing_domain(setting) else {
        return GateDecision::Open;
    };
    let on = domain_states
        .iter()
        .find(|(switch, _)| switch == domain.switch)
        .map(|(_, on)| *on)
        .unwrap_or(true);
    if !on {
        return GateDecision::Open;
    }
    if record_generation.is_some_and(|generation| generation < domain.generation) {
        return GateDecision::Grandfathered {
            domain: domain.switch.to_string(),
        };
    }
    GateDecision::Refused(governed_value_refusal(setting, domain.switch))
}

/// A hub instruction that names the domain it disables and the value it sets.
/// Applied in that order, all-or-nothing: if the domain cannot be disabled, the
/// value is not set either.
#[derive(Debug, Clone, PartialEq)]
pub struct TakeOverInstruction {
    /// The domain to disable. A take-over that does not name one is refused.
    pub disable_domain: Option<String>,
    pub setting: String,
    pub scope: Scope,
    pub value: SettingValue,
    pub surface: Surface,
    pub reason: String,
}

/// What a take-over did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeOverOutcome {
    /// The writes that were applied, in the order they were applied.
    pub applied: Vec<String>,
}

/// Apply a take-over instruction against the store at `path`.
///
/// This is what a management server does TO a node, so it opens the hub-role
/// handle — and that handle is compiled only under `test-support`, which no
/// artifact enables. So this function exists in the harness, where a test
/// stands the hub up, and in no shipped build: a node cannot instruct itself
/// on a fleet's behalf.
#[cfg(feature = "test-support")]
pub fn apply_take_over(
    path: &std::path::Path,
    instruction: &TakeOverInstruction,
) -> Result<TakeOverOutcome, SettingsError> {
    let Some(domain) = instruction.disable_domain.clone() else {
        return Err(SettingsError::Refused(Refusal {
            kind: RefusalKind::TakeOverWithoutDomain,
            cause: format!(
                "this instruction sets {} without naming the automatic management it switches off.",
                instruction.setting
            ),
            remedy: "Name the domain to disable in the same instruction; a value set under an \
                     on domain would be re-decided on the next pass."
                .to_string(),
        }));
    };

    let store = crate::settings_store::SettingsStore::open_hub_role(path)?;
    let target = ScopeTarget {
        tenant: instruction.scope.target.clone(),
        site: instruction.scope.target.clone(),
        node: instruction.scope.target.clone(),
        camera: None,
    };

    // All-or-nothing, checked before anything is written: if a record already
    // outranks the hub on the switch, the domain never goes off, so the value
    // is not set either — and no record of this instruction is left behind.
    if !store.pushed_switch_would_take_effect(&domain, &instruction.scope, &target)? {
        return Err(SettingsError::Refused(governed_value_refusal(
            &instruction.setting,
            &domain,
        )));
    }

    // The order is the contract: the domain is disabled first, then the value
    // is set. Both go through the store's own hub-authored seam, which is the
    // single home of the pushed-rank write.
    store.apply_hub_authored(vec![
        SettingRecord::pushed(
            domain.clone(),
            instruction.scope.clone(),
            SettingValue::Bool(false),
            format!(
                "the management server switched {domain} off to set {}",
                instruction.setting
            ),
        ),
        SettingRecord::pushed(
            instruction.setting.clone(),
            instruction.scope.clone(),
            instruction.value.clone(),
            instruction.reason.clone(),
        ),
    ])?;

    // A pushed value lands exactly like a local one, so it is applied and
    // mirrored exactly like one: the node it landed on brings the live values
    // into force, and the page the Home Assistant user trusts stops disagreeing
    // with the store the moment the hub changes something. A mirror that cannot
    // happen — no Supervisor, an unreachable one — never un-applies the value:
    // the store is the authority and the mirror is a courtesy to the surface.
    let node = store.node_view();
    crate::settings_application::apply_live_change(&node, &target);
    if let Ok(client) = crate::settings_reflection::ContainerSupervisorClient::from_environment() {
        for setting in [domain.as_str(), instruction.setting.as_str()] {
            if let Err(error) = crate::settings_reflection::reflect_landed_change(
                &node, path, &target, &client, setting,
            ) {
                println!("reflection-not-achieved setting={setting} reason={error}");
            }
        }
    }

    Ok(TakeOverOutcome {
        applied: vec![domain, instruction.setting.clone()],
    })
}
