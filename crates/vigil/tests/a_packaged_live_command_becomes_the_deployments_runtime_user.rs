//! Inside the container, `vigil why` must reach the daemon standing next to
//! it — and it only can if it is the same operating-system user.
//!
//! The add-on's promise is plain: an operator opens a terminal in the Vigil
//! container and asks `vigil why`, `vigil events`, `vigil stats`, and the
//! running daemon answers. The owner channel those commands travel is
//! authorized on ONE thing — the peer's operating-system user — and contextdb
//! refuses any other (`local_transport::authentication::authenticate_peer`
//! compares the observed user against the owner's and returns a refusal on any
//! mismatch, with no second rule to fall back on).
//!
//! A packaged Vigil drops to the configured runtime user before it opens its
//! store: `runtime::run` calls `privilege::prepare_runtime_user`, so the
//! daemon holds the channel as that user. Nothing does the same on the command
//! side — live-command dispatch keeps whatever user the exec came in as. In
//! the container that is root, the daemon is user 1000, and every one of the
//! three commands is refused for a mismatch the operator never chose and
//! cannot see. The promise fails on identity alone, on a deployment where the
//! store is healthy, the daemon is serving, and the answer is sitting right
//! there.
//!
//! The other half of the same contract is what the command must NOT do.
//! `prepare_runtime_user` creates the store's parent directory and chowns the
//! parent, the store and its lock, because the daemon is the process that
//! brings a deployment into existence. A read command brings nothing into
//! existence. A `vigil why` that created a store directory, or moved the
//! ownership of a store a daemon is holding, would be a reader rewriting the
//! deployment it came to ask a question of — and on a container started as
//! root against a bind-mounted `/data`, it would do that to the operator's
//! real files. So the command path is identity and NOTHING else.
//!
//! What is pinned here is the PLAN both sides execute, because the drop itself
//! needs root and this harness has none: a plan is the honest unit of
//! comparison, it is how this file's neighbour already pins the ordering of
//! setgroups before setgid/setuid (`privilege_supplemental_gids.rs`), and it
//! makes the negative claim provable rather than merely asserted — the
//! command's plan can be shown to contain no filesystem step at all, beside a
//! daemon plan that demonstrably does. The real-image proof — a daemon at user
//! 1000, a command started the way the package starts one, all three reads
//! served — belongs to the add-on image gate, which runs a container this
//! harness cannot.
//!
//! Nothing here sleeps, reads a clock, spawns a process, or reads the ambient
//! environment: every leg is a pure call with the deployment's configured
//! identity handed in, so no two of them can interfere.

use std::path::{Path, PathBuf};

use vigil::{PrivilegeStep, RuntimeUserStep, privilege_drop_plan};
use vigil::{runtime_user_plan_for_daemon, runtime_user_plan_for_live_command};

/// A packaged deployment's store, at the pathname the add-on ships.
const PACKAGED_STORE: &str = "/data/store.contextgraph";

/// The identity a packaged deployment runs as.
const RUNTIME_UID: u32 = 1000;
const RUNTIME_GID: u32 = 1000;

/// A device-access group the packaged runtime keeps across the drop, so this
/// file's comparison covers the whole identity rather than just uid/gid.
const SUPPLEMENTAL: &str = "109,44";

/// The identity half of a plan, in execution order.
fn identity_of(plan: &[RuntimeUserStep]) -> Vec<PrivilegeStep> {
    plan.iter()
        .filter_map(|step| match step {
            RuntimeUserStep::Drop(step) => Some(step.clone()),
            _ => None,
        })
        .collect()
}

/// Every path a plan would create or take ownership of.
fn paths_touched(plan: &[RuntimeUserStep]) -> Vec<PathBuf> {
    plan.iter()
        .filter_map(|step| match step {
            RuntimeUserStep::CreateStoreParent(path) | RuntimeUserStep::ChownIfExists(path) => {
                Some(path.clone())
            }
            RuntimeUserStep::Drop(_) => None,
        })
        .collect()
}

/// The companion lock a store really has: contextdb appends `.lock` to the
/// whole store filename, keeping the store's own extension, so
/// `/data/store.contextgraph` is guarded by `/data/store.contextgraph.lock`.
/// Replacing the extension instead names `/data/store.lock`, which nothing in
/// this deployment ever creates.
fn appended_companion_lock(store: &Path) -> PathBuf {
    let mut name = store.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

#[test]
fn a_live_command_becomes_the_same_runtime_user_the_daemon_dropped_to() {
    let daemon = runtime_user_plan_for_daemon(
        Path::new(PACKAGED_STORE),
        RUNTIME_UID,
        RUNTIME_GID,
        Some(SUPPLEMENTAL),
    )
    .expect("the daemon's own runtime-user plan");
    let command = runtime_user_plan_for_live_command(RUNTIME_UID, RUNTIME_GID, Some(SUPPLEMENTAL))
        .expect("a live command's runtime-user plan");

    assert_eq!(
        identity_of(&command),
        identity_of(&daemon),
        "the owner channel authorizes on the operating-system user and nothing else, so a live \
         command that becomes any other identity than the daemon's is refused for a mismatch the \
         operator never chose — the add-on's promise that `vigil why` inside the container \
         reaches the daemon fails on identity alone"
    );
    assert_eq!(
        identity_of(&command),
        privilege_drop_plan(RUNTIME_UID, RUNTIME_GID, Some(SUPPLEMENTAL))
            .expect("this deployment's drop plan"),
        "and it is the SAME drop, ordered the same way: supplemental groups before the gid/uid \
         drop, because after setuid the process can no longer call setgroups"
    );
}

#[test]
fn a_live_command_never_prepares_the_store_it_came_to_read() {
    let command = runtime_user_plan_for_live_command(RUNTIME_UID, RUNTIME_GID, Some(SUPPLEMENTAL))
        .expect("a live command's runtime-user plan");

    assert_eq!(
        paths_touched(&command),
        Vec::<PathBuf>::new(),
        "a read command brings no deployment into existence: a `vigil why` that created a store \
         directory, or moved the ownership of a store a daemon is holding, is a reader rewriting \
         the deployment it came to ask a question of — and in a container started as root \
         against a bind-mounted /data it would do that to the operator's real files"
    );

    // The contrast is real rather than vacuous: the DAEMON's plan does prepare
    // the store, because the daemon is the process that brings a deployment
    // into existence.
    let daemon = runtime_user_plan_for_daemon(
        Path::new(PACKAGED_STORE),
        RUNTIME_UID,
        RUNTIME_GID,
        Some(SUPPLEMENTAL),
    )
    .expect("the daemon's own runtime-user plan");
    let store = PathBuf::from(PACKAGED_STORE);
    let parent = store
        .parent()
        .expect("the packaged store has a parent")
        .to_path_buf();
    // contextdb names a store's companion lock by APPENDING `.lock` to the
    // whole filename -- `/data/store.contextgraph` has
    // `/data/store.contextgraph.lock`, never `/data/store.lock`. The daemon
    // prepares the file its store really has, so that is the file this
    // contrast is drawn against.
    let lock = appended_companion_lock(&store);
    for expected in [&parent, &store, &lock] {
        assert!(
            paths_touched(&daemon).contains(expected),
            "sanity: the daemon prepares {} — without that this file's negative claim about the \
             command would be comparing against nothing",
            expected.display()
        );
    }
}

#[test]
fn a_live_command_refuses_an_unusable_group_list_as_loudly_as_the_daemon() {
    // The two sides must fail the same way as well as succeed the same way. A
    // command that quietly ignored a group list the daemon fails on would drop
    // to a different identity than the daemon holds the channel as, which is
    // the same refusal by a longer road.
    let error = runtime_user_plan_for_live_command(RUNTIME_UID, RUNTIME_GID, Some("render,video"))
        .expect_err("names are not numeric gids — a live command must fail loud, never ignore it");
    assert!(
        error.contains("VIGIL_RUN_SUPPLEMENTAL_GIDS"),
        "the error names the variable so the operator can fix it: {error}"
    );
}
