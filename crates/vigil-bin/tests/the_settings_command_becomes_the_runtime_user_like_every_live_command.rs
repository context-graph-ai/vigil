//! `vigil settings` inside the container must reach the daemon standing next
//! to it, on the same terms as `vigil why`, `vigil events` and `vigil stats`.
//!
//! A packaged Vigil drops to the deployment's configured runtime user before
//! it opens its store, and the owner channel those commands travel authorizes
//! on ONE thing — the peer's operating-system user. So a live command that
//! keeps the exec's user is refused for a mismatch the operator never chose
//! and cannot see. The commands that dispatch through
//! `print_control_or_direct` became the deployment's runtime user first; the
//! settings command dispatches through `print_settings`, which never did. In
//! the container that is root against a daemon at user 1000, and `vigil
//! settings` — the surface an operator reaches for to CHANGE their deployment
//! — is refused while the read commands beside it are served.
//!
//! What this file proves from outside the process is that the settings
//! dispatch performs the identity step at all. The step reads the deployment's
//! stated runtime identity before it opens a channel or a store, so a
//! deployment that states an identity vigil cannot read is refused there,
//! by name, whatever the store is doing. That refusal is observable without
//! root, which is what makes the step observable without root: the drop itself
//! needs a privilege this harness does not have, and a fixture that tried to
//! assert on the drop would be asserting on a condition it never created.
//!
//! What the drop CONSISTS of — the same plan the daemon executes, ordered the
//! same way, and touching no path at all, because a read command brings no
//! deployment into existence — is pinned beside this file on the plans
//! themselves (`vigil/tests/a_packaged_live_command_becomes_the_deployments_runtime_user.rs`
//! and `vigil/tests/every_live_command_dispatch_becomes_the_runtime_user_and_nothing_more.rs`).
//! The real-image proof — a daemon at user 1000, `vigil settings` run the way
//! the package runs one, served — belongs to the add-on image gate, which runs
//! a container this harness cannot.
//!
//! Nothing here sleeps, reads a clock, binds a port, or starts a daemon: each
//! leg spawns one command against a fresh deployment directory and reads what
//! it answered.

use std::path::Path;
use std::process::{Command, Output};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::vigil_binary_path;

/// The runtime identity a packaged deployment states, written the way the
/// package writes it — except that the user is not a number. An operator
/// typing a NAME where their deployment wants a numeric user is the ordinary
/// way this configuration is wrong, and it is wrong in a way every command
/// that reads it must say out loud rather than answer around.
const RUNTIME_USER_VARIABLE: &str = "VIGIL_RUN_UID";
const UNREADABLE_RUNTIME_USER: &str = "vigil";

/// Run one `vigil` subcommand against a deployment that states a runtime
/// identity it cannot read.
fn run_stating_an_unreadable_runtime_user(data_dir: &Path, args: &[&str]) -> Output {
    command_against(data_dir, args)
        .env("VIGIL_DROP_PRIVILEGES", "1")
        .env(RUNTIME_USER_VARIABLE, UNREADABLE_RUNTIME_USER)
        .output()
        .expect("spawn vigil")
}

/// Run one `vigil` subcommand against a deployment that states no runtime
/// identity at all.
fn run_stating_no_runtime_user(data_dir: &Path, args: &[&str]) -> Output {
    command_against(data_dir, args)
        .env_remove("VIGIL_DROP_PRIVILEGES")
        .env_remove(RUNTIME_USER_VARIABLE)
        .env_remove("VIGIL_RUN_GID")
        .output()
        .expect("spawn vigil")
}

fn command_against(data_dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(vigil_binary_path());
    command
        .args(args)
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_STORE_PATH")
        .env_remove("VIGIL_CONTROL_SOCKET");
    command
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Everything a live command owes a deployment whose stated runtime identity
/// cannot be read: it does not answer as though the statement were not there,
/// and it names the surface the operator has to fix.
fn assert_refused_by_name(output: &Output, command: &str) {
    let stderr = stderr_of(output);
    assert!(
        !output.status.success(),
        "`vigil {command}` must not answer a deployment whose stated runtime identity it could \
         not read — the identity is how this command reaches the daemon at all, and an answer \
         given without it is an answer given as somebody else: {output:?}"
    );
    assert!(
        stderr.contains(RUNTIME_USER_VARIABLE),
        "the refusal names the surface the operator has to fix, so the deployment is repairable \
         from what `vigil {command}` said: {stderr:?}"
    );
}

#[test]
fn the_settings_command_reads_this_deployments_runtime_identity_before_it_answers() {
    let tmp = tempfile::tempdir().expect("data dir");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create the deployment directory");

    let listed = run_stating_an_unreadable_runtime_user(&data_dir, &["settings"]);

    assert_refused_by_name(&listed, "settings");
}

#[test]
fn the_live_commands_that_already_read_it_refuse_the_same_way() {
    // The control, and it is not decoration: without it the assertion above is
    // satisfied by any refusal at all, and this deployment has plenty of other
    // reasons to refuse — there is no daemon and no store. These commands
    // reach the daemon through the same channel `vigil settings` reaches it
    // through, they already read this deployment's stated identity first, and
    // this is what that refusal looks like.
    let tmp = tempfile::tempdir().expect("data dir");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create the deployment directory");

    for command in ["events", "stats"] {
        let refused = run_stating_an_unreadable_runtime_user(&data_dir, &[command]);
        assert_refused_by_name(&refused, command);
    }
}

#[test]
fn a_deployment_that_states_no_runtime_identity_still_gets_its_settings_answer() {
    // The other control. A deployment that states no runtime identity keeps
    // the identity it was started with, and the settings listing is served the
    // way it always was — so the refusal above is the identity step happening,
    // never a command that stopped answering.
    let tmp = tempfile::tempdir().expect("data dir");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create the deployment directory");

    let listed = run_stating_no_runtime_user(&data_dir, &["settings"]);

    assert!(
        listed.status.success(),
        "a deployment that states no runtime identity is not misconfigured, and its settings \
         listing is owed an answer: {listed:?}"
    );
    assert!(
        !stderr_of(&listed).contains(RUNTIME_USER_VARIABLE),
        "nothing about the runtime identity belongs in the answer of a deployment that stated \
         none: {:?}",
        stderr_of(&listed)
    );
}
