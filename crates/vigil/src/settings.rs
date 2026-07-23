//! The typed settings registry: the single declaration point for every
//! adjustable value Vigil exposes to an owner.
//!
//! An adjustable value is either a declared setting with a visible outside
//! control, or an explicitly declared internal diagnostic — there is no
//! third, hidden category. Declaring a setting through
//! [`SettingsRegistry::declare`] is the only way to obtain a
//! [`SettingHandle`] for it, because the handle's constructor is private to
//! this module; a caller in another module cannot fabricate one and skip the
//! registry.
//!
//! Every declared setting carries exactly three control states
//! ([`ControlState`]): the ordinary product-selected value Vigil may revise
//! ([`ControlState::Automatic`]), a value Vigil has already revised in
//! response to measured conditions ([`ControlState::AutoAdjusted`]), and a
//! value the owner has pinned ([`ControlState::Manual`]). Pinning
//! ([`SettingHandle::set_manual`]) and releasing a pin
//! ([`SettingHandle::return_to_automatic`]) are owner actions, explicit and
//! reversible, and the type only hands that power to whoever holds the
//! owner-facing [`SettingHandle`] itself — not to whoever merely holds an
//! [`AutomationHandle`] minted from it. [`AutomationHandle::apply_automatic`]
//! runs the same validator [`SettingHandle::set_manual`] runs and returns a
//! different type: an automation write against a manually pinned setting is
//! a recorded no-op ([`AutomaticWriteOutcome::Withheld`]) that the caller
//! cannot mistake for a write that took effect
//! ([`AutomaticWriteOutcome::Applied`]), because those are two different
//! values of the same `#[must_use]` enum. Critically, an
//! [`AutomationHandle`] cannot defeat that refusal by unpinning first: it
//! has no `set_manual` and no `return_to_automatic` — those methods simply
//! do not exist on it — so the only way to change who is in control is to
//! be the owner holding the [`SettingHandle`], never the automation calling
//! through it.
//!
//! The registry holds one entry today — the detector's stationary scan
//! interval. Its resolution in `config::load` now goes through
//! [`SettingsRegistry::declare`] and reads back [`SettingHandle::effective_value`];
//! an operator-supplied value is applied as a manual pin
//! ([`SettingHandle::set_manual`]). That proves the declaration and the
//! control-state API are real for a production setting, not just for a test
//! fixture. It does **not** demonstrate live enforcement: `load` builds the
//! registry, declares the one entry, and drops it before returning, so no
//! automation ever mints an [`AutomationHandle`] from it and no automatic
//! write is ever attempted against it while Vigil runs. The manual-pin-
//! versus-automation behavior is enforced by the split between
//! [`SettingHandle`] and [`AutomationHandle`] and proven by this module's
//! unit tests, not by a running system. The durable handle a scheduled
//! tuner would hold, and the rest of Vigil's adjustable values, arrive here
//! as the configuration-surface work migrates them onto this same
//! declaration point, one at a time; this module does not perform that
//! migration. [`check_coverage`] is a pure function over declared entries and observed
//! surfaces precisely so it keeps working, unchanged, as more entries land.

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard};

/// The three understandable control states an adjustable setting can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    /// Vigil is using the ordinary product-selected value and may revise it
    /// as conditions change.
    Automatic,
    /// Vigil has changed the value in response to measured conditions; the
    /// reason is available from the handle.
    AutoAdjusted,
    /// The owner has pinned the value; automatic tuning cannot replace it.
    Manual,
}

/// Where an owner sees and changes a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingSurfaces {
    /// The key an owner sets in a standalone config file.
    pub config_key: &'static str,
    /// The key an owner sets in the Home Assistant add-on options.
    pub addon_option_key: &'static str,
    /// The documentation page describing the setting.
    pub documentation_page: &'static str,
}

/// Validates a proposed value. The owner path and the automation path run
/// the identical validator, so neither can bypass it.
pub type Validator<T> = fn(&T) -> Result<(), String>;

/// The declaration of one setting: a stable name, a default, validation for
/// a proposed value, and where an owner sees and changes it.
pub struct SettingSpec<T> {
    pub name: &'static str,
    pub default: T,
    pub validate: Validator<T>,
    pub surfaces: SettingSurfaces,
}

/// One automatic write that was refused because the setting was manually
/// pinned, kept so the owner can inspect why it did not take effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithheldAutomaticWrite<T> {
    pub attempted_value: T,
    pub reason: String,
    pub pinned_value: T,
}

/// The result of an automation write attempt. The two cases carry different
/// data on purpose: a caller has to look at which one it got back before it
/// can know the effective value, so a withheld write can never be mistaken
/// for one that landed.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomaticWriteOutcome<T> {
    /// The automatic write applied; the setting's control state is now
    /// [`ControlState::AutoAdjusted`].
    Applied {
        previous_value: T,
        effective_value: T,
        reason: String,
    },
    /// The setting is [`ControlState::Manual`]; the proposed value was
    /// recorded, not applied.
    Withheld {
        attempted_value: T,
        reason: String,
        pinned_value: T,
    },
}

struct HandleState<T> {
    control: ControlState,
    value: T,
    last_automatic_reason: Option<String>,
    withheld: Vec<WithheldAutomaticWrite<T>>,
}

/// The behavior and state a declared setting's two handles ([`SettingHandle`],
/// the owner-facing type, and [`AutomationHandle`], the automation-facing
/// type) share: the stable name, the surfaces metadata, the validator, and
/// the live mutable state, plus every method that only READS or proposes
/// against that state (including [`SettingCore::apply_automatic`], which
/// both the owner path and the automation path run through — the owner
/// handle just never exposes it directly). Deliberately private, and
/// deliberately does NOT define `set_manual` or `return_to_automatic`
/// anywhere in its own `impl` block below: those two owner-only methods
/// live SOLELY on [`SettingHandle`]'s own `impl` block, reaching straight
/// into a core's private fields (`self.core.validate`, `self.core.lock()`)
/// rather than being core methods `SettingHandle` merely forwards.
///
/// This is what makes pin/unpin TYPE-STRUCTURALLY unreachable from
/// automation TODAY, not merely absent-by-convention: [`AutomationHandle`]
/// holds `&SettingCore<T>` and nothing else (see its own field below), and
/// `SettingCore` genuinely has neither method, so no accessor or `Deref`
/// added to `AutomationHandle` AS IT IS SHAPED NOW could expose them
/// through it — checkable by reading both type definitions, not by
/// running anything. This is a claim about the CURRENT shape of these two
/// types, not a guarantee that survives every possible future edit: a
/// later change to `AutomationHandle`'s own field — say, back to
/// `&SettingHandle<T>` — would reopen the path immediately, and neither
/// this structural argument nor the lexical scan in
/// `automation_handle_capability_scan.rs` (which watches `impl` blocks for
/// forbidden method names, not struct field declarations) would notice
/// that specific edit. Read [`AutomationHandle`]'s own honesty note before
/// trusting any of this from a doctest; it does not come from one.
struct SettingCore<T> {
    name: &'static str,
    surfaces: SettingSurfaces,
    validate: Validator<T>,
    state: Mutex<HandleState<T>>,
}

impl<T: Clone> SettingCore<T> {
    fn new(spec: SettingSpec<T>) -> Self {
        Self {
            name: spec.name,
            surfaces: spec.surfaces,
            validate: spec.validate,
            state: Mutex::new(HandleState {
                control: ControlState::Automatic,
                value: spec.default,
                last_automatic_reason: None,
                withheld: Vec::new(),
            }),
        }
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn surfaces(&self) -> SettingSurfaces {
        self.surfaces
    }

    fn control_state(&self) -> ControlState {
        self.lock().control
    }

    fn effective_value(&self) -> T {
        self.lock().value.clone()
    }

    fn last_automatic_reason(&self) -> Option<String> {
        self.lock().last_automatic_reason.clone()
    }

    fn withheld_automatic_writes(&self) -> Vec<WithheldAutomaticWrite<T>> {
        self.lock().withheld.clone()
    }

    /// Propose a new value in response to measured conditions. Runs the
    /// same validator the owner's [`SettingHandle::set_manual`] runs. When
    /// the setting is manually pinned, the write is refused and recorded as
    /// [`AutomaticWriteOutcome::Withheld`] instead of silently applying.
    /// Reachable only through [`AutomationHandle::apply_automatic`] (the
    /// owner-facing handle never calls this itself), so the owner-facing
    /// handle never offers a path that could silently overwrite a pin.
    fn apply_automatic(
        &self,
        value: T,
        reason: impl Into<String>,
    ) -> Result<AutomaticWriteOutcome<T>, String> {
        (self.validate)(&value)?;
        let reason = reason.into();
        let mut state = self.lock();
        if state.control == ControlState::Manual {
            let pinned_value = state.value.clone();
            state.withheld.push(WithheldAutomaticWrite {
                attempted_value: value.clone(),
                reason: reason.clone(),
                pinned_value: pinned_value.clone(),
            });
            return Ok(AutomaticWriteOutcome::Withheld {
                attempted_value: value,
                reason,
                pinned_value,
            });
        }
        let previous_value = state.value.clone();
        state.value = value.clone();
        state.control = ControlState::AutoAdjusted;
        state.last_automatic_reason = Some(reason.clone());
        Ok(AutomaticWriteOutcome::Applied {
            previous_value,
            effective_value: value,
            reason,
        })
    }

    fn lock(&self) -> MutexGuard<'_, HandleState<T>> {
        self.state
            .lock()
            .expect("setting state lock is never held across a panic")
    }
}

/// A live, owned adjustable setting.
///
/// A caller outside this module cannot build one directly — every field is
/// private and the constructor is private to this module, so this does not
/// compile:
///
/// ```compile_fail
/// use vigil::settings::SettingHandle;
///
/// let _handle: SettingHandle<u64> = SettingHandle {
///     name: "smuggled",
/// };
/// ```
///
/// The only public path to a handle is [`SettingsRegistry::declare`], which
/// is how declaring a setting becomes the only way to obtain one.
pub struct SettingHandle<T> {
    core: SettingCore<T>,
}

impl<T: Clone> SettingHandle<T> {
    fn new(spec: SettingSpec<T>) -> Self {
        Self {
            core: SettingCore::new(spec),
        }
    }

    /// The setting's stable name.
    pub fn name(&self) -> &'static str {
        self.core.name()
    }

    /// Where an owner sees and changes this setting.
    pub fn surfaces(&self) -> SettingSurfaces {
        self.core.surfaces()
    }

    /// The setting's current control state.
    pub fn control_state(&self) -> ControlState {
        self.core.control_state()
    }

    /// The value Vigil is actually using now.
    pub fn effective_value(&self) -> T {
        self.core.effective_value()
    }

    /// Why the effective value last changed automatically, if it did.
    pub fn last_automatic_reason(&self) -> Option<String> {
        self.core.last_automatic_reason()
    }

    /// Automatic writes that were refused because the setting was manually
    /// pinned, most recent last.
    pub fn withheld_automatic_writes(&self) -> Vec<WithheldAutomaticWrite<T>> {
        self.core.withheld_automatic_writes()
    }

    /// Owner path: pin the setting to an explicit value. Runs the same
    /// validator the automation path runs. A manual pin always wins over
    /// automation until it is explicitly released with
    /// [`SettingHandle::return_to_automatic`]. Defined HERE, on
    /// `SettingHandle` alone, against `self.core`'s own private fields —
    /// not on [`SettingCore`] itself and not forwarded from it, which is
    /// exactly why [`AutomationHandle`] holding only `&SettingCore<T>`
    /// cannot reach it (see [`SettingCore`]'s own doc comment).
    pub fn set_manual(&self, value: T) -> Result<(), String> {
        (self.core.validate)(&value)?;
        let mut state = self.core.lock();
        state.value = value;
        state.control = ControlState::Manual;
        Ok(())
    }

    /// Owner action: release a manual pin back to automatic control. This
    /// is explicit and reversible — the owner can pin the setting again at
    /// any time. Only the owner holding this handle can call it; the
    /// automation-facing [`AutomationHandle`] has no such method, and
    /// [`SettingCore`] — the only type it borrows — never defines one for
    /// it to inherit through any future accessor.
    pub fn return_to_automatic(&self) {
        self.core.lock().control = ControlState::Automatic;
    }

    /// Mint the automation-facing capability over this setting. It can
    /// propose a value change and read the effective state, but it cannot
    /// pin, release, or repin — [`SettingHandle::set_manual`] and
    /// [`SettingHandle::return_to_automatic`] do not exist on
    /// [`AutomationHandle`], so a caller holding only that capability can
    /// never defeat a manual pin by unpinning it first.
    pub fn automation(&self) -> AutomationHandle<'_, T> {
        AutomationHandle { core: &self.core }
    }
}

/// The automation-facing view over a declared setting, minted from
/// [`SettingHandle::automation`]. It can propose an automatic write and
/// read the effective state, but it has no `set_manual` and no
/// `return_to_automatic` — those are not merely refused at runtime, they do
/// not exist as methods on this type, so a caller holding only an
/// `AutomationHandle` cannot unpin a setting to force its own write through.
///
/// ## Two layers of guarantee, and which one is load-bearing
///
/// **The type is now the PRIMARY guarantee — a claim about its CURRENT
/// shape, not a promise about every future edit.** This type's only field
/// is `core: &'a SettingCore<T>` — see the field below — and
/// [`SettingCore`] itself has no `set_manual`/`return_to_automatic`
/// anywhere in its own `impl` block (both live solely on
/// [`SettingHandle`]). So it is not just that `AutomationHandle` doesn't
/// define those methods TODAY: as long as its only field stays
/// `&SettingCore<T>`, no `Deref` or accessor added to it could expose them
/// either, because the type it borrows genuinely does not have them —
/// checkable by reading both definitions, not by running anything. That
/// is a fact about these two type definitions AS THEY STAND; it is not
/// self-maintaining. A later edit changing this field itself — to
/// `&SettingHandle<T>`, for instance — would reopen the path immediately,
/// and neither this structural argument nor the lexical scan below (which
/// watches `impl` blocks for forbidden method names, not struct field
/// declarations) would notice that specific change.
///
/// **`automation_handle_capability_scan.rs` is belt-and-braces
/// defense-in-depth**, re-checked on every test run: it lexically scans
/// every `impl` block naming `AutomationHandle` (or a direct alias of it,
/// in any of ten enumerated syntactic forms, in either identifier
/// spelling — see that file's own module doc) for `set_manual`/
/// `return_to_automatic`, and would catch either method being added
/// straight back — the one thing the type-level guarantee does NOT cover,
/// since a NEW `impl AutomationHandle { fn set_manual ... }` is exactly as
/// legal Rust today as it always was. The scan is not what makes the
/// property true; it is what notices if a future edit makes it stop being
/// true.
///
/// **What the two `compile_fail` blocks below prove, and what they do
/// NOT — read carefully, this is the honest limit of a doctest here:**
/// both fail because [`SettingsRegistry::new`] and
/// [`SettingsRegistry::declare`] are `pub(crate)` — a doctest always
/// compiles as an external crate consuming only the PUBLIC API, so it
/// cannot even construct a handle, and so cannot get anywhere NEAR calling
/// `set_manual`/`return_to_automatic` on one, let alone testing whether
/// they exist. That is a real, separate guarantee (nothing outside this
/// crate can mint an `AutomationHandle` at all), but it says nothing about
/// method presence: these blocks would compile-fail identically, and so
/// would still read as passing, even if one of those methods were added
/// straight back to `AutomationHandle`. Nor can any doctest attempt the
/// STRUCTURAL bypass this section opened with — adding a hypothetical
/// `impl Deref<Target = SettingCore<T>> for AutomationHandle` and calling
/// `set_manual` through it — because [`SettingCore`] is private: an
/// external doctest cannot even NAME it to write that `impl`. This is
/// exactly the asymmetry the type-vs-scan split above exists to state
/// plainly: the deeper, structural guarantee is not something a
/// `compile_fail` doctest can reach from outside the crate at all; it
/// rests on the type definitions themselves, inspected in source, backed
/// by the lexical scan re-checking the one thing that inspection can't
/// re-check on every commit by itself.
///
/// ```compile_fail
/// use vigil::settings::{SettingSpec, SettingSurfaces, SettingsRegistry};
///
/// let mut registry = SettingsRegistry::new(); // `new` is pub(crate); fails here
/// let handle = registry.declare(SettingSpec {
///     name: "example",
///     default: 0u64,
///     validate: |_value| Ok(()),
///     surfaces: SettingSurfaces {
///         config_key: "example",
///         addon_option_key: "example",
///         documentation_page: "docs/example.md",
///     },
/// });
/// let automation = handle.automation();
/// automation.set_manual(5);
/// ```
///
/// ```compile_fail
/// use vigil::settings::{SettingSpec, SettingSurfaces, SettingsRegistry};
///
/// let mut registry = SettingsRegistry::new(); // `new` is pub(crate); fails here
/// let handle = registry.declare(SettingSpec {
///     name: "example",
///     default: 0u64,
///     validate: |_value| Ok(()),
///     surfaces: SettingSurfaces {
///         config_key: "example",
///         addon_option_key: "example",
///         documentation_page: "docs/example.md",
///     },
/// });
/// let automation = handle.automation();
/// automation.return_to_automatic();
/// ```
pub struct AutomationHandle<'a, T> {
    core: &'a SettingCore<T>,
}

impl<'a, T: Clone> AutomationHandle<'a, T> {
    /// The setting's stable name.
    pub fn name(&self) -> &'static str {
        self.core.name()
    }

    /// Where an owner sees and changes this setting.
    pub fn surfaces(&self) -> SettingSurfaces {
        self.core.surfaces()
    }

    /// The setting's current control state.
    pub fn control_state(&self) -> ControlState {
        self.core.control_state()
    }

    /// The value Vigil is actually using now.
    pub fn effective_value(&self) -> T {
        self.core.effective_value()
    }

    /// Why the effective value last changed automatically, if it did.
    pub fn last_automatic_reason(&self) -> Option<String> {
        self.core.last_automatic_reason()
    }

    /// Automatic writes that were refused because the setting was manually
    /// pinned, most recent last.
    pub fn withheld_automatic_writes(&self) -> Vec<WithheldAutomaticWrite<T>> {
        self.core.withheld_automatic_writes()
    }

    /// Propose a new value in response to measured conditions. When the
    /// setting is manually pinned, the write is refused and recorded as
    /// [`AutomaticWriteOutcome::Withheld`] instead of silently applying —
    /// and this type has no way to release that pin first.
    pub fn apply_automatic(
        &self,
        value: T,
        reason: impl Into<String>,
    ) -> Result<AutomaticWriteOutcome<T>, String> {
        self.core.apply_automatic(value, reason)
    }
}

/// Metadata about one declared setting: enough to check that it truly
/// appears on every surface an owner is promised, without exposing the
/// setting's runtime value or type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingCoverageEntry {
    pub name: &'static str,
    pub surfaces: SettingSurfaces,
}

/// The typed settings registry: the single place an adjustable value is
/// declared. Declaring a setting is the only way a caller can obtain a
/// [`SettingHandle`] for it.
///
/// Both the constructor and [`SettingsRegistry::declare`] are `pub(crate)`:
/// a caller outside this crate cannot build a registry at all, so it
/// cannot declare a setting the coverage check (`vigil::declared_settings`)
/// never sees. This does not compile:
///
/// ```compile_fail
/// use vigil::settings::SettingsRegistry;
///
/// let _registry = SettingsRegistry::new();
/// ```
pub struct SettingsRegistry {
    entries: Vec<SettingCoverageEntry>,
}

impl SettingsRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Declare a setting and receive its handle. This is the only path to
    /// a [`SettingHandle`] — and, being `pub(crate)`, only code inside this
    /// crate can reach it at all. `config::declare_settings` is the one
    /// production function that calls it; see its own doc comment for the
    /// in-crate half of this guarantee.
    pub(crate) fn declare<T: Clone>(&mut self, spec: SettingSpec<T>) -> SettingHandle<T> {
        self.entries.push(SettingCoverageEntry {
            name: spec.name,
            surfaces: spec.surfaces,
        });
        SettingHandle::new(spec)
    }

    /// Every setting declared through this registry, for the coverage
    /// check.
    pub fn coverage_entries(&self) -> &[SettingCoverageEntry] {
        &self.entries
    }
}

/// Which of a setting's declared surfaces it failed to appear on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    ConfigFile,
    AddonOption,
    Documentation,
}

/// One setting missing from one of its declared surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageGap {
    pub setting_name: &'static str,
    pub missing_surface: SurfaceKind,
}

/// The surfaces a coverage check has actually observed, gathered however the
/// caller likes (real files in production, fixtures in a test). Presence in
/// a set means an owner can really see and change the setting there today.
#[derive(Debug, Clone, Default)]
pub struct ObservedSurfaces {
    pub config_keys: HashSet<String>,
    pub addon_option_keys: HashSet<String>,
    pub documented_setting_names: HashSet<String>,
}

/// Pure coverage check: does every declared entry really appear on the
/// config-file surface, the add-on options, and the documentation? Pure so
/// it can be exercised with fixture entries and fixture surfaces, with no
/// file I/O of its own.
pub fn check_coverage(
    entries: &[SettingCoverageEntry],
    observed: &ObservedSurfaces,
) -> Vec<CoverageGap> {
    let mut gaps = Vec::new();
    for entry in entries {
        if !observed.config_keys.contains(entry.surfaces.config_key) {
            gaps.push(CoverageGap {
                setting_name: entry.name,
                missing_surface: SurfaceKind::ConfigFile,
            });
        }
        if !observed
            .addon_option_keys
            .contains(entry.surfaces.addon_option_key)
        {
            gaps.push(CoverageGap {
                setting_name: entry.name,
                missing_surface: SurfaceKind::AddonOption,
            });
        }
        if !observed.documented_setting_names.contains(entry.name) {
            gaps.push(CoverageGap {
                setting_name: entry.name,
                missing_surface: SurfaceKind::Documentation,
            });
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_spec() -> SettingSpec<u64> {
        SettingSpec {
            name: "example_interval_secs",
            default: 30,
            validate: |_value| Ok(()),
            surfaces: SettingSurfaces {
                config_key: "example_interval_secs",
                addon_option_key: "example_interval_secs",
                documentation_page: "docs/example.md",
            },
        }
    }

    #[test]
    fn automatic_write_against_a_manual_pin_is_withheld_not_applied() {
        let mut registry = SettingsRegistry::new();
        let handle = registry.declare(example_spec());

        handle.set_manual(45).expect("owner pin validates");
        assert_eq!(handle.control_state(), ControlState::Manual);

        // Go through the automation-facing capability, not the owner
        // handle: this is the type an automatic tuner would actually hold,
        // and it has no way to unpin the setting before writing.
        let automation = handle.automation();
        let outcome = automation
            .apply_automatic(10, "queue pressure eased")
            .expect("automatic proposal validates");
        match outcome {
            AutomaticWriteOutcome::Withheld {
                attempted_value,
                pinned_value,
                ..
            } => {
                assert_eq!(attempted_value, 10);
                assert_eq!(pinned_value, 45);
            }
            AutomaticWriteOutcome::Applied { .. } => {
                panic!("an automatic write must not silently overwrite a manual pin");
            }
        }

        // The manual pin is unaffected, and the withheld attempt is
        // recorded so the owner can inspect it later.
        assert_eq!(handle.effective_value(), 45);
        assert_eq!(handle.control_state(), ControlState::Manual);
        let withheld = handle.withheld_automatic_writes();
        assert_eq!(withheld.len(), 1);
        assert_eq!(withheld[0].attempted_value, 10);
        assert_eq!(withheld[0].reason, "queue pressure eased");
        assert_eq!(withheld[0].pinned_value, 45);
    }

    #[test]
    fn returning_a_pin_to_automatic_lets_the_next_automatic_write_apply() {
        let mut registry = SettingsRegistry::new();
        let handle = registry.declare(example_spec());

        handle.set_manual(45).expect("owner pin validates");
        handle.return_to_automatic();
        assert_eq!(handle.control_state(), ControlState::Automatic);

        let outcome = handle
            .automation()
            .apply_automatic(12, "measured queue depth")
            .expect("automatic proposal validates");
        assert!(matches!(outcome, AutomaticWriteOutcome::Applied { .. }));
        assert_eq!(handle.effective_value(), 12);
        assert_eq!(handle.control_state(), ControlState::AutoAdjusted);
        assert_eq!(
            handle.last_automatic_reason().as_deref(),
            Some("measured queue depth")
        );
    }

    #[test]
    fn manual_and_automatic_writes_share_the_same_validation() {
        let mut registry = SettingsRegistry::new();
        let handle = registry.declare(SettingSpec {
            name: "bounded_value",
            default: 5u64,
            validate: |value| {
                if *value <= 100 {
                    Ok(())
                } else {
                    Err("must be at most 100".to_string())
                }
            },
            surfaces: SettingSurfaces {
                config_key: "bounded_value",
                addon_option_key: "bounded_value",
                documentation_page: "docs/example.md",
            },
        });

        assert!(handle.set_manual(500).is_err());
        assert_eq!(
            handle.effective_value(),
            5,
            "a rejected manual value must not overwrite the default"
        );

        assert!(handle.automation().apply_automatic(500, "test").is_err());
        assert_eq!(
            handle.effective_value(),
            5,
            "a rejected automatic value must not overwrite the default"
        );
    }

    fn fixture_entry(name: &'static str) -> SettingCoverageEntry {
        SettingCoverageEntry {
            name,
            surfaces: SettingSurfaces {
                config_key: name,
                addon_option_key: name,
                documentation_page: "docs/example.md",
            },
        }
    }

    fn fully_observed(names: &[&str]) -> ObservedSurfaces {
        let mut observed = ObservedSurfaces::default();
        for name in names {
            observed.config_keys.insert((*name).to_string());
            observed.addon_option_keys.insert((*name).to_string());
            observed
                .documented_setting_names
                .insert((*name).to_string());
        }
        observed
    }

    #[test]
    fn coverage_check_reports_a_setting_missing_from_the_config_surface() {
        let entry = fixture_entry("missing_from_config");
        let mut observed = fully_observed(&["missing_from_config"]);
        observed.config_keys.remove("missing_from_config");

        let gaps = check_coverage(&[entry], &observed);

        assert_eq!(
            gaps,
            vec![CoverageGap {
                setting_name: "missing_from_config",
                missing_surface: SurfaceKind::ConfigFile,
            }]
        );
    }

    #[test]
    fn coverage_check_reports_a_setting_missing_from_the_addon_options() {
        let entry = fixture_entry("missing_from_addon");
        let mut observed = fully_observed(&["missing_from_addon"]);
        observed.addon_option_keys.remove("missing_from_addon");

        let gaps = check_coverage(&[entry], &observed);

        assert_eq!(
            gaps,
            vec![CoverageGap {
                setting_name: "missing_from_addon",
                missing_surface: SurfaceKind::AddonOption,
            }]
        );
    }

    #[test]
    fn coverage_check_reports_a_setting_missing_from_the_documentation() {
        let entry = fixture_entry("missing_from_docs");
        let mut observed = fully_observed(&["missing_from_docs"]);
        observed
            .documented_setting_names
            .remove("missing_from_docs");

        let gaps = check_coverage(&[entry], &observed);

        assert_eq!(
            gaps,
            vec![CoverageGap {
                setting_name: "missing_from_docs",
                missing_surface: SurfaceKind::Documentation,
            }]
        );
    }

    #[test]
    fn coverage_check_reports_nothing_for_a_fully_covered_setting() {
        let entry = fixture_entry("fully_covered");
        let observed = fully_observed(&["fully_covered"]);

        let gaps = check_coverage(&[entry], &observed);

        assert!(gaps.is_empty());
    }
}
