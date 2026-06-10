use std::ffi::OsString;
use std::process::ExitCode;

pub fn run_cli<I>(args: I) -> ExitCode
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _program = args.next();
    match args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .as_deref()
    {
        Some("--version") => {
            println!("vigil");
            ExitCode::SUCCESS
        }
        Some("--help") | Some("-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("run") => run_unavailable(args.collect()),
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
    println!("  diagnose");
    println!();
    println!("Options:");
    println!("  --help");
    println!("  --version");
}

fn run_unavailable(_args: Vec<OsString>) -> ExitCode {
    eprintln!("vigil runtime is unavailable");
    ExitCode::from(78)
}
