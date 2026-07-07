//! Privilege drop with supplemental groups: `VIGIL_RUN_SUPPLEMENTAL_GIDS`
//! is applied via setgroups BEFORE setgid/setuid so the final process can
//! still open mapped render/video devices. The container proof runs in the
//! deployment smoke; this file pins the plan and its ordering.

use vigil::{PrivilegeStep, privilege_drop_plan};

#[test]
fn supplemental_gids_env_parses_and_orders_setgroups_before_setgid_setuid() {
    let plan = privilege_drop_plan(1000, 1000, Some("109,44")).expect("valid gid list");

    assert_eq!(
        plan,
        vec![
            PrivilegeStep::SetSupplementalGroups(vec![109, 44]),
            PrivilegeStep::SetGid(1000),
            PrivilegeStep::SetUid(1000),
        ],
        "supplemental groups must be applied BEFORE the gid/uid drop — after \
         setuid the process can no longer call setgroups"
    );
}

#[test]
fn no_supplemental_gids_still_clears_groups_before_drop() {
    // Without the env var the drop must not silently inherit root's
    // supplemental groups: the plan still sets (empty) groups first.
    let plan = privilege_drop_plan(1000, 1000, None).expect("no supplemental gids");
    assert_eq!(
        plan,
        vec![
            PrivilegeStep::SetSupplementalGroups(vec![]),
            PrivilegeStep::SetGid(1000),
            PrivilegeStep::SetUid(1000),
        ],
    );
}

#[test]
fn whitespace_and_empty_entries_are_tolerated() {
    let plan = privilege_drop_plan(1000, 1000, Some(" 109 , 44 ,")).expect("tolerant parse");
    assert_eq!(plan[0], PrivilegeStep::SetSupplementalGroups(vec![109, 44]),);
}

#[test]
fn invalid_gid_list_fails_loud() {
    let error = privilege_drop_plan(1000, 1000, Some("render,video"))
        .expect_err("names are not numeric gids — must fail loud, never be ignored");
    assert!(
        error.contains("VIGIL_RUN_SUPPLEMENTAL_GIDS"),
        "the error names the variable so the operator can fix it: {error}"
    );
}
