use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

pub(crate) struct Shutdown {
    flag: Arc<AtomicBool>,
    signals: Signals,
}

impl Shutdown {
    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    pub(crate) fn wait(&mut self) {
        let _ = self.signals.forever().next();
        self.flag.store(true, Ordering::SeqCst);
    }
}

pub(crate) fn install() -> Result<Shutdown, String> {
    install_parent_death_signal()?;
    let signals = Signals::new([SIGTERM, SIGINT])
        .map_err(|error| format!("shutdown signal setup failed: {error}"))?;
    Ok(Shutdown {
        flag: Arc::new(AtomicBool::new(false)),
        signals,
    })
}

#[cfg(target_os = "linux")]
fn install_parent_death_signal() -> Result<(), String> {
    let result = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "parent-death signal setup failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn install_parent_death_signal() -> Result<(), String> {
    Ok(())
}
