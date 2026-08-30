//! The daemon hands its runtime user the lock file its store REALLY has.
//!
//! A packaged deployment starts as root, prepares the surface its store needs,
//! and then becomes the unprivileged runtime user it runs as for the rest of
//! its life. Preparing that surface means the store's parent, the store file,
//! and the store's companion lock: after the drop the process can no longer
//! chown anything, so a companion left owned by root is a companion the
//! running daemon cannot take — the store fails to open on a container that
//! bind-mounts `/data`, and the operator's deployment does not come up.
//!
//! The companion's name is not vigil's to choose. contextdb defines it by
//! APPENDING `.lock` to the whole store filename — `alpha.db` has
//! `alpha.db.lock`, and this deployment's `/data/store.contextgraph` has
//! `/data/store.contextgraph.lock`. Vigil hand-derived that name instead, by
//! REPLACING the store's extension (`privilege.rs`'s
//! `store_path.with_extension("lock")`), which names `/data/store.lock`: a
//! path nothing ever creates. So the daemon chowns a file that does not exist,
//! the real companion keeps whatever ownership it was created with, and the
//! one preparation step that exists to stop a root-created companion locking
//! the runtime user out of its own store does nothing at all.
//!
//! Pinned on the PLAN rather than on a running daemon because the drop itself
//! needs root this harness has not got, and because a plan is the honest unit
//! of comparison — it is how the privilege family already pins setgroups
//! before setgid/setuid (`privilege_supplemental_gids.rs`). The name asserted
//! is the literal one an operator would see on their own filesystem, so a
//! future edit that changes how the name is derived cannot also change what
//! this file expects.
//!
//! Nothing here sleeps, reads a clock, spawns a process, touches a filesystem,
//! or reads the ambient environment: every leg is a pure call with the
//! deployment's configured identity handed in.

use std::path::{Path, PathBuf};

use vigil::{RuntimeUserStep, runtime_user_plan_for_daemon};

/// A packaged deployment's store, at the pathname the add-on ships.
const PACKAGED_STORE: &str = "/data/store.contextgraph";

/// The companion lock that store really has: contextdb appends `.lock` to the
/// whole filename, extension included.
const PACKAGED_COMPANION_LOCK: &str = "/data/store.contextgraph.lock";

/// The path a store filename's extension REPLACED by `lock` names. Nothing in
/// this deployment ever creates it.
const NO_SUCH_LOCK: &str = "/data/store.lock";

/// The identity a packaged deployment runs as.
const RUNTIME_UID: u32 = 1000;
const RUNTIME_GID: u32 = 1000;

/// A device-access group the packaged runtime keeps across the drop.
const SUPPLEMENTAL: &str = "109,44";

/// Every path the daemon's plan would create or take ownership of.
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

fn daemon_plan() -> Vec<RuntimeUserStep> {
    runtime_user_plan_for_daemon(
        Path::new(PACKAGED_STORE),
        RUNTIME_UID,
        RUNTIME_GID,
        Some(SUPPLEMENTAL),
    )
    .expect("the daemon's own runtime-user plan")
}

#[test]
fn the_daemon_hands_its_runtime_user_the_companion_lock_its_store_really_has() {
    let touched = paths_touched(&daemon_plan());

    assert!(
        touched.contains(&PathBuf::from(PACKAGED_COMPANION_LOCK)),
        "this deployment's store is {PACKAGED_STORE} and the companion lock beside it is \
         {PACKAGED_COMPANION_LOCK} — contextdb appends `.lock` to the whole filename rather than \
         replacing the store's own extension. The daemon prepares that companion while it is \
         still root because after the drop it cannot chown anything, and a companion left owned \
         by root is one the running daemon cannot take: the store does not open and the \
         deployment does not come up. Prepared instead: {touched:?}"
    );
}

#[test]
fn the_daemon_prepares_no_lock_path_this_deployment_does_not_have() {
    let touched = paths_touched(&daemon_plan());

    assert!(
        !touched.contains(&PathBuf::from(NO_SUCH_LOCK)),
        "{NO_SUCH_LOCK} is what this store's filename looks like with its extension REPLACED by \
         `lock`, and nothing in this deployment ever creates it. Preparing it is not a harmless \
         extra step: it is the step that was supposed to prepare the real companion, so an \
         operator reading the plan sees the companion covered while the file that actually \
         guards their store is untouched. Prepared: {touched:?}"
    );
}

#[test]
fn the_daemon_still_prepares_the_store_it_is_about_to_open_and_the_directory_it_lives_in() {
    // The contrast that keeps the two legs above from being satisfied by a
    // daemon that simply stopped preparing anything: the parent and the store
    // file are still prepared, in the same plan, because the daemon is the
    // process that brings a deployment into existence.
    let touched = paths_touched(&daemon_plan());

    for expected in [Path::new("/data"), Path::new(PACKAGED_STORE)] {
        assert!(
            touched.contains(&expected.to_path_buf()),
            "the daemon prepares {} for the runtime user it is about to become — dropping that \
             leaves a deployment that cannot open its own store: {touched:?}",
            expected.display()
        );
    }
}
