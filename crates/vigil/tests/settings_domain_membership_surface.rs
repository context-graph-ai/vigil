//! A user told "turn off accelerated detection to set this" has to be able
//! to see what else that turns off before they do it. So every
//! automatic-management domain names, on the operator surface, which
//! settings it governs and what Vigil is currently choosing for each — a
//! setting governed by a domain nobody can enumerate is a hidden knob by
//! another name.
//!
//! Why this is unfakeable: the roster is asserted as an exact set, so a
//! third domain invented to satisfy a message, or a domain quietly dropped,
//! both fail. Membership is cross-checked in both directions —
//! [`domain_roster`] lists the member and [`governing_domain`] returns that
//! same domain for it — so a roster that renders prettily while the gate
//! consults a different membership cannot pass. The ungoverned half is
//! asserted from the production settings registry rather than from a list
//! typed here, so a future domain that quietly swallowed a rate control
//! fails this test rather than surprising an operator. And every member
//! choice must carry a non-empty reason: a blank reason is the defect this
//! test exists to catch, not an empty field to tolerate.

use std::collections::BTreeSet;

use vigil::settings_backends::{DECODE_BACKEND_SETTING, DETECTION_BACKEND_SETTING};
use vigil::settings_domains::{
    ACCELERATED_DETECTION_DOMAIN, HARDWARE_DECODING_DOMAIN, domain_roster, governing_domain,
};
use vigil::settings_projection::{DOMAIN_LINE_PREFIX, report_by_direct_read};
use vigil::settings_store::SettingsStore;

#[test]
fn each_domain_names_the_settings_it_governs_and_what_vigil_currently_chooses() {
    let roster = domain_roster();
    let switches: BTreeSet<&str> = roster.iter().map(|domain| domain.switch).collect();
    assert_eq!(
        switches,
        BTreeSet::from([ACCELERATED_DETECTION_DOMAIN, HARDWARE_DECODING_DOMAIN]),
        "two automatic-management domains exist — the roster is the real membership, not the \
         ambition; got {switches:?}"
    );

    let accelerated = roster
        .iter()
        .find(|domain| domain.switch == ACCELERATED_DETECTION_DOMAIN)
        .expect("the accelerated-detection domain is declared");
    assert!(
        accelerated.members.contains(&DETECTION_BACKEND_SETTING),
        "accelerated detection governs which detection backend runs; members: {:?}",
        accelerated.members
    );
    let hardware_decoding = roster
        .iter()
        .find(|domain| domain.switch == HARDWARE_DECODING_DOMAIN)
        .expect("the hardware-decoding domain is declared");
    assert!(
        hardware_decoding.members.contains(&DECODE_BACKEND_SETTING),
        "hardware decoding governs which decode backend runs per stream; members: {:?}",
        hardware_decoding.members
    );

    // The roster and the gate must read the same membership, in both
    // directions: every member resolves back to the domain that lists it,
    // and no domain switch is itself governed.
    for domain in &roster {
        assert!(
            !domain.members.is_empty(),
            "a domain that governs nothing is not a domain; {}",
            domain.switch
        );
        for member in &domain.members {
            let governing = governing_domain(member).unwrap_or_else(|| {
                panic!(
                    "{member} is listed by {} but is governed by nothing",
                    domain.switch
                )
            });
            assert_eq!(
                governing.switch, domain.switch,
                "{member} is listed by {} but the gate consults {}",
                domain.switch, governing.switch
            );
        }
        assert!(
            governing_domain(domain.switch).is_none(),
            "a domain switch is an ordinary setting, not something a domain governs; {}",
            domain.switch
        );
    }

    // The rate controls are ungoverned, and that is a measured statement:
    // no tuner writes settings yet, so nothing has to be turned off before
    // a person sets them. Read from the production registry, so a domain
    // that later swallowed one of them fails here.
    //
    // The coupling, recorded rather than left implicit: `declared_settings`
    // is the LEGACY in-memory settings registry's inventory, not the
    // store-backed settings this surface otherwise talks about, so this
    // loop proves the ungoverned half only for the settings that registry
    // still declares — it is not an inventory of everything a person can
    // set. What it is checked against IS the compile-time domain
    // declaration (design point 6): each entry is asserted against the
    // roster's own member lists as well as through the gate, so the two
    // cannot drift apart underneath this assertion.
    for entry in vigil::declared_settings() {
        assert!(
            governing_domain(entry.name).is_none(),
            "{} is a setting a person sets, with no domain above it; a domain claiming it is a \
             product change, not a refactor",
            entry.name
        );
        for domain in &roster {
            assert!(
                !domain.members.contains(&entry.name),
                "{} is listed as governed by {}, which is a product change, not a refactor",
                entry.name,
                domain.switch
            );
        }
    }

    // And the same membership reaches the operator surface, with what Vigil
    // currently chooses for each member and why.
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    {
        let _store =
            SettingsStore::open(deployment.path()).expect("open the node-side settings store");
    }
    let report =
        report_by_direct_read(deployment.path()).expect("build the settings report by direct read");

    let rendered_switches: BTreeSet<String> = report
        .domains
        .iter()
        .map(|domain| domain.switch.clone())
        .collect();
    assert_eq!(
        rendered_switches,
        roster
            .iter()
            .map(|domain| domain.switch.to_string())
            .collect::<BTreeSet<String>>(),
        "the surface shows every declared domain and no others"
    );

    let lines = report.render_lines();
    for domain in &report.domains {
        let declaration = roster
            .iter()
            .find(|declared| declared.switch == domain.switch)
            .unwrap_or_else(|| panic!("{} is rendered but not declared", domain.switch));
        assert!(
            domain.on,
            "an automatic-management domain defaults to on — that is the product working; {} \
             rendered off on an untouched deployment",
            domain.switch
        );

        let rendered_members: BTreeSet<String> = domain
            .members
            .iter()
            .map(|member| member.setting.clone())
            .collect();
        assert_eq!(
            rendered_members,
            declaration
                .members
                .iter()
                .map(|member| (*member).to_string())
                .collect::<BTreeSet<String>>(),
            "the domain names exactly the settings it governs; {}",
            domain.switch
        );

        for member in &domain.members {
            assert!(
                !member.reason.trim().is_empty(),
                "{} names what it currently chooses for {} but gives no reason; a blank reason \
                 is a defect, not an empty field",
                domain.switch,
                member.setting
            );
        }

        let domain_lines: Vec<&String> = lines
            .iter()
            .filter(|line| {
                line.trim_start().starts_with(DOMAIN_LINE_PREFIX) && line.contains(&domain.switch)
            })
            .collect();
        assert!(
            !domain_lines.is_empty(),
            "the domain is rendered on the surface under the `{DOMAIN_LINE_PREFIX}` prefix; {}",
            domain.switch
        );
        let rendered_text = domain_lines
            .iter()
            .map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for member in &domain.members {
            assert!(
                rendered_text.contains(&member.setting),
                "the surface names {} among the settings {} governs; rendered: {rendered_text:?}",
                member.setting,
                domain.switch
            );
            assert!(
                rendered_text.contains(member.reason.trim()),
                "the surface shows why {} chose what it did for {}; rendered: {rendered_text:?}",
                domain.switch,
                member.setting
            );
        }
    }
}
