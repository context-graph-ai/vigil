use std::path::{Path, PathBuf};

pub(crate) fn control_socket_path(data_dir: &Path) -> PathBuf {
    std::env::var_os("VIGIL_CONTROL_SOCKET")
        .map(Into::into)
        .unwrap_or_else(|| data_dir.join("control.sock"))
}
