//! The family that becomes the deployment's runtime user is CLOSED: no
//! subcommand reaches the owner channel from outside it, and every member
//! resolves the deployment's location before it becomes the deployment user.
//!
//! The owner channel authorizes on the peer's operating-system user and
//! nothing else, so a subcommand that opens it without first becoming this
//! deployment's runtime user is refused for a mismatch the operator never
//! chose and cannot see. The dispatch asks one list which subcommands must
//! take that route (`lib.rs`'s `commands_that_ask_the_running_deployment`), and
//! the members of that list are pinned as a family in
//! `every_live_command_dispatch_becomes_the_runtime_user_and_nothing_more.rs`.
//!
//! What no test held until now is the OTHER direction: that the list is the
//! whole set. `vigil settings` was a live command for as long as it existed
//! and was not in it, and the operator paid for that with a settings surface
//! refused inside the container while the read commands beside it were served.
//! Nothing about that omission was visible to anything — the list was
//! self-consistent, and so was every test written against it. A subcommand
//! added later that reaches the daemon and is not added to the list repeats
//! exactly that, silently.
//!
//! So this reads the dispatch itself. Every arm that routes to the owner
//! channel — through `print_control_or_direct`, which asks it directly, or
//! through `print_settings`, which asks it and falls back to the store — must
//! be a member of the family, and every member must have such an arm. A future
//! subcommand that reaches the daemon fails here on the day it is written
//! rather than on the day an operator finds it refused.
//!
//! Structural by necessity: the defect is a subcommand's ABSENCE from a list,
//! and an absence has no behavior to observe. A behavioral test can only
//! exercise the subcommands someone thought to name, which is the same set
//! someone thought to add to the list — the two blind spots are identical, so
//! a behavioral form of this guard would have passed against the settings
//! omission it exists to catch. The scan carries its own positive and negative
//! controls below, so a reader that recognized nothing cannot pass quietly.
//!
//! Nothing here sleeps, reads a clock, spawns a process, or reads the ambient
//! environment.

use std::path::PathBuf;

use vigil::commands_that_ask_the_running_deployment;

/// The dispatch this deployment answers its command line with.
fn dispatch_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read the command dispatch at {}: {error}", path.display()))
}

/// One arm of the command dispatch: the subcommand an operator types, and
/// whether answering it goes through the deployment's owner channel.
#[derive(Debug, PartialEq, Eq)]
struct DispatchArm {
    command: String,
    reaches_the_owner_channel: bool,
}

/// The prefix every subcommand arm of the dispatch is written with.
const ARM_PREFIX: &str = "Some(command) if command == \"";

/// The two ways an arm's body reaches the process that owns this deployment's
/// store. `print_control_or_direct` asks the owner channel and falls back to
/// the store when nobody is holding it; `print_settings` does the same for the
/// settings surface. Both open the channel, so both need the identity.
const OWNER_CHANNEL_CALLS: [&str; 2] = ["print_control_or_direct(", "print_settings("];

/// Every subcommand arm of the dispatch, in the order it is written.
///
/// An arm's body runs from its own line to the start of the next arm — the
/// next subcommand arm, the flag arms, or the catch-all that prints the help.
fn dispatch_arms(source: &str) -> Vec<DispatchArm> {
    let lines: Vec<&str> = source.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.trim_start().starts_with(ARM_PREFIX).then_some(index))
        .collect();
    let mut arms = Vec::new();
    for (position, start) in starts.iter().enumerate() {
        let trimmed = lines[*start].trim_start();
        let rest = &trimmed[ARM_PREFIX.len()..];
        let Some(end_quote) = rest.find('"') else {
            continue;
        };
        let command = rest[..end_quote].to_string();
        let end = starts
            .get(position + 1)
            .copied()
            .unwrap_or_else(|| next_arm_boundary(&lines, *start));
        let body = lines[*start..end].join("\n");
        arms.push(DispatchArm {
            reaches_the_owner_channel: OWNER_CHANNEL_CALLS.iter().any(|call| body.contains(call)),
            command,
        });
    }
    arms
}

/// Where the LAST subcommand arm's body stops: the catch-all that prints the
/// help for anything the dispatch does not recognize.
fn next_arm_boundary(lines: &[&str], start: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, line)| (line.trim_start() == "_ => {").then_some(index))
        .unwrap_or(lines.len())
}

#[test]
fn every_dispatch_that_asks_the_running_deployment_belongs_to_the_family() {
    let arms = dispatch_arms(&dispatch_source());
    let family = commands_that_ask_the_running_deployment();

    let outside_the_family: Vec<&str> = arms
        .iter()
        .filter(|arm| arm.reaches_the_owner_channel)
        .map(|arm| arm.command.as_str())
        .filter(|command| !family.contains(command))
        .collect();

    assert!(
        outside_the_family.is_empty(),
        "`vigil {}` reaches this deployment's owner channel and is not in the family that becomes \
         the deployment's runtime user first. The channel authorizes on the peer's \
         operating-system user and nothing else, so inside the container that subcommand runs as \
         root against a daemon at user 1000 and is refused for a mismatch the operator never \
         chose and cannot see — while the subcommands beside it are served. That is exactly what \
         `vigil settings` did. The family this deployment states is {family:?}",
        outside_the_family.join("`, `vigil ")
    );
}

#[test]
fn every_member_of_the_family_really_has_a_dispatch_that_asks_the_running_deployment() {
    // The same closure from the other side. A name left in the list after its
    // subcommand stopped reaching the daemon makes the family look wider than
    // it is, and the next reader trusts it.
    let arms = dispatch_arms(&dispatch_source());
    let family = commands_that_ask_the_running_deployment();

    for command in family {
        let arm = arms
            .iter()
            .find(|arm| arm.command == *command)
            .unwrap_or_else(|| {
                panic!(
                    "the family names `vigil {command}` as a subcommand that asks this \
                     deployment's running daemon, but the dispatch has no arm for it at all. \
                     Arms read: {arms:?}"
                )
            });
        assert!(
            arm.reaches_the_owner_channel,
            "the family names `vigil {command}` as a subcommand that asks this deployment's \
             running daemon, but its dispatch arm never opens the owner channel. A family that \
             lists subcommands which do not belong to it is a family nobody can read the closure \
             of"
        );
    }
}

#[test]
fn the_dispatch_takes_the_identity_step_from_the_family_list_and_not_a_second_list() {
    // The closure above is only worth anything if the family list is what the
    // dispatch actually consults. A second, hand-written list at the identity
    // step would drift from this one, and the drift would be invisible.
    let source = dispatch_source();
    assert!(
        source.contains("commands_that_ask_the_running_deployment().contains(&name)"),
        "the dispatch decides whether to become this deployment's runtime user by asking the \
         family list itself, once, for the whole family — never by re-deciding it at each arm, \
         which is how `vigil settings` came to open the owner channel as whoever ran it"
    );
    assert!(
        source.contains("privilege::adopt_runtime_user_for_live_command()"),
        "and what it does for a member of that family is become the runtime user before it \
         opens an owner channel or a store"
    );

    let location_resolution = source
        .find("match configured_locations()")
        .expect("the live-command dispatch resolves its deployment location");
    let identity_step = source
        .find("privilege::adopt_runtime_user_for_live_command()")
        .expect("the live-command dispatch adopts the deployment user");
    assert!(
        location_resolution < identity_step,
        "Supervisor exposes the add-on options file as root-only, so a live command must resolve \
         its data directory and store path while it can read that file, carry only those paths, \
         and then become the deployment user before it opens the owner channel or store"
    );
    assert_eq!(
        source.matches("match configured_locations()").count(),
        1,
        "the live-command family resolves its deployment location once at dispatch; a handler \
         that reads the Supervisor-owned options again after the identity change recreates the \
         packaged add-on permission failure"
    );
}

#[test]
fn the_dispatch_reader_tells_an_arm_that_reaches_the_daemon_from_one_that_does_not() {
    // A positive and a negative control on the reader itself, on planted text
    // rather than the real dispatch: a reader that recognized nothing would
    // report an empty set of owner-channel arms and pass the closure check
    // above while proving nothing at all.
    let planted = r#"
    match command {
        Some(command) if command == "events" => print_control_or_direct("events", ""),
        Some(command) if command == "settings" => {
            let request = args.collect::<Vec<String>>().join(" ");
            print_settings(&request)
        }
        Some(command) if command == "doctor" => match doctor::run(args.collect()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => ExitCode::from(2),
        },
        _ => {
            print_help();
        }
    }
"#;

    assert_eq!(
        dispatch_arms(planted),
        vec![
            DispatchArm {
                command: "events".to_string(),
                reaches_the_owner_channel: true,
            },
            DispatchArm {
                command: "settings".to_string(),
                reaches_the_owner_channel: true,
            },
            DispatchArm {
                command: "doctor".to_string(),
                reaches_the_owner_channel: false,
            },
        ],
        "the reader must find every subcommand arm and tell the ones that reach this \
         deployment's owner channel — on one line or in a block — from the ones that answer \
         locally; a reader that finds none passes the closure check having read nothing"
    );

    // And it really is reading THIS deployment's dispatch, not an empty file:
    // the real dispatch has arms of both kinds.
    let arms = dispatch_arms(&dispatch_source());
    assert!(
        arms.iter().any(|arm| arm.reaches_the_owner_channel),
        "the real dispatch has subcommands that ask the running daemon: {arms:?}"
    );
    assert!(
        arms.iter().any(|arm| !arm.reaches_the_owner_channel),
        "and subcommands that answer without it, which is what makes the closure a claim rather \
         than a tautology: {arms:?}"
    );
}
