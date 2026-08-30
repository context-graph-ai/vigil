//! No live command is exempt: every `vigil` subcommand whose answer comes from
//! the running deployment becomes that deployment's runtime user first, and
//! does nothing else on the way.
//!
//! Its neighbour
//! (`a_packaged_live_command_becomes_the_deployments_runtime_user.rs`) pins
//! WHAT the drop is — the same plan the daemon executes, ordered the same way,
//! carrying no filesystem step, because a read command brings no deployment
//! into existence. It pins it on one plan, for the live-command path in
//! general. That left room for a dispatch to be a live command and not take
//! that path at all, and one of them was: `vigil settings` reaches the daemon
//! over the same owner channel as `vigil why`, `vigil events` and `vigil
//! stats`, and it opened that channel as whatever user the exec came in as.
//! In the container that is root against a daemon at user 1000, so the surface
//! an operator reaches for to CHANGE their deployment was refused for a
//! mismatch they never chose and cannot see, while the read commands beside it
//! were served.
//!
//! A per-command fix would leave the same room. What is pinned here is the
//! whole family at once, from the deployment's own list of the commands that
//! ask it for an answer: every one of them has an identity plan, every one is
//! the SAME plan, and not one of them touches a path. A subcommand added to
//! that family later is covered the day it is added.
//!
//! Both halves matter and they pull opposite ways. The positive half is the
//! add-on's promise that a command inside the container reaches the daemon
//! standing next to it. The negative half is what a read command must never
//! do: the daemon creates the store's parent and chowns the parent, the store
//! and its lock, because the daemon is the process that brings a deployment
//! into existence, and a `vigil settings` that created a store directory or
//! moved the ownership of a store a daemon is holding would be a reader
//! rewriting the deployment it came to ask about — against a bind-mounted
//! `/data` on a container started as root, the operator's real files.
//!
//! Pinned on the PLAN because the drop itself needs root this harness has not
//! got: a plan is the honest unit of comparison, it is how the privilege
//! family already pins setgroups before setgid/setuid
//! (`privilege_supplemental_gids.rs`), and it makes the negative claim
//! provable rather than merely asserted. That the settings dispatch really
//! performs the step at runtime is pinned from outside the process on the real
//! binary
//! (`vigil-bin/tests/the_settings_command_becomes_the_runtime_user_like_every_live_command.rs`).
//!
//! Nothing here sleeps, reads a clock, spawns a process, or reads the ambient
//! environment: every leg is a pure call with the deployment's configured
//! identity handed in, so no two of them can interfere.

use std::path::{Path, PathBuf};

use vigil::{PrivilegeStep, RuntimeUserStep};
use vigil::{commands_that_ask_the_running_deployment, runtime_user_plan_for_dispatched_command};
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

/// What one subcommand's dispatch does to become this deployment's runtime
/// user, or the failure of a dispatch that does nothing at all.
fn plan_for(command: &str) -> Vec<RuntimeUserStep> {
    runtime_user_plan_for_dispatched_command(command, RUNTIME_UID, RUNTIME_GID, Some(SUPPLEMENTAL))
        .unwrap_or_else(|error| panic!("`vigil {command}`'s own runtime-user plan: {error}"))
        .unwrap_or_else(|| {
            panic!(
                "`vigil {command}` asks this deployment's daemon for its answer, and the owner \
                 channel it asks over authorizes on the peer's operating-system user and nothing \
                 else — so a dispatch that performs no identity step at all opens that channel \
                 as whoever ran it, and is refused for a mismatch the operator never chose and \
                 cannot see. Inside the container that is root against a daemon at user 1000."
            )
        })
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
fn the_settings_command_asks_the_running_deployment_like_every_other_live_command() {
    // Without this the two legs below are satisfied by omission: a family list
    // that quietly left `settings` out would pass them while the operator's
    // settings command stayed refused in the container.
    let family = commands_that_ask_the_running_deployment();
    for command in ["settings", "events", "stats", "why"] {
        assert!(
            family.contains(&command),
            "`vigil {command}` gets its answer from the running deployment over the owner \
             channel, so it belongs to the family that becomes the deployment's runtime user \
             first. The family this deployment states is {family:?}"
        );
    }
}

#[test]
fn every_command_that_asks_the_running_deployment_becomes_its_runtime_user() {
    let expected = identity_of(
        &runtime_user_plan_for_live_command(RUNTIME_UID, RUNTIME_GID, Some(SUPPLEMENTAL))
            .expect("a live command's runtime-user plan"),
    );
    let daemon = identity_of(
        &runtime_user_plan_for_daemon(
            Path::new(PACKAGED_STORE),
            RUNTIME_UID,
            RUNTIME_GID,
            Some(SUPPLEMENTAL),
        )
        .expect("the daemon's own runtime-user plan"),
    );
    assert_eq!(
        expected, daemon,
        "sanity: a live command and the daemon arrive at the same identity, which is the whole \
         reason the channel between them opens at all"
    );

    for command in commands_that_ask_the_running_deployment() {
        assert_eq!(
            identity_of(&plan_for(command)),
            expected,
            "`vigil {command}` must become the SAME operating-system user the daemon holds the \
             channel as, ordered the same way — supplemental groups before the gid/uid drop, \
             because after setuid the process can no longer call setgroups. A command that \
             becomes any other identity is refused, and the add-on's promise that it reaches the \
             daemon standing next to it fails on identity alone, on a deployment where the store \
             is healthy and the answer is sitting right there"
        );
    }
}

#[test]
fn no_command_that_asks_the_running_deployment_prepares_the_store_it_came_to_read() {
    for command in commands_that_ask_the_running_deployment() {
        assert_eq!(
            paths_touched(&plan_for(command)),
            Vec::<PathBuf>::new(),
            "`vigil {command}` brings no deployment into existence: a command that created a \
             store directory, or moved the ownership of a store a daemon is holding, is a reader \
             rewriting the deployment it came to ask about — and in a container started as root \
             against a bind-mounted /data it would do that to the operator's real files"
        );
    }

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
             commands would be comparing against nothing",
            expected.display()
        );
    }
}
