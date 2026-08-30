use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use context_graph::owner_control::OwnerReadCancellation;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

pub(crate) struct Shutdown {
    flag: Arc<AtomicBool>,
    /// The same ask, as a token the layers below can be interrupted by.
    ///
    /// A flag only reaches code that looks at it. Startup blocks IN THE KERNEL
    /// waiting for another process to let go of this node's store, and nothing
    /// there is going to come back and read a boolean — so the ask has to be
    /// something that can wake it. This is that: cancelled the moment the
    /// signal arrives, and the wait ends with it.
    stop: OwnerReadCancellation,
    asked: Arc<Asked>,
}

/// The ask, as something a thread can block on.
#[derive(Default)]
struct Asked {
    said: Mutex<bool>,
    changed: Condvar,
}

impl Shutdown {
    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    /// The stop token, for a wait that cannot see a flag.
    pub(crate) fn stop(&self) -> OwnerReadCancellation {
        self.stop.clone()
    }

    /// Block until this process has been asked to stop.
    ///
    /// Returns at once when the ask already arrived: the flag is raised by the
    /// signal itself, so a signal delivered during startup is not still waiting
    /// to be noticed here.
    pub(crate) fn wait(&mut self) {
        if self.flag.load(Ordering::SeqCst) {
            return;
        }
        let mut said = self
            .asked
            .said
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*said && !self.flag.load(Ordering::SeqCst) {
            said = self
                .asked
                .changed
                .wait(said)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// Install signal handling for this run.
///
/// The flag is raised BY THE SIGNAL, at the moment it arrives, rather than by
/// whatever part of the process happens to look at the signal queue next. That
/// is the whole point of it: startup can take arbitrarily long — a store this
/// node must own can be held by another process's read for as long as that read
/// lasts — and an operator's `systemctl stop` during that window must be
/// observed there, not queued behind work that is itself waiting. An installed
/// service is promised an orderly exit inside its stop grace, and a stop that is
/// only noticed after startup finishes is not one.
///
/// Everything that has to happen when the signal arrives and cannot happen
/// inside a signal handler — cancelling the stop token, waking whoever is
/// blocked on the ask — happens on a thread of this run's own that does nothing
/// but wait for the signal. It is woken BY the signal, never by a clock.
pub(crate) fn install() -> Result<Shutdown, String> {
    install_parent_death_signal()?;
    let flag = Arc::new(AtomicBool::new(false));
    let stop = OwnerReadCancellation::new();
    let asked = Arc::new(Asked::default());
    let mut signals = Signals::new([SIGTERM, SIGINT])
        .map_err(|error| format!("shutdown signal setup failed: {error}"))?;
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&flag))
            .map_err(|error| format!("shutdown signal setup failed: {error}"))?;
    }
    let signal_flag = Arc::clone(&flag);
    let signal_stop = stop.clone();
    let signal_asked = Arc::clone(&asked);
    thread::spawn(move || {
        let _ = signals.forever().next();
        signal_flag.store(true, Ordering::SeqCst);
        // The store open may be parked in the kernel waiting for another
        // process's read to finish; this is what ends that wait.
        signal_stop.cancel();
        let mut said = signal_asked
            .said
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *said = true;
        signal_asked.changed.notify_all();
    });
    Ok(Shutdown { flag, stop, asked })
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
