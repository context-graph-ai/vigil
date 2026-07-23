//! A wrapper for secret string values (passwords, tokens) that closes off
//! every *implicit* disclosure path.
//!
//! [`Secret`] has no `Display` implementation at all, so `println!("{}",
//! secret)` and any `format!`/log call that interpolates it directly fails to
//! compile. Its `Debug` implementation always prints a fixed redaction and
//! never the wrapped value, so a `#[derive(Debug)]` struct that holds a
//! `Secret` field stays safe to print without any extra care at the call
//! site — a secret cannot reach a log line or a debug dump by accident. The
//! only way to read the real value back out is [`Secret::expose_secret`] — a
//! name chosen so every deliberate exposure is one greppable call; nothing
//! stops a caller from printing what that call returns, so exposure is a
//! choice each call site makes visibly, not an accident the type prevents.

use std::fmt;

use serde::Deserialize;

const REDACTED: &str = "<redacted>";

#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Returns the real secret value. Every call site is a deliberate,
    /// reviewable exposure of the secret.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Whether the wrapped value is the empty string. Checking this does
    /// not disclose the secret's contents.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Secret {}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_the_secret_and_always_shows_the_redaction() {
        let secret = Secret::new("hunter2".to_string());
        let debug_output = format!("{secret:?}");

        assert!(
            !debug_output.contains("hunter2"),
            "Debug output must never contain the secret value, got {debug_output:?}"
        );
        assert!(
            debug_output.contains(REDACTED),
            "Debug output must contain the fixed redaction, got {debug_output:?}"
        );
    }

    #[test]
    fn expose_secret_round_trips_the_original_value() {
        let secret = Secret::new("hunter2".to_string());
        assert_eq!(secret.expose_secret(), "hunter2");
    }

    #[test]
    fn equality_and_clone_compare_and_preserve_the_underlying_value() {
        let secret = Secret::new("hunter2".to_string());
        let cloned = secret.clone();
        assert_eq!(secret, cloned);
        assert_ne!(secret, Secret::new("different".to_string()));
    }

    #[test]
    fn deserializes_from_a_plain_json_string() {
        let secret: Secret = serde_json::from_str("\"hunter2\"").expect("valid JSON string");
        assert_eq!(secret.expose_secret(), "hunter2");
    }

    #[test]
    fn is_empty_reports_without_exposing_the_value() {
        assert!(Secret::new(String::new()).is_empty());
        assert!(!Secret::new("hunter2".to_string()).is_empty());
    }
}
