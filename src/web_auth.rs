use std::{collections::HashSet, sync::Mutex};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};

use crate::Result;

pub(crate) fn port_scoped_cookie_name(prefix: &str, port: u16) -> String {
    format!("{prefix}_p{port}")
}

pub struct WebSessionAuth {
    cookie_name: String,
    password_hash: Option<String>,
    sessions: Mutex<HashSet<String>>,
}

impl WebSessionAuth {
    pub fn new(cookie_name: impl Into<String>, passkey: Option<&str>) -> Result<Self> {
        let password_hash = passkey
            .map(|passkey| -> Result<String> {
                let mut salt = [0_u8; 16];
                getrandom::fill(&mut salt)
                    .map_err(|error| format!("failed to generate WebUI password salt: {error}"))?;
                let salt = SaltString::encode_b64(&salt)
                    .map_err(|error| format!("failed to encode WebUI password salt: {error}"))?;
                let hash = Argon2::default()
                    .hash_password(passkey.as_bytes(), &salt)
                    .map_err(|error| format!("failed to hash WebUI password: {error}"))?;
                Ok(hash.to_string())
            })
            .transpose()?;
        Ok(Self {
            cookie_name: cookie_name.into(),
            password_hash,
            sessions: Mutex::new(HashSet::new()),
        })
    }

    pub fn cookie_name(&self) -> &str {
        &self.cookie_name
    }

    pub fn required(&self) -> bool {
        self.password_hash.is_some()
    }

    pub fn authorized(&self, token: Option<&str>) -> bool {
        if !self.required() {
            return true;
        }
        let Some(token) = token else {
            return false;
        };
        self.sessions
            .lock()
            .is_ok_and(|sessions| sessions.contains(token))
    }

    pub fn login(&self, passkey: &str) -> Result<Option<String>> {
        let Some(encoded) = &self.password_hash else {
            return Ok(Some(String::new()));
        };
        let parsed = PasswordHash::new(encoded)
            .map_err(|error| format!("stored WebUI password hash is invalid: {error}"))?;
        if Argon2::default()
            .verify_password(passkey.as_bytes(), &parsed)
            .is_err()
        {
            return Ok(None);
        }
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|error| format!("failed to generate WebUI session: {error}"))?;
        let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        self.sessions
            .lock()
            .map_err(|_| "WebUI session store is unavailable")?
            .insert(token.clone());
        Ok(Some(token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passkey_sessions_are_process_local_and_secret_free() {
        let auth = WebSessionAuth::new("test_session_p38199", Some("correct horse")).unwrap();
        assert!(auth.required());
        assert!(!auth.authorized(None));
        assert!(auth.login("wrong").unwrap().is_none());
        let token = auth.login("correct horse").unwrap().unwrap();
        assert!(!token.contains("correct horse"));
        assert!(auth.authorized(Some(&token)));
        assert!(
            !WebSessionAuth::new("test_session_p38201", Some("correct horse"))
                .unwrap()
                .authorized(Some(&token))
        );
    }

    #[test]
    fn missing_passkey_keeps_the_service_open() {
        let auth = WebSessionAuth::new("test_session_p38199", None).unwrap();
        assert!(!auth.required());
        assert!(auth.authorized(None));
    }

    #[test]
    fn cookie_names_are_scoped_to_service_and_actual_port() {
        let direct = port_scoped_cookie_name("me_webui_session", 38199);
        let gateway = port_scoped_cookie_name("me_gateway_session", 38199);
        assert_eq!(direct, "me_webui_session_p38199");
        assert_eq!(gateway, "me_gateway_session_p38199");
        assert_ne!(direct, port_scoped_cookie_name("me_webui_session", 38201));
        assert_ne!(direct, gateway);
        assert!(direct.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
        }));
    }
}
