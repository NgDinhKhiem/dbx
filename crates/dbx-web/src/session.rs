//! In-memory login sessions for DBX Web.
//!
//! Sessions expire after an idle timeout (`DBX_SESSION_IDLE_TIMEOUT_SECS`,
//! default 12h) and after an absolute lifetime (`DBX_SESSION_MAX_AGE_SECS`,
//! default 7d). At most `DBX_SESSION_MAX_COUNT` sessions (default 1024) are
//! kept; creating one more evicts the least recently used session. Every
//! session carries a cancellation token that fires when it is revoked or
//! expires so long-lived responses (SSE, WebSocket) can end with it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use tokio_util::sync::CancellationToken;

pub const DEFAULT_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(12 * 60 * 60);
pub const DEFAULT_SESSION_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const DEFAULT_MAX_SESSIONS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    pub idle_timeout: Duration,
    pub max_age: Duration,
    pub max_sessions: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            max_age: DEFAULT_SESSION_MAX_AGE,
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }
}

impl SessionConfig {
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var("DBX_SESSION_IDLE_TIMEOUT_SECS").ok().as_deref(),
            std::env::var("DBX_SESSION_MAX_AGE_SECS").ok().as_deref(),
            std::env::var("DBX_SESSION_MAX_COUNT").ok().as_deref(),
        )
    }

    pub fn from_values(idle_secs: Option<&str>, max_age_secs: Option<&str>, max_count: Option<&str>) -> Self {
        fn positive<T: std::str::FromStr + PartialOrd + Default>(value: Option<&str>) -> Option<T> {
            value.and_then(|value| value.trim().parse::<T>().ok()).filter(|value| *value > T::default())
        }
        let defaults = Self::default();
        Self {
            idle_timeout: positive::<u64>(idle_secs).map(Duration::from_secs).unwrap_or(defaults.idle_timeout),
            max_age: positive::<u64>(max_age_secs).map(Duration::from_secs).unwrap_or(defaults.max_age),
            max_sessions: positive::<usize>(max_count).unwrap_or(defaults.max_sessions),
        }
    }
}

struct SessionEntry {
    created: Instant,
    last_seen: Instant,
    revoked: CancellationToken,
}

pub enum SessionLookup {
    /// The session is valid; the token fires when it is revoked or expires.
    Valid(CancellationToken),
    /// The session existed but has expired and was removed.
    Expired,
    Missing,
}

pub struct SessionStore {
    config: SessionConfig,
    entries: Mutex<HashMap<String, SessionEntry>>,
}

impl SessionStore {
    pub fn new(config: SessionConfig) -> Self {
        Self { config, entries: Mutex::new(HashMap::new()) }
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionEntry>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_expired(&self, entry: &SessionEntry, now: Instant) -> bool {
        now.saturating_duration_since(entry.created) >= self.config.max_age
            || now.saturating_duration_since(entry.last_seen) >= self.config.idle_timeout
    }

    /// Creates a session and returns its token plus the tokens evicted to stay
    /// within the configured session cap.
    pub fn create(&self) -> (String, Vec<String>) {
        self.create_at(Instant::now())
    }

    pub(crate) fn create_at(&self, now: Instant) -> (String, Vec<String>) {
        let token = new_session_token();
        let mut entries = self.entries();
        let mut evicted = Vec::new();
        // Expired sessions go first, then the least recently used ones.
        entries.retain(|token, entry| {
            let expired = self.is_expired(entry, now);
            if expired {
                entry.revoked.cancel();
                evicted.push(token.clone());
            }
            !expired
        });
        while entries.len() >= self.config.max_sessions.max(1) {
            let Some(oldest) = entries.iter().min_by_key(|(_, entry)| entry.last_seen).map(|(token, _)| token.clone())
            else {
                break;
            };
            if let Some(entry) = entries.remove(&oldest) {
                entry.revoked.cancel();
            }
            evicted.push(oldest);
        }
        entries.insert(token.clone(), SessionEntry { created: now, last_seen: now, revoked: CancellationToken::new() });
        (token, evicted)
    }

    /// Validates a session and records activity on it.
    pub fn validate(&self, token: &str) -> SessionLookup {
        self.validate_at(token, Instant::now())
    }

    pub(crate) fn validate_at(&self, token: &str, now: Instant) -> SessionLookup {
        let mut entries = self.entries();
        let Some(entry) = entries.get_mut(token) else {
            return SessionLookup::Missing;
        };
        if self.is_expired(entry, now) {
            if let Some(entry) = entries.remove(token) {
                entry.revoked.cancel();
            }
            return SessionLookup::Expired;
        }
        entry.last_seen = entry.last_seen.max(now);
        SessionLookup::Valid(entry.revoked.clone())
    }

    pub fn remove(&self, token: &str) -> bool {
        match self.entries().remove(token) {
            Some(entry) => {
                entry.revoked.cancel();
                true
            }
            None => false,
        }
    }

    /// Revokes every session except `keep`; returns the removed tokens.
    pub fn remove_all_except(&self, keep: Option<&str>) -> Vec<String> {
        let mut removed = Vec::new();
        self.entries().retain(|token, entry| {
            if keep == Some(token.as_str()) {
                return true;
            }
            entry.revoked.cancel();
            removed.push(token.clone());
            false
        });
        removed
    }

    /// Removes expired sessions; returns the removed tokens.
    pub fn sweep(&self) -> Vec<String> {
        self.sweep_at(Instant::now())
    }

    pub(crate) fn sweep_at(&self, now: Instant) -> Vec<String> {
        let mut removed = Vec::new();
        self.entries().retain(|token, entry| {
            let expired = self.is_expired(entry, now);
            if expired {
                entry.revoked.cancel();
                removed.push(token.clone());
            }
            !expired
        });
        removed
    }

    /// Test helper: registers a known token as a fresh session.
    #[cfg(test)]
    pub fn insert_for_tests(&self, token: String) -> bool {
        let now = Instant::now();
        self.entries()
            .insert(token, SessionEntry { created: now, last_seen: now, revoked: CancellationToken::new() })
            .is_none()
    }

    /// Test-only shim for fixtures written against the previous
    /// `RwLock<HashSet<String>>` (`state.sessions.write().await.insert(..)`).
    #[cfg(test)]
    pub async fn write(&self) -> TestSessionWriter<'_> {
        TestSessionWriter(self)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries().len()
    }

    #[cfg(test)]
    pub fn contains(&self, token: &str) -> bool {
        self.entries().contains_key(token)
    }
}

#[cfg(test)]
pub struct TestSessionWriter<'a>(&'a SessionStore);

#[cfg(test)]
impl TestSessionWriter<'_> {
    pub fn insert(&mut self, token: String) -> bool {
        self.0.insert_for_tests(token)
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new(SessionConfig::default())
    }
}

/// 256-bit random token encoded as lowercase hex.
pub fn new_session_token() -> String {
    random_hex_token(32)
}

pub fn random_hex_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Constant-time comparison for secrets of equal length.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter().zip(right).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(idle: u64, max_age: u64, max_sessions: usize) -> SessionStore {
        SessionStore::new(SessionConfig {
            idle_timeout: Duration::from_secs(idle),
            max_age: Duration::from_secs(max_age),
            max_sessions,
        })
    }

    #[test]
    fn config_parses_positive_values_and_falls_back_to_defaults() {
        assert_eq!(SessionConfig::from_values(None, None, None), SessionConfig::default());
        assert_eq!(SessionConfig::from_values(Some("0"), Some("-1"), Some("abc")), SessionConfig::default());
        let config = SessionConfig::from_values(Some("60"), Some(" 3600 "), Some("5"));
        assert_eq!(config.idle_timeout, Duration::from_secs(60));
        assert_eq!(config.max_age, Duration::from_secs(3600));
        assert_eq!(config.max_sessions, 5);
    }

    #[test]
    fn tokens_are_random_hex() {
        let first = new_session_token();
        let second = new_session_token();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn idle_timeout_expires_sessions_and_activity_extends_them() {
        let store = store(60, 3600, 10);
        let start = Instant::now();
        let (token, _) = store.create_at(start);
        assert!(matches!(store.validate_at(&token, start + Duration::from_secs(50)), SessionLookup::Valid(_)));
        // Activity at +50s moves the idle deadline to +110s.
        assert!(matches!(store.validate_at(&token, start + Duration::from_secs(100)), SessionLookup::Valid(_)));
        let SessionLookup::Valid(revoked) = store.validate_at(&token, start + Duration::from_secs(150)) else {
            panic!("session should still be valid");
        };
        assert!(matches!(store.validate_at(&token, start + Duration::from_secs(211)), SessionLookup::Expired));
        assert!(revoked.is_cancelled());
        assert!(matches!(store.validate_at(&token, start + Duration::from_secs(212)), SessionLookup::Missing));
    }

    #[test]
    fn absolute_lifetime_expires_active_sessions() {
        let store = store(60, 120, 10);
        let start = Instant::now();
        let (token, _) = store.create_at(start);
        for seconds in [30, 60, 90] {
            assert!(matches!(store.validate_at(&token, start + Duration::from_secs(seconds)), SessionLookup::Valid(_)));
        }
        assert!(matches!(store.validate_at(&token, start + Duration::from_secs(120)), SessionLookup::Expired));
    }

    #[test]
    fn session_cap_evicts_least_recently_used() {
        let store = store(3600, 7200, 2);
        let start = Instant::now();
        let (first, _) = store.create_at(start);
        let (second, _) = store.create_at(start + Duration::from_secs(1));
        assert!(matches!(store.validate_at(&first, start + Duration::from_secs(2)), SessionLookup::Valid(_)));
        let (third, evicted) = store.create_at(start + Duration::from_secs(3));
        assert_eq!(evicted, vec![second.clone()]);
        assert_eq!(store.len(), 2);
        assert!(store.contains(&first));
        assert!(store.contains(&third));
        assert!(!store.contains(&second));
    }

    #[test]
    fn sweep_and_remove_all_except_revoke_sessions() {
        let store = store(60, 3600, 10);
        let start = Instant::now();
        let (stale, _) = store.create_at(start);
        // Created before `stale` expires, so creation does not already evict it.
        let (fresh, _) = store.create_at(start + Duration::from_secs(50));
        assert!(matches!(store.validate_at(&fresh, start + Duration::from_secs(100)), SessionLookup::Valid(_)));
        assert_eq!(store.sweep_at(start + Duration::from_secs(120)), vec![stale]);
        let (other, _) = store.create_at(start + Duration::from_secs(120));
        let SessionLookup::Valid(other_revoked) = store.validate_at(&other, start + Duration::from_secs(121)) else {
            panic!("session should be valid");
        };
        assert_eq!(store.remove_all_except(Some(&fresh)), vec![other]);
        assert!(other_revoked.is_cancelled());
        assert!(store.contains(&fresh));
        assert!(store.remove(&fresh));
        assert!(!store.remove(&fresh));
    }

    #[test]
    fn constant_time_eq_compares_contents_and_length() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
