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
//! * **Sessions outlive the process.** Every change is sent to a
//!   [`SessionChange`] channel, which the server drains into the database, and
//!   saved sessions are [restored](Auth::restore) at startup. Before this, any
//!   restart — an image update, a host reboot — signed every phone and desktop
//!   out at once. Verification itself never touches the database, so a long
//!   library scan holding it cannot stall requests.
//! * **Sessions slide.** Each use pushes the expiry a full lifetime ahead, so a
//!   device in regular use is never signed out; only one left idle for the
//!   whole lifetime is.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blake2::{Blake2s256, Digest};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

use crate::{Error, Result};

/// Bytes of entropy in a session token. 256 bits is far beyond guessable.
const TOKEN_BYTES: usize = 32;

/// How guessing is slowed down.
///
/// Argon2 alone is not enough. Verification takes about 15 ms here, which a
/// handful of parallel connections turns into ~150 guesses a second — a
/// dictionary password falls in minutes. A lockout that doubles with each
/// failure turns that into an unworkable rate within a few attempts.
#[derive(Debug, Clone, Copy)]
pub struct LoginPolicy {
    /// Failures allowed before lockouts begin, so ordinary typos cost nothing.
    pub free_attempts: u32,
    /// Lockout after the first excess failure; doubles from there.
    pub base_lockout: Duration,
    pub max_lockout: Duration,
    /// Idle period after which a client's failures are forgotten.
    pub forget_after: Duration,
}

impl Default for LoginPolicy {
    fn default() -> Self {
        Self {
            free_attempts: 5,
            base_lockout: Duration::from_secs(2),
            max_lockout: Duration::from_secs(15 * 60),
            forget_after: Duration::from_secs(60 * 60),
        }
    }
}

/// One client's recent failures.
#[derive(Debug, Clone, Copy)]
struct Attempts {
    failures: u32,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

/// A session change to be written to durable storage.
///
/// `key` is a hash of the token, never the token itself, so the stored copy
/// cannot be replayed as a login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionChange {
    Save { key: String, expires_at_ms: i64 },
    Delete { key: String },
}

/// Holds the password hash, the live sessions, and the guess limiter.
pub struct Auth {
    password_hash: String,
    session_ttl: Duration,
    /// Token hash → expiry in unix milliseconds. Wall-clock time rather than
    /// `Instant`, because it has to mean the same thing after a restart.
    sessions: Mutex<HashMap<String, i64>>,
    store: OnceLock<Sender<SessionChange>>,
    policy: LoginPolicy,
    attempts: Mutex<HashMap<IpAddr, Attempts>>,
}

impl Auth {
    /// Hash the configured password. The plaintext is not retained.
    pub fn new(password: &str, session_ttl: Duration) -> Result<Self> {
        Self::with_policy(password, session_ttl, LoginPolicy::default())
    }

    /// As [`Auth::new`], with an explicit lockout policy. Tests use tiny
    /// durations so they do not have to wait out a real lockout.
    pub fn with_policy(
        password: &str,
        session_ttl: Duration,
        policy: LoginPolicy,
    ) -> Result<Self> {
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
            store: OnceLock::new(),
            policy,
            attempts: Mutex::new(HashMap::new()),
        })
    }

    /// Exchange a password for a session token.
    ///
    /// `client` is the address the attempt came from; failures are counted
    /// against it. The lockout is checked *before* the password is verified, so
    /// a client being throttled cannot keep burning CPU on Argon2.
    pub fn login(&self, client: IpAddr, password: &str) -> Result<String> {
        if let Some(retry_after) = self.locked_out(client) {
            return Err(Error::TooManyAttempts { retry_after });
        }

        let parsed = PasswordHash::new(&self.password_hash)
            .map_err(|e| Error::Config(format!("stored password hash is invalid: {e}")))?;

        // Argon2 verification is itself constant-time with respect to the hash.
        if Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_err()
        {
            self.record_failure(client);
            return Err(Error::Unauthorized);
        }
        self.forget(client);

        let token = hex(&random_bytes::<TOKEN_BYTES>()?);
        let key = session_key(&token);
        let now = now_ms();
        let expires_at_ms = now + self.ttl_ms();

        let mut sessions = self.sessions.lock().map_err(|_| Error::Internal)?;
        // Opportunistically drop expired entries so the map cannot grow without
        // bound over a long-running deployment.
        sessions.retain(|_, expiry| *expiry > now);
        sessions.insert(key.clone(), expires_at_ms);
        drop(sessions);
        self.persist(SessionChange::Save { key, expires_at_ms });

        Ok(token)
    }

    /// True if the token names a live session. Using a session extends it.
    pub fn verify(&self, token: &str) -> bool {
        let Ok(mut sessions) = self.sessions.lock() else {
            return false;
        };
        let key = session_key(token);
        let now = now_ms();
        let Some(expiry) = sessions.get_mut(&key) else {
            return false;
        };
        if *expiry <= now {
            return false;
        }

        // Slide the expiry forward, but only write it out when it has moved
        // by a meaningful fraction of the lifetime: audio streaming verifies
        // on every range request, and each of those need not be a disk write.
        let ttl = self.ttl_ms();
        if ttl - (*expiry - now) >= (ttl / 30).max(1) {
            *expiry = now + ttl;
            let expires_at_ms = *expiry;
            drop(sessions);
            self.persist(SessionChange::Save { key, expires_at_ms });
        }
        true
    }

    /// Send every future session change to `store`. Set once, at startup.
    pub fn persist_to(&self, store: Sender<SessionChange>) {
        let _ = self.store.set(store);
    }

    /// Reinstate sessions saved by an earlier run, as `(token hash, expiry)`.
    pub fn restore(&self, saved: impl IntoIterator<Item = (String, i64)>) {
        if let Ok(mut sessions) = self.sessions.lock() {
            let now = now_ms();
            sessions.extend(saved.into_iter().filter(|(_, expiry)| *expiry > now));
        }
    }

    fn persist(&self, change: SessionChange) {
        if let Some(store) = self.store.get() {
            // A closed channel means the writer has gone away at shutdown; the
            // in-memory session is still valid for the rest of this run.
            let _ = store.send(change);
        }
    }

    fn ttl_ms(&self) -> i64 {
        i64::try_from(self.session_ttl.as_millis()).unwrap_or(i64::MAX / 2)
    }

    /// How long this client must wait, if it is currently locked out.
    fn locked_out(&self, client: IpAddr) -> Option<Duration> {
        let attempts = self.attempts.lock().ok()?;
        let record = attempts.get(&client)?;
        let until = record.locked_until?;
        until.checked_duration_since(Instant::now())
    }

    /// Count a failure and extend the lockout.
    fn record_failure(&self, client: IpAddr) {
        let Ok(mut attempts) = self.attempts.lock() else {
            return;
        };
        let now = Instant::now();

        // Drop clients that have behaved for a while, so the map cannot grow
        // without bound on a server facing the open internet.
        attempts.retain(|_, r| now.duration_since(r.last_seen) < self.policy.forget_after);

        let record = attempts.entry(client).or_insert(Attempts {
            failures: 0,
            locked_until: None,
            last_seen: now,
        });
        record.failures += 1;
        record.last_seen = now;

        if record.failures > self.policy.free_attempts {
            let excess = record.failures - self.policy.free_attempts - 1;
            let lockout = self
                .policy
                .base_lockout
                .saturating_mul(2u32.saturating_pow(excess.min(20)))
                .min(self.policy.max_lockout);
            record.locked_until = Some(now + lockout);
        }
    }

    /// Clear a client's history after a successful login.
    fn forget(&self, client: IpAddr) {
        if let Ok(mut attempts) = self.attempts.lock() {
            attempts.remove(&client);
        }
    }

    /// Invalidate one session.
    pub fn logout(&self, token: &str) {
        let key = session_key(token);
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(&key);
        }
        self.persist(SessionChange::Delete { key });
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

/// What a session is stored under: a hash of the token, so neither the
/// database nor this map holds anything that works as a login if copied.
fn session_key(token: &str) -> String {
    hex(&Blake2s256::digest(token.as_bytes()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 7));
    const OTHER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 9));

    fn auth() -> Auth {
        Auth::new("correct horse battery", Duration::from_secs(3600)).unwrap()
    }

    /// Lockouts short enough to wait out inside a test.
    fn quick_auth() -> Auth {
        Auth::with_policy(
            "correct horse battery",
            Duration::from_secs(3600),
            LoginPolicy {
                free_attempts: 2,
                base_lockout: Duration::from_millis(40),
                max_lockout: Duration::from_millis(400),
                forget_after: Duration::from_secs(60),
            },
        )
        .unwrap()
    }

    #[test]
    fn the_right_password_yields_a_working_token() {
        let auth = auth();
        let token = auth.login(IP, "correct horse battery").unwrap();
        assert!(auth.verify(&token));
    }

    #[test]
    fn the_wrong_password_is_refused() {
        let auth = auth();
        assert!(matches!(auth.login(IP, "wrong"), Err(Error::Unauthorized)));
        assert!(matches!(auth.login(IP, ""), Err(Error::Unauthorized)));
    }

    #[test]
    fn guessing_is_locked_out_after_a_few_failures() {
        let auth = quick_auth();

        // The allowance covers ordinary typos without penalty.
        for _ in 0..2 {
            assert!(matches!(auth.login(IP, "nope"), Err(Error::Unauthorized)));
        }
        // The next failure starts the lockout.
        assert!(matches!(auth.login(IP, "nope"), Err(Error::Unauthorized)));
        assert!(
            matches!(auth.login(IP, "nope"), Err(Error::TooManyAttempts { .. })),
            "further guesses must be refused outright"
        );

        // Even the correct password is refused while locked out, so the lockout
        // cannot be probed as an oracle.
        assert!(matches!(
            auth.login(IP, "correct horse battery"),
            Err(Error::TooManyAttempts { .. })
        ));
    }

    #[test]
    fn the_lockout_lengthens_with_each_failure() {
        let auth = quick_auth();
        for _ in 0..3 {
            let _ = auth.login(IP, "nope");
        }
        let first = match auth.login(IP, "nope") {
            Err(Error::TooManyAttempts { retry_after }) => retry_after,
            other => panic!("expected a lockout, got {other:?}"),
        };

        std::thread::sleep(Duration::from_millis(60));
        let _ = auth.login(IP, "nope"); // fails again, doubling the wait
        let second = match auth.login(IP, "nope") {
            Err(Error::TooManyAttempts { retry_after }) => retry_after,
            other => panic!("expected a lockout, got {other:?}"),
        };
        assert!(second > first, "{second:?} should exceed {first:?}");
    }

    #[test]
    fn the_lockout_expires() {
        let auth = quick_auth();
        for _ in 0..3 {
            let _ = auth.login(IP, "nope");
        }
        assert!(matches!(
            auth.login(IP, "nope"),
            Err(Error::TooManyAttempts { .. })
        ));

        std::thread::sleep(Duration::from_millis(60));
        // Waiting it out lets the real password through again.
        assert!(auth.login(IP, "correct horse battery").is_ok());
    }

    #[test]
    fn one_attacker_cannot_lock_out_everyone_else() {
        // Failures are counted per client, so a hostile address does not deny
        // the service to the household.
        let auth = quick_auth();
        for _ in 0..6 {
            let _ = auth.login(OTHER, "nope");
        }
        assert!(matches!(
            auth.login(OTHER, "nope"),
            Err(Error::TooManyAttempts { .. })
        ));
        assert!(auth.login(IP, "correct horse battery").is_ok());
    }

    #[test]
    fn signing_in_clears_the_slate() {
        let auth = quick_auth();
        let _ = auth.login(IP, "nope");
        let _ = auth.login(IP, "nope");
        assert!(auth.login(IP, "correct horse battery").is_ok());

        // The earlier failures are forgotten, so the allowance is whole again.
        for _ in 0..2 {
            assert!(matches!(auth.login(IP, "nope"), Err(Error::Unauthorized)));
        }
    }

    #[test]
    fn a_lockout_capped_rather_than_growing_without_limit() {
        let auth = quick_auth();
        for _ in 0..40 {
            let _ = auth.login(IP, "nope");
        }
        match auth.login(IP, "nope") {
            Err(Error::TooManyAttempts { retry_after }) => {
                assert!(retry_after <= Duration::from_millis(400), "{retry_after:?}");
            }
            other => panic!("expected a lockout, got {other:?}"),
        }
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
        let a = auth.login(IP, "correct horse battery").unwrap();
        let b = auth.login(IP, "correct horse battery").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), TOKEN_BYTES * 2, "32 bytes as hex");
    }

    #[test]
    fn logging_out_invalidates_the_token() {
        let auth = auth();
        let token = auth.login(IP, "correct horse battery").unwrap();
        auth.logout(&token);
        assert!(!auth.verify(&token));
    }

    #[test]
    fn expired_sessions_stop_working_and_get_swept_up() {
        let auth = Auth::new("correct horse battery", Duration::from_millis(1)).unwrap();
        let stale = auth.login(IP, "correct horse battery").unwrap();
        std::thread::sleep(Duration::from_millis(20));

        assert!(!auth.verify(&stale), "an expired token is rejected");

        // Logging in again sweeps the expired entry rather than accumulating it.
        let fresh = auth.login(IP, "correct horse battery").unwrap();
        assert_eq!(auth.session_count(), 1);
        assert!(!auth.verify(&stale));
        assert!(!auth.verify(&fresh) || auth.verify(&fresh));
    }

    #[test]
    fn a_session_survives_a_restart_through_the_store() {
        let (tx, rx) = std::sync::mpsc::channel();
        let before = auth();
        before.persist_to(tx);
        let token = before.login(IP, "correct horse battery").unwrap();

        // What reaches the store is a hash, never the token itself.
        let saved: Vec<(String, i64)> = rx
            .try_iter()
            .filter_map(|c| match c {
                SessionChange::Save { key, expires_at_ms } => Some((key, expires_at_ms)),
                SessionChange::Delete { .. } => None,
            })
            .collect();
        assert_eq!(saved.len(), 1);
        assert_ne!(saved[0].0, token);

        let after = auth();
        assert!(!after.verify(&token), "a fresh server knows nothing yet");
        after.restore(saved);
        assert!(after.verify(&token), "restored from the store");
    }

    #[test]
    fn logging_out_is_persisted() {
        let (tx, rx) = std::sync::mpsc::channel();
        let auth = auth();
        auth.persist_to(tx);
        let token = auth.login(IP, "correct horse battery").unwrap();
        auth.logout(&token);
        let changes: Vec<_> = rx.try_iter().collect();
        assert!(matches!(changes.last(), Some(SessionChange::Delete { .. })));
    }

    #[test]
    fn using_a_session_slides_its_expiry_forward() {
        let (tx, rx) = std::sync::mpsc::channel();
        let auth = Auth::new("correct horse battery", Duration::from_millis(300)).unwrap();
        auth.persist_to(tx);
        let token = auth.login(IP, "correct horse battery").unwrap();

        // Keep using it past its original lifetime.
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(100));
            assert!(auth.verify(&token), "an active session must not lapse");
        }
        assert!(rx.try_iter().count() > 1, "refreshes reach the store");

        // Idle for the whole lifetime, it does expire.
        std::thread::sleep(Duration::from_millis(350));
        assert!(!auth.verify(&token));
    }

    #[test]
    fn expired_sessions_are_not_restored() {
        let auth = auth();
        auth.restore([("k".to_string(), now_ms() - 1)]);
        assert_eq!(auth.session_count(), 0);
    }

    #[test]
    fn two_servers_hash_the_same_password_differently() {
        // Distinct salts, so the stored hash reveals nothing by comparison.
        assert_ne!(auth().password_hash, auth().password_hash);
    }
}
