//! A command that could not open the store classifies THE ERROR IT GOT — it
//! does not go back and ask the store again.
//!
//! The read commands hold a typed failure the moment their own open is
//! refused: context-graph says reader-held, writer-held, or something else, and
//! says who and where. That value is what the answer must be built from. It was
//! not: until this guard went red, the failure was flattened to a string and
//! thrown away, and the answer came from a SECOND open performed a moment later
//! (`classify_store_open_at`). Two things followed, and both were the
//! operator's problem rather than a tidiness question:
//!
//!  * The second open is a WRITABLE node handle — the same door the runtime
//!    itself comes through. A command asked to READ contends for the store it
//!    is diagnosing, on a deployment whose store is already busy enough that
//!    the first open was refused.
//!  * The two opens can disagree, because they happen at different moments. If
//!    the holder lets go in between, the second open succeeds and the command
//!    answers about a store that is no longer the one that refused it: the
//!    review commands fall through to the generic capability-unavailable arm,
//!    and the settings surface prints the FIRST error it had already decided to
//!    discard. Either way the operator is told about a world that was not the
//!    one their command met.
//!
//! That window cannot be timed from a test — which is the point, and the
//! reason this guard reads the source rather than trying. A command that
//! classifies the failure it is holding has no second observation to disagree
//! with, so the race stops existing rather than becoming rare. That is why what
//! is pinned here is the SHAPE and not a timing: keep the shape and the race
//! cannot come back. The sibling
//! behavioral pins in `crates/vigil-bin/tests/
//! a_store_busy_with_readers_is_not_an_unreadable_store.rs` hold the other half:
//! the answer's content, and that a refused command touches this deployment's
//! storage not at all.
//!
//! Two scopes, stated exactly, because a guard whose comment misdescribes it is
//! the same defect it exists to catch. On the review path only the
//! answer-building function is read: the startup path is a different caller
//! with a different job — it is deciding whether to create and take the store,
//! so opening is the whole point there — and nothing here constrains it. On the
//! settings surface the WHOLE file is read, which is broader than the read path
//! and deliberately so: a settings CHANGE legitimately opens for writing, and
//! requiring every one of those to come through a named change door means a
//! future writable open cannot appear next to a read answer and look like it
//! belongs. The broad form is the stronger end-state, not an accident of
//! grepping.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn source(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The body of one function, from its signature to the line that closes it at
/// the same indentation.
fn function_body(text: &str, signature: &str) -> Option<String> {
    let start = text.find(signature)?;
    let rest = &text[start..];
    let indent: String = rest
        .lines()
        .next()?
        .chars()
        .take_while(|character| character.is_whitespace())
        .collect();
    let closing = format!("\n{indent}}}");
    let end = rest.find(&closing)? + closing.len();
    Some(rest[..end].to_string())
}

/// The re-opening classifier: it takes a PATH, so every caller of it is asking
/// the store again rather than reading what it was already told.
const RE_OPENS_THE_STORE: &str = "classify_store_open_at(";

/// The door that second open goes through — a writable node handle over the
/// deployment, which is what makes the re-open contend rather than merely
/// duplicate.
const WRITABLE_HANDLE: &str = "SettingsStore::open_at(";

/// The review commands. `vigil why` and `vigil events` meet a busy store more
/// often than anything else does, because an operator reaches for them exactly
/// when something is going on.
#[test]
fn the_review_command_answer_is_built_from_the_failure_not_from_a_second_open() {
    let library = source("crates/vigil/src/lib.rs");
    let refusal = function_body(&library, "fn degraded_refusal_for(")
        .expect("the refusal a read command gets must still be built somewhere in the library");

    assert!(
        !refusal.contains(RE_OPENS_THE_STORE),
        "the answer for a command whose open was refused is built by opening the store AGAIN. The \
         command already holds the typed failure — who is holding the store, and where it is — and \
         that is what the operator has to be told about. A second look can disagree with the \
         first: if the holder lets go in between it succeeds, and the operator is told their \
         review history is unavailable on a deployment that is perfectly fine. Classify the error \
         in hand. The body read:\n{refusal}"
    );
    assert!(
        !refusal.contains(WRITABLE_HANDLE),
        "and that second look is a WRITABLE handle on the store — a read command contending for \
         the store it was refused by, which is the one thing a diagnosis must never do to the \
         thing it is diagnosing. The body read:\n{refusal}"
    );
}

/// The settings surfaces. Same defect, second call site: the failure is
/// stringified first and the class comes from a fresh open, so a holder that
/// lets go in between leaves the stale string to be printed.
#[test]
fn the_settings_answer_is_built_from_the_failure_not_from_a_second_open() {
    let command = source("crates/vigil/src/settings_command.rs");

    assert!(
        !command.contains(RE_OPENS_THE_STORE),
        "the settings answer classifies by opening the store a second time. When the two opens \
         disagree the operator is handed the FIRST error as raw text — the very value this path \
         decided was not good enough to answer with — rather than an answer about the condition \
         their command actually met. The failure is already in hand when the first open returns; \
         classify that."
    );
    assert!(
        !command.contains(WRITABLE_HANDLE),
        "a writable node handle is opened directly on the settings surface. A settings CHANGE may \
         of course open for writing — through the named change door, where it is visible as a \
         write — but a bare handle here is indistinguishable from the re-open a read answer must \
         never do, and a read-only question that contends for the store it was refused by is \
         exactly what this guard exists to keep out."
    );
}

/// The guard's own proof that it can see what it claims to see: the exact
/// shapes it forbids, in a sample built for the purpose. Without this, a
/// renamed classifier would make both tests above pass by seeing nothing at
/// all, which reads identically to a fixed source.
#[test]
fn the_guard_recognizes_a_re_opening_classifier_and_leaves_an_error_classifier_alone() {
    let re_opening = r#"
        fn refusal_for(command: &str, store_path: &Path) -> Option<String> {
            match settings_degraded::classify_store_open_at(store_path) {
                Ok(class) => Some(render(class)),
                Err(_) => None,
            }
        }
    "#;
    assert!(
        re_opening.contains(RE_OPENS_THE_STORE),
        "positive control: the guard must recognize a path that classifies by re-opening, or the \
         assertions above prove nothing"
    );

    let from_the_error = r#"
        fn refusal_for(command: &str, failure: &StoreOpenFailure) -> Option<String> {
            match settings_degraded::classify_failure(failure) {
                StoreOpenFailure::HeldByReaders { .. } => Some(render_busy(failure)),
                _ => None,
            }
        }
    "#;
    assert!(
        !from_the_error.contains(RE_OPENS_THE_STORE) && !from_the_error.contains(WRITABLE_HANDLE),
        "negative control: a path that classifies the failure it was handed must satisfy this \
         guard, whatever the classifier and its type end up being called — the shape is what is \
         pinned here, never a chosen name"
    );
}

/// The classifier itself, driven directly: given a failure, it says which
/// condition that failure IS — and it does so without touching a store.
///
/// This is the other half of the guard above. That one proves no second open
/// happens; this one proves the value the answer is built from carries what the
/// operator needs, straight out of the error the first open returned. It runs
/// against a path that does not exist, which is the purity proof: a classifier
/// that consulted the filesystem could not name a store that was never there.
#[test]
fn a_failure_is_classified_into_the_condition_it_reports_without_touching_a_store() {
    let never_created = Path::new("/nonexistent/deployment/store.contextgraph");
    let readers = vec![context_graph::ReaderIdentity {
        process_id: 4_294_967_297,
        process_name: "vigil-stats".to_string(),
        process_start: 12_345,
    }];

    let held = vigil::settings_degraded::classify_open_failure(
        &vigil::settings_model::SettingsError::StoreHeldByReaders {
            observed_direct_readers: 3,
            readers: readers.clone(),
            path: never_created.to_path_buf(),
        },
    );
    match held {
        Some(vigil::settings_degraded::StoreOpenClass::HeldByReaders {
            observed_direct_readers,
            readers: carried,
            path,
        }) => {
            assert_eq!(
                (observed_direct_readers, carried.len(), path.as_path()),
                (3, 1, never_created),
                "the reader-held condition must arrive with the count, the identified readers and \
                 the store the failure named — all three come from the error, and a store that \
                 was never created proves none of them was fetched by looking"
            );
            assert_eq!(
                carried[0].process_id, 4_294_967_297,
                "a process id wider than 32 bits reaches the operator whole, since that is the \
                 width the layers below report it in"
            );
        }
        other => panic!("a reader-held failure is the reader-held condition; got {other:?}"),
    }

    let locked = vigil::settings_degraded::classify_open_failure(
        &vigil::settings_model::SettingsError::LockedByAnotherRuntime {
            holder_pid: Some(4_242),
        },
    );
    assert!(
        matches!(
            locked,
            Some(
                vigil::settings_degraded::StoreOpenClass::LockedByAnotherRuntime {
                    holder_pid: Some(4_242)
                }
            )
        ),
        "a writer holding the store is its own condition with its own answer, and the holder \
         travels with it; got {locked:?}"
    );
}

/// A store that OPENED and then decided something is not a store that could not
/// be read, and the classifier says so by answering about nothing at all.
///
/// This is what the optional answer is for. A refused write and a scope-label
/// violation both arrive as the same error type as an open failure, and both
/// happen on the far side of a successful open. Classifying either as
/// unreadable would tell an operator their storage is broken because a value
/// they set was governed — the store is fine, and the thing that refused them
/// is a rule, not a disk.
#[test]
fn a_decision_taken_after_the_store_opened_is_never_classified_as_an_unreadable_store() {
    let governed = vigil::settings_degraded::classify_open_failure(
        &vigil::settings_model::SettingsError::ScopeLabelViolation {
            requested: "pushed".to_string(),
            allowed: "node".to_string(),
        },
    );
    assert!(
        governed.is_none(),
        "the store opened and then refused the rank this handle may write at. Reported as an open \
         failure it becomes `the store cannot be read`, and an operator goes looking for a storage \
         fault that does not exist; got {governed:?}"
    );
}
