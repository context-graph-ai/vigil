//! Secrets are settable everywhere, with one deterministic precedence: an
//! environment variable wins over a stored value, because the environment leg
//! is the per-process injection a secret manager or a rotation just handed this
//! run, and letting a stale stored value shadow it would silently strand a
//! rotated credential.
//!
//! The environment leg is an override, not an author: it writes no record, it
//! syncs nowhere, and when the variable disappears the stored value is
//! effective again with nothing to undo. And none of this makes a secret
//! visible — the operator surface names the SOURCE, never the value.
//!
//! Environment mutation is serialized within this test binary by `ENV_LOCK`,
//! held for the whole of each test, with every variable restored by a guard.
//! Each integration test file is its own process, so no other test binary
//! shares this environment.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tempfile::TempDir;

use vigil::settings_environment::{
    SecretSource, resolve_secret, secret_environment_variable, secret_source_lines,
};
use vigil::settings_model::{Scope, SettingValue, Surface};
use vigil::settings_projection::{SECRET_LINE_PREFIX, report_by_direct_read};
use vigil::settings_store::SettingsStore;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The camera password: the worked example the secrets ruling itself uses.
const SECRET: &str = "rtsp_password";

/// The variable that backs this secret, taken from the product's own roster
/// rather than spelled out here: a test that hardcoded the spelling would keep
/// passing while the runtime read a different name.
fn secret_variable() -> &'static str {
    secret_environment_variable(SECRET).unwrap_or_else(|| {
        panic!(
            "the camera password is one of the secrets the environment still carries, so it must \
             have a variable on the roster"
        )
    })
}

/// Distinctive plaintexts, so "the surface never prints the value" is a real
/// check against real content rather than a check against a placeholder that
/// could not have appeared anyway.
const ENVIRONMENT_PLAINTEXT: &str = "env-plaintext-Zq7Kx-never-render-me";
const STORED_PLAINTEXT: &str = "stored-plaintext-Xr3Vb-never-render-me";

fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvGuard {
    name: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    // Reading the variable is what makes restore-on-drop possible; the guard
    // exists to drive the env surface under test (allow: test env fixture).
    #[allow(clippy::disallowed_methods)]
    fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var(name).ok();
        // SAFETY: serialized by ENV_LOCK, which the caller holds for the whole
        // test; this binary mutates the environment nowhere else.
        unsafe { std::env::set_var(name, value) };
        Self { name, previous }
    }

    #[allow(clippy::disallowed_methods)] // as above: the guard's own read
    fn clear(name: &'static str) -> Self {
        let previous = std::env::var(name).ok();
        // SAFETY: as above.
        unsafe { std::env::remove_var(name) };
        Self { name, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: as above — the test still holds ENV_LOCK while it drops.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

/// A deployment whose config file already carries the camera password, which is
/// the case the precedence rule exists to settle. The handle is opened against
/// the DEPLOYMENT DIRECTORY; where the store file sits inside it is the
/// product's decision, asked for by name below rather than guessed at here.
fn deployment_with_a_stored_secret() -> TempDir {
    let dir = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open(dir.path()).expect("node-side settings store opens");
    store
        .set_local(
            SECRET,
            Surface::ConfigFile,
            Scope::node("node-a"),
            SettingValue::text(STORED_PLAINTEXT),
        )
        .expect("a camera password is settable through the ordinary settings surfaces");
    dir
}

/// The store file inside a deployment directory, as the product places it.
fn store_file(data_dir: &Path) -> PathBuf {
    SettingsStore::store_path(data_dir)
}

fn stored_records(data_dir: &Path) -> Vec<vigil::settings_model::SettingRecord> {
    let store = SettingsStore::open(data_dir).expect("node-side settings store opens");
    store
        .records(SECRET)
        .expect("the stored records for one setting are readable")
}

#[test]
fn an_environment_secret_is_used_over_a_stored_one() {
    // Unfakeable: the two plaintexts are distinct, so the assertion is over
    // WHICH value came back, not over a flag the implementation sets alongside
    // it. The source line and the exposed value are both checked, so an
    // implementation that labels the source correctly while returning the
    // stored secret fails.
    let _lock = env_lock();
    let dir = deployment_with_a_stored_secret();
    let store_path = store_file(dir.path());
    let _injected = EnvGuard::set(secret_variable(), ENVIRONMENT_PLAINTEXT);

    let (secret, line) =
        resolve_secret(&store_path, SECRET).expect("the camera password resolves from some source");

    assert_eq!(
        secret.expose_secret(),
        ENVIRONMENT_PLAINTEXT,
        "an environment variable, when present, wins: a rotated credential must never be shadowed \
         by a stale stored one"
    );
    assert_eq!(
        line.effective,
        SecretSource::Environment,
        "the source line names the environment as the effective source"
    );
    assert!(
        line.stored_value_exists,
        "a secret set in two places is something the operator should be able to see and clean up, \
         so the stored value underneath is reported as existing"
    );
    assert_eq!(
        line.secret, SECRET,
        "the source line names which secret it is about"
    );
}

#[test]
fn the_operator_surface_names_the_secret_source_and_never_prints_the_value() {
    // Unfakeable: both plaintexts are planted, distinctive strings, and the
    // rendered line is searched for each of them. A surface that "redacts" by
    // wording alone, or that prints the stored value while naming the
    // environment as the source, fails on content rather than on phrasing.
    let _lock = env_lock();
    let dir = deployment_with_a_stored_secret();
    let store_path = store_file(dir.path());
    let _injected = EnvGuard::set(secret_variable(), ENVIRONMENT_PLAINTEXT);

    let (_secret, line) =
        resolve_secret(&store_path, SECRET).expect("the camera password resolves");

    let statement = line.statement.clone();
    assert!(
        statement.to_lowercase().contains("environment"),
        "the operator surface shows the SOURCE — that this machine is taking the secret from the \
         environment — so the source must be named: {statement:?}"
    );
    assert!(
        !statement.contains(ENVIRONMENT_PLAINTEXT),
        "the operator surface must never print the secret value; the rendered line leaked the \
         environment plaintext"
    );
    assert!(
        !statement.contains(STORED_PLAINTEXT),
        "the operator surface must never print the secret value; the rendered line leaked the \
         stored plaintext"
    );

    let lines = secret_source_lines(&store_path).expect("every secret has a source line");
    let about_this_secret = lines
        .iter()
        .find(|candidate| candidate.secret == SECRET)
        .unwrap_or_else(|| panic!("the camera password must appear in the secret source lines"));
    assert_eq!(
        about_this_secret.effective,
        SecretSource::Environment,
        "the whole-deployment listing agrees with the single resolution about the source"
    );
    for rendered in &lines {
        assert!(
            !rendered.statement.contains(ENVIRONMENT_PLAINTEXT)
                && !rendered.statement.contains(STORED_PLAINTEXT),
            "no secret source line may carry a secret value: {:?}",
            rendered.statement
        );
    }

    // The typed struct is not what an operator reads. Criterion 11 is about the
    // RENDERED surface, so the same two facts are proven again on the output of
    // the one projection every operator surface goes through: the secret's
    // source is on it, and neither plaintext appears anywhere in it. A build
    // that carried the source line in the report but dropped it at rendering,
    // or that echoed the value into some other line, passes everything above
    // and fails here.
    let rendered_lines = report_by_direct_read(dir.path())
        .expect("the operator surface answers by direct read")
        .render_lines();
    let secret_line = rendered_lines
        .iter()
        .filter(|line| line.starts_with(&format!("{SECRET_LINE_PREFIX} ")))
        .find(|line| line.contains(SECRET))
        .unwrap_or_else(|| {
            panic!(
                "the rendered operator surface must carry a `{SECRET_LINE_PREFIX}` line naming \
                 `{SECRET}`; got:\n{}",
                rendered_lines.join("\n")
            )
        });
    assert!(
        secret_line.to_lowercase().contains("environment"),
        "the rendered secret line names the SOURCE this machine is taking the secret from; \
         got: {secret_line}"
    );
    for line in &rendered_lines {
        assert!(
            !line.contains(ENVIRONMENT_PLAINTEXT) && !line.contains(STORED_PLAINTEXT),
            "no line of the rendered operator surface may carry a secret value — the surface \
             names the source, never the value; got: {line}"
        );
    }
}

#[test]
fn removing_the_environment_variable_restores_the_stored_secret_with_nothing_to_undo() {
    // Unfakeable: the same store path is resolved twice, once with the variable
    // and once without, and the stored records are compared across both. There
    // is nothing to undo precisely because nothing was written — which is
    // asserted as record equality, not as an absence of complaint.
    let _lock = env_lock();
    let dir = deployment_with_a_stored_secret();
    let store_path = store_file(dir.path());
    let records_before = stored_records(dir.path());

    {
        let _injected = EnvGuard::set(secret_variable(), ENVIRONMENT_PLAINTEXT);
        let (secret, line) =
            resolve_secret(&store_path, SECRET).expect("the camera password resolves");
        assert_eq!(secret.expose_secret(), ENVIRONMENT_PLAINTEXT);
        assert_eq!(line.effective, SecretSource::Environment);
    }

    let _removed = EnvGuard::clear(secret_variable());
    let (secret, line) = resolve_secret(&store_path, SECRET)
        .expect("the stored value is effective again once the variable is gone");
    assert_eq!(
        secret.expose_secret(),
        STORED_PLAINTEXT,
        "when the environment variable disappears the stored value is effective again"
    );
    assert_eq!(
        line.effective,
        SecretSource::Stored,
        "the source line follows the value back to the store"
    );

    let records_after = stored_records(dir.path());
    assert_eq!(
        records_after, records_before,
        "there is nothing to un-do because the environment leg never authored anything: the \
         stored records are identical before the override and after it went away"
    );
}

#[test]
fn the_environment_secret_leg_authors_no_record_and_syncs_nowhere() {
    // Unfakeable: the records are compared as whole values across the use of
    // the override, and every stored record is additionally searched for the
    // environment plaintext. An implementation that wrote the injected secret
    // under some other author, surface, or table is caught by the second check
    // even if it kept the record count steady.
    let _lock = env_lock();
    let dir = deployment_with_a_stored_secret();
    let store_path = store_file(dir.path());
    let records_before = stored_records(dir.path());

    let _injected = EnvGuard::set(secret_variable(), ENVIRONMENT_PLAINTEXT);
    let (secret, line) = resolve_secret(&store_path, SECRET).expect("the camera password resolves");
    assert_eq!(secret.expose_secret(), ENVIRONMENT_PLAINTEXT);
    assert_eq!(line.effective, SecretSource::Environment);

    let records_after = stored_records(dir.path());
    assert_eq!(
        records_after, records_before,
        "the environment leg is an override, not an author: using it writes no record, so the \
         stored records are unchanged"
    );
    for record in &records_after {
        assert_ne!(
            record.value,
            SettingValue::text(ENVIRONMENT_PLAINTEXT),
            "a record carrying the injected secret would mean the environment leg authored — and \
             therefore would sync — something it must never author: {record:?}"
        );
    }
}
