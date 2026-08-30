//! Two read-only commands asked at the same time both answer, and neither
//! changes the store.
//!
//! ContextDB's concurrent direct-reader contract is settled: genuine read
//! sessions coexist, and the busy answer this product documents is the one a
//! WRITER gets while readers are hydrating — never a read meeting another read.
//! Vigil's read-only fallbacks do not take that road. They open Context Graph's
//! writable `Store`, so two questions asked at once contend as writers: one of
//! them is told `database is locked by another process (holder pid N); stop
//! that runtime before starting a second one against this data directory`,
//! naming a peer READ command as the runtime to stop, or is refused with `the
//! process holding the store ... does not answer owner requests`. Both leave on
//! standard error with status 2, against a healthy store, for a condition that
//! clears by itself — and the operator is sent to stop a runtime that does not
//! exist.
//!
//! This is CROSS-PROCESS on purpose: inter-process store ownership is the whole
//! mechanism, and nothing about it can be exercised inside one process, where
//! Context Graph shares a single handle per path. Every child is spawned before
//! any child is waited on, so they genuinely overlap; the test synchronizes on
//! process exit and on the store's own bytes, never on a clock, and asserts no
//! timing of any kind.
//!
//! The store is left exactly as it was, which is the second half of the
//! promise: a question is not a change, however many are asked at once.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{open_store_at, vigil_binary_path};

/// How many questions are asked at the same time. Two is the number the
/// contract is stated for — two operators, or one operator and one script.
const AT_ONCE: usize = 2;

/// How many times the pair is asked. A contended open is a race, so one round
/// proves nothing about the rounds that would have lost; a read path that
/// genuinely coexists wins all of them.
const ROUNDS: usize = 12;

/// What a peer read command must never be told. Each names a condition that is
/// not happening: no runtime is running, and the store is healthy.
const CONTENTION_CLAIMS: [&str; 4] = [
    "database is locked",
    "does not answer owner requests",
    "stop that runtime",
    "is already serving as many requests as it admits",
];

/// A deployment whose store really exists and which nothing is holding: the
/// state an operator's node is in between runs, and the state two review
/// commands meet when they are typed together.
struct IdleDeployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    store_path: PathBuf,
}

impl IdleDeployment {
    fn prepare() -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        let store_path = data_dir.join("store.contextgraph");
        // A real store, created and closed before anything else runs, so every
        // command below meets a store that exists and that nobody holds.
        drop(open_store_at(&store_path).expect("seed this deployment's store"));
        fs::write(tmp.path().join("vigil.toml"), "cameras = []\n")
            .expect("write the configuration file");
        Self {
            _tmp: tmp,
            data_dir,
            store_path,
        }
    }

    fn command(&self, request: &[&str]) -> Command {
        let mut command = Command::new(vigil_binary_path());
        command
            .args(request)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Ask the same question `AT_ONCE` times over, with every child spawned
    /// before any of them is waited on, so the questions genuinely overlap.
    fn ask_together(&self, request: &[&str]) -> Vec<Output> {
        let children: Vec<_> = (0..AT_ONCE)
            .map(|_| {
                self.command(request)
                    .spawn()
                    .unwrap_or_else(|error| panic!("spawn `vigil {}`: {error}", request.join(" ")))
            })
            .collect();
        children
            .into_iter()
            .map(|child| child.wait_with_output().expect("wait for the answer"))
            .collect()
    }

    /// The store exactly as it stands, so a read that wrote is caught.
    fn fingerprint(&self) -> (std::time::SystemTime, Vec<u8>) {
        let modified = fs::metadata(&self.store_path)
            .and_then(|meta| meta.modified())
            .expect("the store's modification time");
        (
            modified,
            fs::read(&self.store_path).expect("the store's bytes"),
        )
    }
}

fn text_of(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Whether this answer is one of the contention failures a read must never
/// produce against a healthy store nobody is holding.
fn contention_failure(answer: &str) -> Option<&'static str> {
    CONTENTION_CLAIMS
        .into_iter()
        .find(|claim| answer.contains(claim))
}

fn assert_no_contention(label: &str, answers: &[Output], round: usize) -> Vec<String> {
    let mut failures = Vec::new();
    for answer in answers {
        let text = text_of(answer);
        if let Some(claim) = contention_failure(&text) {
            failures.push(format!(
                "{label}, round {round}: a question asked beside another question was answered \
                 `{claim}`. Nothing is holding this store, no runtime is running, and the process \
                 the answer points at is a peer READ command — so the operator is sent to stop a \
                 runtime that does not exist, over a condition that clears by itself. The answer \
                 was:\n{text}"
            ));
        }
    }
    failures
}

/// The proof the whole read-only path exists for: two `vigil settings` reads
/// asked at the same time both answer, and the store is untouched.
///
/// Unfakeable because the two answers come from two separate operating-system
/// processes contending for one store file — the only way inter-process store
/// ownership can be exercised at all — and the store's own bytes are compared
/// before and after.
#[test]
fn two_concurrent_settings_reads_both_answer_and_change_nothing() {
    let deployment = IdleDeployment::prepare();
    let before = deployment.fingerprint();
    let mut failures: Vec<String> = Vec::new();

    for round in 1..=ROUNDS {
        let answers = deployment.ask_together(&["settings"]);
        failures.extend(assert_no_contention("vigil settings", &answers, round));
        for answer in &answers {
            if answer.status.code() != Some(0) {
                failures.push(format!(
                    "vigil settings, round {round}: a read-only listing of a healthy store nobody \
                     is holding exited {:?}. The answer was:\n{}",
                    answer.status.code(),
                    text_of(answer)
                ));
            }
        }
    }

    assert_eq!(
        deployment.fingerprint(),
        before,
        "asking is not changing: {ROUNDS} rounds of {AT_ONCE} concurrent read-only listings must \
         leave the store byte-identical"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The same defect on the review commands, which take the same writable open.
/// `why --latest` against a store with no events refuses — that is its own
/// honest answer and it is not what this pins; what it must never do is refuse
/// because a peer question was being asked at the same moment.
#[test]
fn concurrent_review_reads_are_never_answered_as_a_locked_store() {
    let deployment = IdleDeployment::prepare();
    let before = deployment.fingerprint();
    let mut failures: Vec<String> = Vec::new();

    for round in 1..=ROUNDS {
        for request in [&["events"][..], &["why", "--latest"][..]] {
            let answers = deployment.ask_together(request);
            failures.extend(assert_no_contention(
                &format!("vigil {}", request.join(" ")),
                &answers,
                round,
            ));
        }
    }

    assert_eq!(
        deployment.fingerprint(),
        before,
        "and neither review command writes to the store, however many are asked at once"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The control beside both proofs: asked one after another, every one of these
/// commands answers cleanly. Without it, a read path that never answered at all
/// would satisfy the two assertions above.
#[test]
fn the_same_reads_asked_one_after_another_all_answer() {
    let deployment = IdleDeployment::prepare();

    for request in [&["settings"][..], &["events"][..]] {
        for round in 1..=AT_ONCE {
            let answer = deployment
                .command(request)
                .output()
                .expect("spawn a sequential read");
            assert_eq!(
                answer.status.code(),
                Some(0),
                "`vigil {}` asked on its own, round {round}, must answer: {}",
                request.join(" "),
                text_of(&answer)
            );
        }
    }
}

/// The store path this deployment seeded really is the one the commands read,
/// so a green result above cannot come from commands answering about a
/// different file.
#[test]
fn the_commands_read_the_store_this_deployment_seeded() {
    let deployment = IdleDeployment::prepare();
    assert!(
        Path::new(&deployment.store_path).exists(),
        "fixture: the seeded store must exist at {}",
        deployment.store_path.display()
    );
    let answer = deployment.command(&["settings"]).output().expect("spawn");
    assert!(
        answer.status.success(),
        "the seeded deployment answers a settings read: {}",
        text_of(&answer)
    );
}
