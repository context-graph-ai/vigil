//! The Vigil binary's composition root: the one place core and the Home
//! Assistant adapter are wired together. Core has no knowledge of the
//! adapter; this crate is the only thing that depends on both.

fn main() -> std::process::ExitCode {
    vigil::run_cli_with_site_channel(std::env::args_os(), &vigil_ha::HomeAssistantMqtt)
}
