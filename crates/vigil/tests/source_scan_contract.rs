use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn live_read_and_owner_control_source_contracts_are_fast() {
    let vigil_sources = collect_rust_source_files(&workspace_root().join("crates/vigil/src"));
    let context_graph_sources = collect_rust_source_files(
        &workspace_root()
            .parent()
            .expect("workspace parent")
            .join("context-graph/crates/context-graph/src"),
    );

    let mut failures = Vec::new();
    assert_context_graph_owns_generic_owner_control_transport(
        &context_graph_sources,
        &mut failures,
    );
    assert_vigil_keeps_only_control_path_and_handler_layer(&vigil_sources, &mut failures);
    assert_live_reads_stay_store_backed(&vigil_sources, &context_graph_sources, &mut failures);

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

fn assert_context_graph_owns_generic_owner_control_transport(
    sources: &[SourceFile],
    failures: &mut Vec<String>,
) {
    let transport = sources
        .iter()
        .find(|source| source.path.ends_with("control_transport.rs"));
    let Some(transport) = transport else {
        failures.push("context-graph is missing src/control_transport.rs".to_string());
        return;
    };

    for required in [
        "pub type ControlHandler",
        "pub fn start_control_listener",
        "pub fn request_control",
        "UnixListener",
        "UnixStream",
        ".accept()",
        "read_to_string",
        "write_all",
        "flush",
        "handler",
    ] {
        if !transport.text.contains(required) {
            failures.push(format!(
                "context-graph owner-control transport omitted {required}"
            ));
        }
    }

    for forbidden in [
        "\"why\"",
        "\"events\"",
        "\"stats\"",
        "Observation",
        "Decision",
        "Intention",
        "Entity",
        "context_graph::Context",
        "RuntimeStats",
        "Detector",
        "VIGIL_CONTROL_SOCKET",
    ] {
        if transport.text.contains(forbidden) {
            failures.push(format!(
                "context-graph owner-control transport contains vigil domain marker {forbidden}"
            ));
        }
    }
}

fn assert_vigil_keeps_only_control_path_and_handler_layer(
    sources: &[SourceFile],
    failures: &mut Vec<String>,
) {
    let vigil_text = join_sources(sources);
    for forbidden in [
        "UnixListener",
        "UnixStream",
        "tokio::net::UnixListener",
        "tokio::net::UnixStream",
        "std::os::unix::net",
    ] {
        if vigil_text.contains(forbidden) {
            failures.push(format!(
                "vigil source still owns AF_UNIX primitive {forbidden}; transport belongs in context-graph"
            ));
        }
    }

    for required in [
        "VIGIL_CONTROL_SOCKET",
        "control.sock",
        "request_control",
        "start_control_listener",
        "handle_owner_request",
        "\"why\"",
        "\"events\"",
        "\"stats\"",
        "served-by=af_unix",
    ] {
        if !vigil_text.contains(required) {
            failures.push(format!(
                "vigil source omitted required control handler/path marker {required}"
            ));
        }
    }
}

fn assert_live_reads_stay_store_backed(
    vigil_sources: &[SourceFile],
    context_graph_sources: &[SourceFile],
    failures: &mut Vec<String>,
) {
    let vigil_text = join_sources(vigil_sources);
    let cg_text = join_sources(context_graph_sources);

    for expected in [
        "trait StoreReadObserver",
        "struct StoreReadEvent",
        "read_observer",
        "observe_read",
        "writer: \"context-graph\"",
        "StoreReadEvent StoreReadObserver writer={}",
    ] {
        if !cg_text.contains(expected) {
            failures.push(format!(
                "context-graph source omitted Store read observer marker {expected}"
            ));
        }
    }

    for expected in [
        "handle_owner_request",
        "handle_why_read",
        "handle_events_read",
        "StoreBackedWhyResponse",
        "StoreBackedEventsResponse",
        "list_observations",
        "audit_query",
        "get_observation",
        "get_decision",
        "get_intention",
        "get_entity",
        "get_context",
        "from_store_reads",
    ] {
        if !vigil_text.contains(expected) {
            failures.push(format!(
                "vigil live-read source omitted store-backed marker {expected}"
            ));
        }
    }

    for forbidden in [
        "provenance_mirror",
        "shadow_provenance",
        "event_cache",
        "ObservationCache",
        "DecisionCache",
    ] {
        if vigil_text.contains(forbidden) {
            failures.push(format!(
                "vigil live-read source retained forbidden mirror marker {forbidden}"
            ));
        }
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn collect_rust_source_files(root: &Path) -> Vec<SourceFile> {
    let mut sources = Vec::new();
    collect_rust_source_files_into(root, &mut sources);
    sources
}

fn collect_rust_source_files_into(dir: &Path, sources: &mut Vec<SourceFile>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_source_files_into(&path, sources);
        } else if path.extension() == Some(OsStr::new("rs"))
            && let Ok(text) = fs::read_to_string(&path)
        {
            sources.push(SourceFile { path, text });
        }
    }
}

fn join_sources(sources: &[SourceFile]) -> String {
    sources
        .iter()
        .map(|source| source.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

struct SourceFile {
    path: PathBuf,
    text: String,
}
