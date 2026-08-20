use serde::{Deserialize, Serialize};

use crate::Result;

pub const MANAGED_PROTOCOL_VERSION: u32 = 1;
pub const MANAGED_READY_PATH: &str = "/api/managed/ready";
pub const MANAGED_SHUTDOWN_PATH: &str = "/api/managed/shutdown";
pub const MANAGED_AUTH_HEADER: &str = "Authorization";
pub const MANAGED_BIND_ADDRESS: &str = "127.0.0.1";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedLaunchConfig {
    pub protocol_version: u32,
    pub port: u16,
    pub token: String,
    pub instance_nonce: String,
}

impl ManagedLaunchConfig {
    pub fn validate(&self) -> Result<()> {
        if self.protocol_version != MANAGED_PROTOCOL_VERSION {
            return Err(format!(
                "unsupported gateway protocol version {}; expected {MANAGED_PROTOCOL_VERSION}",
                self.protocol_version
            )
            .into());
        }
        if self.port == 0 {
            return Err("managed WebUI port must not be zero".into());
        }
        validate_hex_secret("managed token", &self.token, 64)?;
        validate_hex_secret("managed instance nonce", &self.instance_nonce, 32)?;
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ManagedReadyResponse {
    pub ok: bool,
    pub ready: bool,
    pub protocol_version: u32,
    pub product_version: String,
    pub workspace_path: String,
    pub instance_nonce: String,
}

pub fn random_hex_secret(bytes: usize) -> Result<String> {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value)
        .map_err(|error| format!("failed to generate private gateway credential: {error}"))?;
    Ok(value.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn bearer_header_value(token: &str) -> String {
    format!("Bearer {token}")
}

pub fn bearer_token_matches(header: Option<&str>, expected: &str) -> bool {
    let Some(actual) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
        return false;
    };
    constant_time_eq(actual.as_bytes(), expected.as_bytes())
}

fn validate_hex_secret(name: &str, value: &str, expected_len: usize) -> Result<()> {
    if value.len() != expected_len || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("{name} must be {expected_len} hexadecimal characters").into());
    }
    Ok(())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut different = 0_u8;
    for (left, right) in left.iter().zip(right) {
        different |= left ^ right;
    }
    different == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> ManagedLaunchConfig {
        ManagedLaunchConfig {
            protocol_version: MANAGED_PROTOCOL_VERSION,
            port: 41001,
            token: "ab".repeat(32),
            instance_nonce: "cd".repeat(16),
        }
    }

    #[test]
    fn launch_config_requires_the_exact_protocol_and_private_values() {
        launch().validate().unwrap();
        let mut invalid = launch();
        invalid.protocol_version += 1;
        assert!(invalid.validate().is_err());
        let mut invalid = launch();
        invalid.port = 0;
        assert!(invalid.validate().is_err());
        let mut invalid = launch();
        invalid.token = "not-secret".into();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn bearer_authentication_requires_an_exact_token() {
        let token = "ab".repeat(32);
        assert!(bearer_token_matches(
            Some(&bearer_header_value(&token)),
            &token
        ));
        assert!(!bearer_token_matches(Some(&token), &token));
        assert!(!bearer_token_matches(
            Some(&bearer_header_value(&"ac".repeat(32))),
            &token
        ));
    }

    #[test]
    fn generated_private_values_have_the_requested_entropy_width() {
        let token = random_hex_secret(32).unwrap();
        let nonce = random_hex_secret(16).unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(nonce.len(), 32);
        assert_ne!(token, random_hex_secret(32).unwrap());
    }
}
