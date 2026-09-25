//! Per-client limiter for password attempts (login, setup token, change password).
//!
//! Each client IP (IPv6 grouped by /64) gets `MAX_FAILURES` attempts. Reaching
//! the limit locks the client out with exponential backoff (60s, 120s, ... up
//! to 1h). Attempts are reserved atomically before the password is verified,
//! so concurrent requests cannot exceed the budget. The map is bounded; quiet
//! clients are forgotten.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const MAX_FAILURES: u32 = 5;
const BASE_LOCKOUT: Duration = Duration::from_secs(60);
const MAX_LOCKOUT: Duration = Duration::from_secs(60 * 60);
/// Failures older than this no longer count towards the next lockout.
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Clients quiet for this long lose their lockout history.
const ENTRY_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_MAX_ENTRIES: usize = 10_000;

type RateLimitKey = Option<IpAddr>;

#[derive(Debug)]
struct AttemptState {
    failures: u32,
    in_flight: u32,
    lockouts: u32,
    locked_until: Option<Instant>,
    last_failure: Option<Instant>,
    last_seen: Instant,
}

impl AttemptState {
    fn new(now: Instant) -> Self {
        Self { failures: 0, in_flight: 0, lockouts: 0, locked_until: None, last_failure: None, last_seen: now }
    }

    fn is_locked(&self, now: Instant) -> bool {
        self.locked_until.is_some_and(|until| until > now)
    }

    fn is_idle(&self, now: Instant, ttl: Duration) -> bool {
        self.in_flight == 0 && !self.is_locked(now) && now.saturating_duration_since(self.last_seen) >= ttl
    }
}

pub struct LoginRateLimiter {
    entries: Mutex<HashMap<RateLimitKey, AttemptState>>,
    max_entries: usize,
}

impl Default for LoginRateLimiter {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_MAX_ENTRIES)
    }
}

/// Groups IPv6 clients by /64 so rotating addresses inside one allocation does
/// not reset the budget; IPv4-mapped addresses are treated as IPv4.
fn bucket(ip: Option<IpAddr>) -> RateLimitKey {
    ip.map(|ip| match crate::security::canonical_ip(ip) {
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            IpAddr::V6(Ipv6Addr::new(segments[0], segments[1], segments[2], segments[3], 0, 0, 0, 0))
        }
        v4 => v4,
    })
}

fn lockout_duration(lockouts: u32) -> Duration {
    let exponent = lockouts.saturating_sub(1).min(16);
    BASE_LOCKOUT.saturating_mul(1u32 << exponent).min(MAX_LOCKOUT)
}

impl LoginRateLimiter {
    pub fn with_capacity(max_entries: usize) -> Self {
        Self { entries: Mutex::new(HashMap::new()), max_entries: max_entries.max(1) }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<RateLimitKey, AttemptState>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reserves one attempt for `client`, or returns how long to wait.
    pub fn try_begin(&self, client: Option<IpAddr>) -> Result<LoginAttempt<'_>, Duration> {
        self.try_begin_at(client, Instant::now())
    }

    pub(crate) fn try_begin_at(&self, client: Option<IpAddr>, now: Instant) -> Result<LoginAttempt<'_>, Duration> {
        let key = bucket(client);
        let mut entries = self.entries();
        if !entries.contains_key(&key) && entries.len() >= self.max_entries {
            Self::evict(&mut entries, self.max_entries, now);
        }
        let entry = entries.entry(key).or_insert_with(|| AttemptState::new(now));
        entry.last_seen = now;
        if let Some(until) = entry.locked_until {
            if until > now {
                return Err(until - now);
            }
            entry.locked_until = None;
        }
        if entry.last_failure.is_some_and(|last| now.saturating_duration_since(last) >= FAILURE_WINDOW) {
            entry.failures = 0;
        }
        if entry.last_failure.is_some_and(|last| now.saturating_duration_since(last) >= ENTRY_TTL) {
            entry.lockouts = 0;
        }
        if entry.failures.saturating_add(entry.in_flight) >= MAX_FAILURES {
            // The remaining budget is reserved by attempts still being verified.
            return Err(Duration::from_secs(1));
        }
        entry.in_flight += 1;
        Ok(LoginAttempt { limiter: self, key, finished: false })
    }

    fn evict(entries: &mut HashMap<RateLimitKey, AttemptState>, max_entries: usize, now: Instant) {
        entries.retain(|_, entry| !entry.is_idle(now, FAILURE_WINDOW));
        while entries.len() >= max_entries {
            let oldest = entries
                .iter()
                .filter(|(_, entry)| entry.in_flight == 0)
                .min_by_key(|(_, entry)| entry.last_seen)
                .map(|(key, _)| *key);
            match oldest {
                Some(key) => {
                    entries.remove(&key);
                }
                None => break,
            }
        }
    }

    /// Drops clients that have been quiet for a day and hold no lock.
    pub fn sweep(&self) {
        let now = Instant::now();
        self.entries().retain(|_, entry| !entry.is_idle(now, ENTRY_TTL));
    }

    fn finish(&self, key: RateLimitKey, success: bool, now: Instant) -> Option<Duration> {
        let mut entries = self.entries();
        let entry = entries.get_mut(&key)?;
        entry.in_flight = entry.in_flight.saturating_sub(1);
        entry.last_seen = now;
        if success {
            entry.failures = 0;
            entry.lockouts = 0;
            entry.locked_until = None;
            entry.last_failure = None;
            if entry.in_flight == 0 {
                entries.remove(&key);
            }
            return None;
        }
        entry.failures += 1;
        entry.last_failure = Some(now);
        if entry.failures >= MAX_FAILURES {
            entry.lockouts = entry.lockouts.saturating_add(1);
            let lockout = lockout_duration(entry.lockouts);
            entry.locked_until = Some(now + lockout);
            entry.failures = 0;
            return Some(lockout);
        }
        None
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries().len()
    }
}

/// A reserved attempt. Dropping it without `succeed`/`fail` releases the
/// reservation without counting a failure (for example when the client
/// disconnects before the result is known).
pub struct LoginAttempt<'a> {
    limiter: &'a LoginRateLimiter,
    key: RateLimitKey,
    finished: bool,
}

impl LoginAttempt<'_> {
    pub fn succeed(mut self) {
        self.finished = true;
        self.limiter.finish(self.key, true, Instant::now());
    }

    /// Records a failure; returns the lockout if this failure triggered one.
    pub fn fail(mut self) -> Option<Duration> {
        self.finished = true;
        self.limiter.finish(self.key, false, Instant::now())
    }

    #[cfg(test)]
    fn fail_at(mut self, now: Instant) -> Option<Duration> {
        self.finished = true;
        self.limiter.finish(self.key, false, now)
    }
}

impl Drop for LoginAttempt<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut entries = self.limiter.entries();
        if let Some(entry) = entries.get_mut(&self.key) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> Option<IpAddr> {
        Some(value.parse().unwrap())
    }

    #[test]
    fn locks_out_one_client_without_affecting_others() {
        let limiter = LoginRateLimiter::default();
        let now = Instant::now();
        for attempt in 1..=MAX_FAILURES {
            let lockout = limiter.try_begin_at(ip("203.0.113.7"), now).unwrap().fail_at(now);
            assert_eq!(lockout.is_some(), attempt == MAX_FAILURES);
        }
        let wait = limiter.try_begin_at(ip("203.0.113.7"), now).err().expect("client should be locked");
        assert_eq!(wait, Duration::from_secs(60));
        // The owner on another address can still log in.
        limiter.try_begin_at(ip("198.51.100.1"), now).unwrap().succeed();
    }

    #[test]
    fn lockouts_grow_exponentially_and_are_capped() {
        assert_eq!(lockout_duration(1), Duration::from_secs(60));
        assert_eq!(lockout_duration(2), Duration::from_secs(120));
        assert_eq!(lockout_duration(3), Duration::from_secs(240));
        assert_eq!(lockout_duration(10), MAX_LOCKOUT);
        assert_eq!(lockout_duration(u32::MAX), MAX_LOCKOUT);

        let limiter = LoginRateLimiter::default();
        let mut now = Instant::now();
        for expected in [60, 120, 240] {
            let mut lockout = None;
            for _ in 0..MAX_FAILURES {
                lockout = limiter.try_begin_at(ip("203.0.113.8"), now).unwrap().fail_at(now);
            }
            assert_eq!(lockout, Some(Duration::from_secs(expected)));
            now += Duration::from_secs(expected);
        }
    }

    #[test]
    fn concurrent_reservations_cannot_exceed_the_budget() {
        let limiter = LoginRateLimiter::default();
        let now = Instant::now();
        let reserved =
            (0..MAX_FAILURES).map(|_| limiter.try_begin_at(ip("203.0.113.9"), now).unwrap()).collect::<Vec<_>>();
        assert!(limiter.try_begin_at(ip("203.0.113.9"), now).is_err());
        drop(reserved);
        // Abandoned reservations are released without counting as failures.
        assert!(limiter.try_begin_at(ip("203.0.113.9"), now).is_ok());
    }

    #[test]
    fn success_resets_history_and_old_failures_decay() {
        let limiter = LoginRateLimiter::default();
        let now = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            limiter.try_begin_at(ip("203.0.113.10"), now).unwrap().fail_at(now);
        }
        limiter.try_begin_at(ip("203.0.113.10"), now).unwrap().succeed();
        assert_eq!(limiter.len(), 0);

        for _ in 0..MAX_FAILURES - 1 {
            limiter.try_begin_at(ip("203.0.113.10"), now).unwrap().fail_at(now);
        }
        let later = now + FAILURE_WINDOW;
        assert_eq!(limiter.try_begin_at(ip("203.0.113.10"), later).unwrap().fail_at(later), None);
    }

    #[test]
    fn ipv6_clients_share_a_slash_64_bucket() {
        let limiter = LoginRateLimiter::default();
        let now = Instant::now();
        for index in 0..MAX_FAILURES {
            let address = format!("2001:db8:1:2::{index:x}");
            limiter.try_begin_at(ip(&address), now).unwrap().fail_at(now);
        }
        assert!(limiter.try_begin_at(ip("2001:db8:1:2::ffff"), now).is_err());
        assert!(limiter.try_begin_at(ip("2001:db8:1:3::1"), now).is_ok());
        assert_eq!(bucket(ip("::ffff:192.0.2.1")), ip("192.0.2.1"));
    }

    #[test]
    fn map_is_bounded() {
        let limiter = LoginRateLimiter::with_capacity(3);
        let now = Instant::now();
        for index in 0..10u8 {
            let address = format!("192.0.2.{index}");
            limiter.try_begin_at(ip(&address), now + Duration::from_secs(u64::from(index))).unwrap().fail_at(now);
        }
        assert!(limiter.len() <= 3);
    }
}
