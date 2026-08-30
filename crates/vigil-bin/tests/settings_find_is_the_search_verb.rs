//! Searching the settings listing is `vigil settings find <text>`, and there is
//! no `domain` subcommand.
//!
//! The old name promised a declared grouping and delivered a substring search
//! over the whole rendered answer — `settings domain machine` returned two
//! settings because the word appears in their explanations, and "machine" is
//! not a domain. The owner ruled the operation SEARCH and renamed the verb
//! (DR-28): the breadth is kept and now honest, and the old name is gone with
//! no compatibility alias, so an operator who types it is told what to type
//! instead rather than being answered by a verb that no longer exists.
//!
//! Three answers are pinned because they are three different things an operator
//! does: a search that matches narrows the listing to exactly the lines
//! carrying the text, whichever kind of line they are; a search that matches
//! nothing SUCCEEDED and found nothing, which is not a refusal and must not
//! exit as one; and a search with no text at all is a usage error, because
//! nothing was asked for.
//!
//! Everything runs against a deployment that has never started, so the answer
//! is the automatic floor this artifact would run at — no runtime, no store,
//! and no dependence on what any previous test left behind.

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use vigil::settings_projection::{DOMAIN_LINE_PREFIX, SETTING_LINE_PREFIX};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::vigil_binary_path;

/// A word that appears in the NAME of several settings.
const NAME_TEXT: &str = "detector";

/// A word that appears only in explanation prose — the breadth the ruling keeps
/// deliberately: an operator searching for what a line SAYS finds it.
const EXPLANATION_TEXT: &str = "machine";

/// Text no line of any listing carries.
const NO_MATCH_TEXT: &str = "zzzz-no-such-text";

/// The status a request that was not served exits with.
const REFUSED: i32 = 2;

fn settings(data_dir: &Path, request: &[&str]) -> Output {
    Command::new(vigil_binary_path())
        .arg("settings")
        .args(request)
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_STORE_PATH")
        .env_remove("VIGIL_CONTROL_SOCKET")
        .stdin(Stdio::null())
        .output()
        .expect("spawn `vigil settings`")
}

fn deployment() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("temporary deployment directory");
    fs::create_dir_all(tmp.path().join("data")).expect("create the deployment directory");
    tmp
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Unfakeable because the answer is compared line by line against the full
/// listing the same deployment renders: every line that carries the text is
/// present and every line that does not is gone, so a filter that dropped an
/// explanation line or kept an unrelated one is caught either way.
#[test]
fn find_narrows_the_listing_to_every_line_carrying_the_text() {
    let tmp = deployment();
    let data_dir = tmp.path().join("data");

    let whole = settings(&data_dir, &[]);
    assert!(
        whole.status.success(),
        "the listing must answer before a search over it means anything; stderr:\n{}",
        stderr_of(&whole)
    );
    let listing = stdout_of(&whole);

    for text in [NAME_TEXT, EXPLANATION_TEXT] {
        let found = settings(&data_dir, &["find", text]);
        assert_eq!(
            found.status.code(),
            Some(0),
            "`vigil settings find {text}` matched lines and is a served request; stdout:\n{}\n\
             stderr:\n{}",
            stdout_of(&found),
            stderr_of(&found)
        );
        let answer = stdout_of(&found);
        let expected: Vec<&str> = listing.lines().filter(|line| line.contains(text)).collect();
        assert!(
            !expected.is_empty(),
            "fixture: the listing must carry lines containing `{text}` or this proves nothing:\n\
             {listing}"
        );
        let got: Vec<&str> = answer
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        assert_eq!(
            got, expected,
            "`vigil settings find {text}` answers with exactly the lines of the listing that \
             carry the text, in the order the listing renders them"
        );
    }

    // The breadth the ruling keeps: a word that appears only in what a line
    // SAYS still finds that line, and both a setting and a domain line can
    // match, because the search is over the whole rendered answer.
    let explained = stdout_of(&settings(&data_dir, &["find", EXPLANATION_TEXT]));
    assert!(
        explained
            .lines()
            .any(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} "))),
        "a search over the explanations still returns the settings whose explanation carries the \
         word:\n{explained}"
    );
    assert!(
        explained
            .lines()
            .any(|line| line.starts_with(&format!("{DOMAIN_LINE_PREFIX} "))),
        "and the control lines are searched with everything else — the ordinary listing keeps \
         them and so does a search:\n{explained}"
    );
}

/// Unfakeable because a no-match search and a refusal are told apart by the
/// exit status of a real process: an operator who searched for something this
/// deployment does not have asked a good question and got a true answer.
#[test]
fn a_search_that_matches_nothing_succeeded_and_found_nothing() {
    let tmp = deployment();
    let data_dir = tmp.path().join("data");

    let found = settings(&data_dir, &["find", NO_MATCH_TEXT]);
    let stdout = stdout_of(&found);
    let stderr = stderr_of(&found);

    assert_eq!(
        found.status.code(),
        Some(0),
        "nothing matched, which is an answer and not a failure — a script that reads the status \
         must not see a refusal here. stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.trim().is_empty(),
        "and the answer is empty rather than the whole listing or an invented line:\n{stdout}"
    );
    assert!(
        stderr.trim().is_empty(),
        "with nothing on the error stream, because nothing went wrong:\n{stderr}"
    );
}

/// A search with no text asked for nothing, which is a usage error rather than
/// a whole-listing answer.
#[test]
fn a_search_with_no_text_is_a_usage_error() {
    let tmp = deployment();
    let data_dir = tmp.path().join("data");

    let found = settings(&data_dir, &["find"]);
    let stdout = stdout_of(&found);
    let stderr = stderr_of(&found);

    assert_eq!(
        found.status.code(),
        Some(REFUSED),
        "`vigil settings find` names nothing to search for. stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("find"),
        "and the usage it prints names the verb the operator was using:\n{stderr}"
    );
}

/// The removed verb is gone, and an operator who types it is pointed at the one
/// that replaced it rather than being answered as though it still worked.
#[test]
fn the_removed_domain_verb_is_refused_and_names_find() {
    let tmp = deployment();
    let data_dir = tmp.path().join("data");

    let found = settings(&data_dir, &["domain", "detection"]);
    let stdout = stdout_of(&found);
    let stderr = stderr_of(&found);

    assert_eq!(
        found.status.code(),
        Some(REFUSED),
        "`settings domain` no longer exists, so it must not answer as though it did. stdout:\n\
         {stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains(SETTING_LINE_PREFIX),
        "and it renders no listing of any width:\n{stdout}"
    );
    assert!(
        stderr.contains("find"),
        "and the refusal names the verb that replaced it, or the operator is left guessing what \
         to type:\n{stderr}"
    );
}

/// Help offers the verb that exists. A search an operator cannot discover is a
/// search they do not have, and a name help still advertises is one they will
/// type.
#[test]
fn help_offers_find_and_no_longer_offers_domain() {
    let help = Command::new(vigil_binary_path())
        .arg("--help")
        .stdin(Stdio::null())
        .output()
        .expect("spawn `vigil --help`");
    let printed = stdout_of(&help);

    let settings_line = printed
        .lines()
        .find(|line| line.trim_start().starts_with("settings"))
        .unwrap_or_else(|| panic!("help must offer the settings command:\n{printed}"));
    assert!(
        settings_line.contains("find"),
        "help offers the search verb operators are meant to use: {settings_line}"
    );
    assert!(
        !settings_line.contains("domain"),
        "and no longer offers the name that was removed: {settings_line}"
    );
}
