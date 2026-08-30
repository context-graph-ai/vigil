//! A live command has to arrive at the SAME file the daemon opened, or it is
//! answering about somebody else's store.
//!
//! `vigil why`, `vigil events` and `vigil stats` reach the running daemon
//! through a channel derived from the store's own resolved pathname, so the
//! store location is not a detail of the command — it IS the address. Resolve
//! a different pathname and the command looks for an owner that was never
//! there, finds nobody holding it, and answers from whatever file it landed on
//! instead. The operator is told about a deployment that is not theirs, on a
//! machine whose real daemon is up and serving.
//!
//! The two sides diverge today, and the add-on is exactly where. The daemon
//! resolves its store through `config::load`, which reads
//! `/data/options.json` — the add-on's own options file, where `store_path` is
//! the one location key the package ships (`addons/vigil/config.yaml`), stated
//! because a store cannot say where itself is. The command side
//! (`lib.rs`'s `store_path_from_env`) never looks at that file at all: it
//! reads the environment, and failing that joins a default filename onto
//! whatever data directory it worked out. In the add-on those are two
//! different answers whenever the operator sets `store_path` to anything but
//! the shipped value — and it is a settable, schema-declared key, so an
//! operator who moves their store onto a mounted disk loses every live command
//! at once.
//!
//! What both sides owe the operator is one LOCATION-ONLY resolution: the
//! surfaces that say where things are, read the same way in the same order,
//! without loading or validating anything else. A live command has no command
//! line to carry a store path and no business validating a camera list, but it
//! must land on the daemon's file.
//!
//! Precedence pinned below is the deployment's own, unchanged: the environment
//! over the add-on's options, and a store pathname stated anywhere over any
//! filename joined onto a directory. The packaged `/data/store.contextgraph`
//! is not a constant hidden in the binary — it is what a packaged deployment
//! resolves to, which is why the last leg proves it falls out of the data
//! directory rather than being written down twice.
//!
//! Nothing here sleeps, reads a clock, spawns a process, or reads the ambient
//! environment: each leg hands the resolver the surfaces it is being asked
//! about, so no two of them can interfere.

use std::fs;
use std::path::{Path, PathBuf};

use vigil::{StoreLocation, StoreLocationEnvironment, resolve_store_location};

/// The store pathname the add-on ships in its own options.
const PACKAGED_STORE: &str = "/data/store.contextgraph";

/// The deployment directory a packaged Vigil runs against.
const PACKAGED_DATA_DIR: &str = "/data";

/// An operator who moved their store onto a mounted disk — a schema-declared,
/// settable key, so this is an ordinary deployment rather than an exotic one.
const RELOCATED_STORE: &str = "/mnt/nvr/vigil/store.contextgraph";

/// An add-on options file with the given body, written where a fixture can
/// hand it to the resolver.
fn options_json(inside: &Path, body: &str) -> PathBuf {
    let path = inside.join("options.json");
    fs::write(&path, body).expect("write this deployment's add-on options");
    path
}

/// Nothing stated in the environment: the add-on case, where the Supervisor
/// passes options in a file rather than as variables.
fn nothing_in_the_environment() -> StoreLocationEnvironment {
    StoreLocationEnvironment {
        data_dir: None,
        store_path: None,
    }
}

#[test]
fn a_live_command_reads_the_store_path_the_add_on_states() {
    let tmp = tempfile::tempdir().expect("fixture directory");
    let options = options_json(
        tmp.path(),
        &format!(
            "{{\"store_path\": \"{RELOCATED_STORE}\", \"data_dir\": \"{PACKAGED_DATA_DIR}\"}}"
        ),
    );

    let resolved: StoreLocation =
        resolve_store_location(&nothing_in_the_environment(), Some(&options))
            .expect("resolve this deployment's store location");

    assert_eq!(
        resolved.store_path,
        PathBuf::from(RELOCATED_STORE),
        "the add-on's own options are where a packaged deployment states its store, and the \
         daemon opens the file they name — a command that joined a default filename onto the \
         data directory instead would look for an owner that was never there and answer about a \
         store nobody is holding, on a machine whose daemon is up and serving"
    );
}

#[test]
fn a_stated_store_path_is_never_rebuilt_from_the_data_directory() {
    let tmp = tempfile::tempdir().expect("fixture directory");
    let options = options_json(
        tmp.path(),
        &format!(
            "{{\"store_path\": \"{RELOCATED_STORE}\", \"data_dir\": \"{PACKAGED_DATA_DIR}\"}}"
        ),
    );

    // The environment says where the deployment DIRECTORY is and says nothing
    // about the store. A resolution that let the directory win would move the
    // store every time an operator named a data directory.
    let environment = StoreLocationEnvironment {
        data_dir: Some(PathBuf::from(PACKAGED_DATA_DIR)),
        store_path: None,
    };
    let resolved = resolve_store_location(&environment, Some(&options))
        .expect("resolve this deployment's store location");

    assert_eq!(
        resolved.store_path,
        PathBuf::from(RELOCATED_STORE),
        "a store pathname stated anywhere outranks any filename joined onto a directory: the \
         store and the deployment directory are two separate answers, and a deployment that \
         keeps its store on a mounted disk is an ordinary one"
    );
}

#[test]
fn the_environment_still_outranks_the_add_ons_options() {
    let tmp = tempfile::tempdir().expect("fixture directory");
    let options = options_json(
        tmp.path(),
        &format!("{{\"store_path\": \"{PACKAGED_STORE}\"}}"),
    );
    let environment = StoreLocationEnvironment {
        data_dir: None,
        store_path: Some(PathBuf::from(RELOCATED_STORE)),
    };

    let resolved = resolve_store_location(&environment, Some(&options))
        .expect("resolve this deployment's store location");

    assert_eq!(
        resolved.store_path,
        PathBuf::from(RELOCATED_STORE),
        "the deployment's precedence is unchanged by this resolution being shared — the \
         environment over the add-on's options — because the daemon resolves it that way and the \
         command has to land on the daemon's file"
    );
}

#[test]
fn a_packaged_deployment_that_states_no_store_falls_back_to_its_own_data_directory() {
    let tmp = tempfile::tempdir().expect("fixture directory");
    let options = options_json(
        tmp.path(),
        &format!("{{\"data_dir\": \"{PACKAGED_DATA_DIR}\"}}"),
    );

    let resolved = resolve_store_location(&nothing_in_the_environment(), Some(&options))
        .expect("resolve this deployment's store location");

    assert_eq!(
        resolved.data_dir,
        PathBuf::from(PACKAGED_DATA_DIR),
        "the deployment directory is the one the packaged deployment states"
    );
    assert_eq!(
        resolved.store_path,
        PathBuf::from(PACKAGED_STORE),
        "with no store stated anywhere, the store sits in this deployment's own directory — so \
         the packaged pathname the add-on ships falls OUT of the deployment rather than being \
         written down a second time inside the binary, where it could drift from the file the \
         daemon opens"
    );
}

#[test]
fn a_deployment_with_no_add_on_options_resolves_from_its_environment_alone() {
    // The unpackaged install, which is every ordinary machine: no options file
    // exists, and the resolution is exactly what it has always been.
    let tmp = tempfile::tempdir().expect("fixture directory");
    let data_dir = tmp.path().join("vigil-data");
    let environment = StoreLocationEnvironment {
        data_dir: Some(data_dir.clone()),
        store_path: None,
    };

    let resolved = resolve_store_location(&environment, None)
        .expect("resolve this deployment's store location");

    assert_eq!(resolved.data_dir, data_dir);
    assert_eq!(
        resolved.store_path,
        data_dir.join("store.contextgraph"),
        "an install with no add-on options behind it keeps exactly the resolution it has today: \
         sharing one location-only resolution with the daemon is an add-on fix, never a new \
         requirement for the deployments that never had an options file"
    );
}
