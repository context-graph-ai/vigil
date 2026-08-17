use std::path::{Path, PathBuf};

// VIGIL_CONTROL_SOCKET is an enumerated, reviewed override
// (`environment_read_surface.baseline.txt`), not an ad-hoc read.
#[allow(clippy::disallowed_methods)]
/// Where the running runtime publishes its control socket for a deployment.
/// One place fixes the relationship, so a caller — a test included — never has
/// to assume the filename.
pub fn control_socket_path(data_dir: &Path) -> PathBuf {
    std::env::var_os("VIGIL_CONTROL_SOCKET")
        .map(Into::into)
        .unwrap_or_else(|| data_dir.join("control.sock"))
}
