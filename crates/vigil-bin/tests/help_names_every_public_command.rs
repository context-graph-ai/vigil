//! The public command inventory: every subcommand an operator is entitled to
//! find is named on `vigil --help`, the one internal subcommand is not, and
//! every name help prints is a name the binary actually dispatches.
//!
//! The defect this holds shut is an OMISSION, which no single command's own
//! test can see. `events`, `why`, `stats`, `settings`, `enroll`, `forget` and
//! `doctor acceleration` were all dispatched and answered while `--help` named
//! `run`, `settings` and `fabric ticket` alone — so the one surface an operator
//! reads to find out what their deployment can do said most of it did not
//! exist, and `docs/cli.md` carried a standing admission of that gap.
//!
//! Both directions are checked, because either one alone leaves the operator
//! wrong: nothing advertised may be missing from the binary, and nothing public
//! in the binary may be missing from the advertisement. The inventory itself is
//! frozen here as an exact list, so adding a command without deciding whether
//! operators may have it fails rather than passes quietly.

use std::path::PathBuf;
use std::process::Command;

// `VIGIL_ACCEPTANCE_BIN` is the restored process binary when this test runs
// from the immutable nextest archive on a fresh closeout runner. Cargo's
// compile-time path names the builder's target directory and is not valid
// there.
#[allow(clippy::disallowed_methods)]
fn vigil_binary_path() -> PathBuf {
    std::env::var_os("VIGIL_ACCEPTANCE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_vigil")))
}

/// The exact set of subcommands this binary offers an operator, in the order
/// help prints them. Retyped here rather than read from the binary: a list that
/// only ever compared itself to itself would pass whatever it became, and the
/// point of this file is that adding, removing or renaming a public command is
/// a decision somebody takes deliberately.
const PUBLIC_COMMANDS: [&str; 9] = [
    "run",
    "events",
    "why",
    "stats",
    "settings",
    "enroll",
    "forget",
    "doctor acceleration",
    "fabric ticket",
];

/// The one dispatched subcommand help must never name. It is the detection
/// probe machinery's own subprocess entry point, carrying wire arguments, and
/// an operator who found it on help would reasonably run it by hand.
const INTERNAL_COMMAND: &str = "detector-probe";

/// What the binary prints when it did not recognize the command it was given:
/// the help block, on standard output, with a failing status. Its presence
/// after a real subcommand is how this file tells a dispatched name from an
/// advertised one that goes nowhere.
const UNRECOGNIZED_COMMAND_OUTPUT: &str = "Usage: vigil <COMMAND>";

fn help_output() -> String {
    let output = Command::new(vigil_binary_path())
        .arg("--help")
        // Retired variables are refused ahead of dispatch, so a stray one in
        // the caller's environment would answer for the binary instead of the
        // help block this reads.
        .env_remove("VIGIL_CONTROL_SOCKET")
        .output()
        .expect("spawn vigil --help");
    assert!(output.status.success(), "vigil --help must exit 0");
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// The lines of the help block's command listing, indentation trimmed.
fn advertised_lines(help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|line| line.trim() != "Commands:")
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .collect()
}

/// Unfakeable because the text compared is what a real process printed and the
/// list compared against is the one the binary renders it from: a help block
/// that dropped, reordered or reworded a command cannot match, and neither can
/// an inventory that grew an entry help does not print.
#[test]
fn help_prints_exactly_the_declared_public_inventory() {
    let advertised = advertised_lines(&help_output());
    let declared: Vec<String> = vigil::public_commands()
        .iter()
        .map(|(command, arguments)| {
            if arguments.is_empty() {
                (*command).to_string()
            } else {
                format!("{command} {arguments}")
            }
        })
        .collect();
    assert_eq!(
        advertised, declared,
        "the help block and the declared public inventory must be one statement, not two that \
         can drift"
    );
}

/// Unfakeable because the expected list is written out here in full: a command
/// added to the inventory without a decision about whether operators may have
/// it fails on this assertion, and so does one quietly dropped from it.
#[test]
fn the_declared_inventory_is_exactly_the_nine_public_commands() {
    let declared: Vec<&str> = vigil::public_commands()
        .iter()
        .map(|(command, _)| *command)
        .collect();
    assert_eq!(
        declared,
        PUBLIC_COMMANDS.to_vec(),
        "the public command inventory changed; a subcommand joining or leaving what operators \
         are offered is a decision, and it is taken here"
    );
}

/// Unfakeable because it reads the real help text: an implementation that
/// rendered the internal entry from the same loop as the rest would print it.
#[test]
fn help_names_no_internal_subcommand() {
    let help = help_output();
    assert!(
        !help.contains(INTERNAL_COMMAND),
        "`{INTERNAL_COMMAND}` is an internal subprocess entry point and must not be offered on \
         help; got:\n{help}"
    );
}

/// Unfakeable because every name is handed to a real process and the answer
/// read back is the process's own: a name help advertises that reaches no
/// dispatch arm falls through to the unrecognized-command arm, which prints the
/// help block again, and that is exactly what is asserted absent.
///
/// `run` is the one entry not invoked here — it starts the daemon, and a test
/// that started one to prove a name is dispatched would leave a camera runtime
/// behind. It is dispatched under every acceptance suite that spawns it, and
/// `binary_cli_surface_smoke::help_flag_names_the_run_command` holds its place
/// on help.
#[test]
fn every_advertised_command_is_dispatched_by_the_binary() {
    let deployment = tempfile::tempdir().expect("tempdir");
    for command in PUBLIC_COMMANDS {
        let first_word = command
            .split_whitespace()
            .next()
            .expect("every advertised command names at least one word");
        // `run` starts the daemon; it is dispatched under every acceptance
        // suite that spawns one, and starting a camera runtime here to prove a
        // name would leave that runtime behind.
        if first_word == "run" {
            continue;
        }
        let output = Command::new(vigil_binary_path())
            .arg(first_word)
            .env("VIGIL_DATA_DIR", deployment.path())
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .output()
            .unwrap_or_else(|error| panic!("spawn vigil {first_word}: {error}"));
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            !stdout.contains(UNRECOGNIZED_COMMAND_OUTPUT),
            "`vigil {first_word}` is advertised on help, so the binary must dispatch it; it \
             answered with the unrecognized-command help block instead:\n{stdout}"
        );
    }
}

/// Unfakeable because the family list is the one the dispatch itself takes its
/// identity step from: a subcommand added to it is a subcommand that asks this
/// deployment's running process for an answer, and a live command an operator
/// cannot discover is a live command they do not have.
#[test]
fn every_command_that_asks_the_running_deployment_is_advertised() {
    let advertised: Vec<&str> = vigil::public_commands()
        .iter()
        .map(|(command, _)| *command)
        .collect();
    for member in vigil::commands_that_ask_the_running_deployment() {
        assert!(
            advertised.contains(member),
            "`vigil {member}` asks this deployment's running process for its answer and must be \
             discoverable on help; help offers {advertised:?}"
        );
    }
}
