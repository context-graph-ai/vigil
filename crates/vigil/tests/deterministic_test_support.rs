#![allow(dead_code)]

use std::io::Read;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub struct TcpPortReservation {
    listener: Option<TcpListener>,
    port: u16,
}

impl TcpPortReservation {
    pub fn reserve_loopback() -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("reserve loopback TCP port: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("read reserved TCP port: {error}"))?
            .port();
        Ok(Self {
            listener: Some(listener),
            port,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn reserve_specific(port: u16) -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|error| format!("reserve loopback TCP port {port}: {error}"))?;
        Ok(Self {
            listener: Some(listener),
            port,
        })
    }

    /// Release immediately before spawning an external process that cannot
    /// inherit the listener. The caller must detect and retry bind races.
    pub fn release(mut self) -> u16 {
        self.listener.take();
        self.port
    }
}

pub fn wait_until<T>(
    description: &str,
    timeout: Duration,
    mut check: impl FnMut() -> Result<Option<T>, String>,
) -> Result<T, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = check()? {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out after {:.2}s waiting for {description}",
                timeout.as_secs_f64()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// Generic process-output capture: carries no context-graph dependency, so it
// lives here (not in `deterministic_fixture_support.rs`) and is reachable
// context-graph-free by anything that only needs to capture a spawned
// process's stdout/stderr (e.g. the Mosquitto broker fixture). Re-exported
// from `deterministic_fixture_support.rs` for its existing consumers.
pub fn capture_pipe<T: Read + Send + 'static>(pipe: Option<T>) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = pipe {
        let captured = Arc::clone(&buf);
        thread::spawn(move || {
            let mut tmp = [0u8; 4096];
            loop {
                match pipe.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut g) = captured.lock() {
                            g.push_str(&String::from_utf8_lossy(&tmp[..n]));
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    buf
}
