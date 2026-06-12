use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config;
use crate::health::{HealthServer, HealthState, HealthStatus};
use crate::privilege;
use crate::shutdown;
use crate::store;

pub(crate) fn run(args: Vec<OsString>) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

fn run_inner(args: Vec<OsString>) -> Result<(), String> {
    let config = config::load(args)?;
    privilege::prepare_runtime_user(&config.store_path)?;
    let mut shutdown = shutdown::install()?;
    let health = HealthState::new();
    let server = HealthServer::bind(config.health_port, health.clone(), shutdown.flag())?;

    log_startup(&config);

    let store = match store::open(&config.store_path) {
        Ok(store) => {
            let state = if store.created {
                "store created"
            } else {
                "existing store"
            };
            println!("{state} path={}", store.path.display());
            println!("store opened path={}", store.path.display());
            println!("{}", store.trace);
            println!("runtime loop ready");
            health.set(HealthStatus::Ready, "store open and runtime loop ready");
            Some(store.handle)
        }
        Err(error) => {
            println!(
                "store open error path={} error={}",
                config.store_path.display(),
                error
            );
            health.set(HealthStatus::StoreOpenFailed, "store open failed");
            None
        }
    };

    shutdown.wait();

    drop(store);
    server.join();
    Ok(())
}

fn log_startup(config: &config::RuntimeConfig) {
    println!(
        "vigil version={} startup_epoch={}",
        env!("CARGO_PKG_VERSION"),
        startup_epoch()
    );
    println!("data_dir={}", display(&config.data_dir));
    println!("store_path={}", display(&config.store_path));
    println!("health_port={}", config.health_port);
}

fn startup_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn display(path: &Path) -> String {
    path.display().to_string()
}
