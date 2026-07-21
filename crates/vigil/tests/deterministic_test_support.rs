#![allow(dead_code)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::Packet;
use rumqttc::v5::{Client, Connection, Event, MqttOptions, RecvTimeoutError};

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

pub fn required_tool(env_key: &str, binary: &str, fallbacks: &[&Path]) -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(env_key).map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "{env_key} points to missing {binary} binary: {}",
            path.display()
        ));
    }
    if let Some(path) = std::env::var_os("PATH").and_then(|path_var| {
        std::env::split_paths(&path_var)
            .map(|dir| dir.join(binary))
            .find(|path| path.is_file())
    }) {
        return Ok(path);
    }
    fallbacks
        .iter()
        .find(|path| path.is_file())
        .map(|path| path.to_path_buf())
        .ok_or_else(|| {
            format!(
                "required test prerequisite `{binary}` was not found; set {env_key} or add it to PATH"
            )
        })
}

#[derive(Debug, Clone)]
pub struct ObservedPublish {
    pub topic: String,
    pub payload: Vec<u8>,
    pub retain: bool,
}

pub struct MqttProbe {
    client: Client,
    connection: Connection,
}

impl MqttProbe {
    pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<Self, String> {
        Self::connect_and_subscribe(host, port, &[], timeout)
    }

    pub fn connect_and_subscribe(
        host: &str,
        port: u16,
        topics: &[&str],
        timeout: Duration,
    ) -> Result<Self, String> {
        Self::connect_and_subscribe_with_max_packet(host, port, topics, timeout, None)
    }

    pub fn connect_and_subscribe_with_max_packet(
        host: &str,
        port: u16,
        topics: &[&str],
        timeout: Duration,
        max_packet_size: Option<u32>,
    ) -> Result<Self, String> {
        static NEXT_ID: AtomicU32 = AtomicU32::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut options = MqttOptions::new(format!("vigil-test-probe-{id}"), host, port);
        options.set_keep_alive(Duration::from_secs(5));
        if max_packet_size.is_some() {
            options.set_max_packet_size(max_packet_size);
        }
        let (client, connection) = Client::new(options, 32);
        let mut probe = Self { client, connection };

        probe.wait_for_packet("MQTT CONNACK", timeout, |packet| {
            matches!(packet, Packet::ConnAck(_))
        })?;
        for topic in topics {
            probe
                .client
                .subscribe(*topic, QoS::AtLeastOnce)
                .map_err(|error| format!("subscribe to {topic}: {error}"))?;
            probe.wait_for_packet(&format!("MQTT SUBACK for {topic}"), timeout, |packet| {
                matches!(packet, Packet::SubAck(_))
            })?;
        }
        Ok(probe)
    }

    pub fn publish_qos1(&self, topic: &str, payload: impl AsRef<[u8]>) -> Result<(), String> {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload.as_ref().to_vec())
            .map_err(|error| format!("publish to {topic}: {error}"))
    }

    pub fn wait_for_pubacks(&mut self, count: usize, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut observed = 0;
        while observed < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out after {:.2}s waiting for {count} MQTT PUBACKs; observed {observed}",
                    timeout.as_secs_f64()
                ));
            }
            match self
                .connection
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(Ok(Event::Incoming(Packet::PubAck(_)))) => observed += 1,
                Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Err(error)) => return Err(format!("MQTT connection error: {error}")),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("MQTT probe disconnected while waiting for PUBACKs".to_string());
                }
            }
        }
        Ok(())
    }

    pub fn recv_matching(
        &mut self,
        description: &str,
        timeout: Duration,
        mut matches: impl FnMut(&ObservedPublish) -> bool,
    ) -> Result<ObservedPublish, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out after {:.2}s waiting for {description}",
                    timeout.as_secs_f64()
                ));
            }
            match self
                .connection
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(Ok(Event::Incoming(Packet::Publish(publish)))) => {
                    let observed = ObservedPublish {
                        topic: String::from_utf8_lossy(&publish.topic).into_owned(),
                        payload: publish.payload.to_vec(),
                        retain: publish.retain,
                    };
                    if matches(&observed) {
                        return Ok(observed);
                    }
                }
                Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Err(error)) => return Err(format!("MQTT connection error: {error}")),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("MQTT probe disconnected".to_string());
                }
            }
        }
    }

    fn wait_for_packet(
        &mut self,
        description: &str,
        timeout: Duration,
        mut matches: impl FnMut(&Packet) -> bool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out after {:.2}s waiting for {description}",
                    timeout.as_secs_f64()
                ));
            }
            match self
                .connection
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(Ok(Event::Incoming(packet))) if matches(&packet) => return Ok(()),
                Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Err(error)) => return Err(format!("MQTT connection error: {error}")),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!(
                        "MQTT probe disconnected while waiting for {description}"
                    ));
                }
            }
        }
    }
}
