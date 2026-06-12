mod config;
mod health;
mod privilege;
mod runtime;
mod shutdown;
mod store;

use std::ffi::OsString;
use std::process::ExitCode;

pub fn run_cli<I>(args: I) -> ExitCode
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _program = args.next();

    match args.next().and_then(|arg| arg.into_string().ok()) {
        Some(flag) if flag == "--version" => {
            println!("vigil {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(flag) if flag == "--help" || flag == "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(command) if command == "run" => runtime::run(args.collect()),
        _ => {
            print_help();
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!("Usage: vigil <COMMAND>");
    println!();
    println!("Commands:");
    println!("  run");
    println!();
    println!("Options:");
    println!("  --help");
    println!("  --version");
}
