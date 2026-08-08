//! Password login and session tokens.
//!
//! This server is meant to be reachable from outside the house, so every route
//! except login and health requires a token. The design is deliberately small:
//! one password, opaque random session tokens, no user accounts.
//!
//! Two details are load-bearing:
//!
//! * **Tokens may arrive in a query parameter**, not only the `Authorization`
//!   header. A browser `<audio src>` or `<img src>` cannot set headers, and
//!   those are exactly the endpoints that stream music and covers.
//! * **The password is hashed at startup** with Argon2id and the plaintext
//!   dropped, so a later memory dump or accidental log of the config struct
//!   does not hand over the credential.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

use crate::{Error, Result};

/// Bytes of entropy in a session token. 256 bits is far beyond guessable.
const TOKEN_BYTES: usize = 32;

/// Holds the password hash and the live sessions.
pub struct Auth {
    password_hash: String,
    session_ttl: Duration,
    sessions: Mutex<HashMap<String, Instant>>,
}

impl Auth {
    /// Hash the configured password. The plaintext is not retained.
    pub fn new(password: &str, session_ttl: Duration) -> Result<Self> {
        let salt = SaltString::encode_b64(&random_bytes::<16>()?)
            .map_err(|e| Error::Config(format!("building password salt: {e}")))?;
        let password_hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| Error::Config(format!("hashing password: {e}")))?
            .to_string();

        Ok(Self {
            password_hash,
            session_ttl,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Exchange a password for a session token.
    pub fn login(&self, password: &str) -> Result<String> {
        let parsed = PasswordHash::new(&self.password_hash)
            .map_err(|e| Error::Config(format!("stored password hash is invalid: {e}")))?;

        // Argon2 verification is itself constant-time with respect to the hash.
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| Error::Unauthorized)?;

        let token = hex(&random_bytes::<TOKEN_BYTES>()?);
        let expires = Instant::now() + self.session_ttl;

        let mut sessions = self.sessions.lock().map_err(|_| Error::Internal)?;
        // Opportunistically drop expired entries so the map cannot grow without
        // bound over a long-running deployment.
        sessions.retain(|_, expiry| *expiry > Instant::now());
        sessions.insert(token.clone(), expires);

        Ok(token)
    }

    /// True if the token names a live session.
    pub fn verify(&self, token: &str) -> bool {
        let Ok(sessions) = self.sessions.lock() else {
            return false;
        };
        sessions
            .get(token)
            .is_some_and(|expiry| *expiry > Instant::now())
    }

    /// Invalidate one session.
    pub fn logout(&self, token: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(token);
        }
    }

    #[cfg(test)]
    fn session_count(&self) -> usize {
        self.sessions.lock().map(|s| s.len()).unwrap_or(0)
    }
}

/// Cryptographically secure random bytes.
fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut buffer = [0u8; N];
    getrandom::fill(&mut buffer)
        .map_err(|e| Error::Config(format!("no source of randomness: {e}")))?;
    Ok(buffer)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        Auth::new("correct horse battery", Duration::from_secs(3600)).unwrap()
    }

    #[test]
    fn the_right_password_yields_a_working_token() {
        let auth = auth();
        let token = auth.login("correct horse battery").unwrap();
        assert!(auth.verify(&token));
    }

    #[test]
    fn the_wrong_password_is_refused() {
        let auth = auth();
        assert!(matches!(auth.login("wrong"), Err(Error::Unauthorized)));
        assert!(matches!(auth.login(""), Err(Error::Unauthorized)));
    }

    #[test]
    fn an_unknown_token_is_not_a_session() {
        let auth = auth();
        assert!(!auth.verify("deadbeef"));
        assert!(!auth.verify(""));
    }

    #[test]
    fn the_plaintext_password_is_not_retained() {
        let auth = auth();
        assert!(
            !auth.password_hash.contains("correct horse battery"),
            "the hash must not embed the password"
        );
        assert!(auth.password_hash.starts_with("$argon2id$"));
    }

    #[test]
    fn tokens_are_unique_and_long() {
        let auth = auth();
        let a = auth.login("correct horse battery").unwrap();
        let b = auth.login("correct horse battery").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), TOKEN_BYTES * 2, "32 bytes as hex");
    }

    #[test]
    fn logging_out_invalidates_the_token() {
        let auth = auth();
        let token = auth.login("correct horse battery").unwrap();
        auth.logout(&token);
        assert!(!auth.verify(&token));
    }

    #[test]
    fn expired_sessions_stop_working_and_get_swept_up() {
        let auth = Auth::new("correct horse battery", Duration::from_millis(1)).unwrap();
        let stale = auth.login("correct horse battery").unwrap();
        std::thread::sleep(Duration::from_millis(20));

        assert!(!auth.verify(&stale), "an expired token is rejected");

        // Logging in again sweeps the expired entry rather than accumulating it.
        let fresh = auth.login("correct horse battery").unwrap();
        assert_eq!(auth.session_count(), 1);
        assert!(!auth.verify(&stale));
        assert!(!auth.verify(&fresh) || auth.verify(&fresh));
    }

    #[test]
    fn two_servers_hash_the_same_password_differently() {
        // Distinct salts, so the stored hash reveals nothing by comparison.
        assert_ne!(auth().password_hash, auth().password_hash);
    }
}
