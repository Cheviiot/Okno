use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Failures allowed before any delay, to forgive typos.
const FREE_ATTEMPTS: u32 = 3;
const MAX_DELAY: Duration = Duration::from_secs(60);
/// A source is forgotten after this long without failures.
const RESET_AFTER: Duration = Duration::from_secs(15 * 60);
/// Password checks running at once; each one takes 64 MiB.
const CONCURRENT_LOGINS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleDecision {
    Allow,
    RetryAfter(Duration),
}

#[derive(Debug, Clone, Copy)]
struct Failures {
    count: u32,
    last: Instant,
}

impl Failures {
    fn delay(&self) -> Duration {
        if self.count < FREE_ATTEMPTS {
            return Duration::ZERO;
        }
        let exp = (self.count - FREE_ATTEMPTS).min(6);
        (Duration::from_secs(2) * 2u32.pow(exp)).min(MAX_DELAY)
    }
}

/// Per-source exponential backoff after failed logins, plus a cap on how many
/// password checks run in parallel.
pub struct LoginThrottle {
    failures: Mutex<HashMap<IpAddr, Failures>>,
    permits: Arc<Semaphore>,
}

/// Held while one password check runs.
pub struct LoginPermit(#[allow(dead_code)] OwnedSemaphorePermit);

impl Default for LoginThrottle {
    fn default() -> Self {
        Self { failures: Mutex::default(), permits: Arc::new(Semaphore::new(CONCURRENT_LOGINS)) }
    }
}

impl LoginThrottle {
    pub fn check(&self, source: IpAddr, now: Instant) -> ThrottleDecision {
        let mut map = self.failures.lock().unwrap();
        map.retain(|_, f| now.duration_since(f.last) < RESET_AFTER);
        let Some(f) = map.get(&key(source)) else {
            return ThrottleDecision::Allow;
        };
        let ready_at = f.last + f.delay();
        if now >= ready_at { ThrottleDecision::Allow } else { ThrottleDecision::RetryAfter(ready_at - now) }
    }

    /// Returns `None` when too many checks are already running.
    pub fn try_begin(&self) -> Option<LoginPermit> {
        self.permits.clone().try_acquire_owned().ok().map(LoginPermit)
    }

    /// Records a failure and returns the delay before the next attempt.
    pub fn record_failure(&self, source: IpAddr, now: Instant) -> Duration {
        let mut map = self.failures.lock().unwrap();
        let entry = map.entry(key(source)).or_insert(Failures { count: 0, last: now });
        entry.count = entry.count.saturating_add(1);
        entry.last = now;
        entry.delay()
    }

    pub fn record_success(&self, source: IpAddr) {
        self.failures.lock().unwrap().remove(&key(source));
    }
}

/// IPv6 clients can rotate addresses within their /64, so count them per
/// prefix.
fn key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let mut seg = v6.segments();
            seg[4..].fill(0);
            IpAddr::V6(seg.into())
        }
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 5));

    #[test]
    fn backoff_grows_and_caps() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..FREE_ATTEMPTS - 1 {
            assert_eq!(t.record_failure(IP, now), Duration::ZERO);
        }
        assert_eq!(t.record_failure(IP, now), Duration::from_secs(2));
        assert_eq!(t.record_failure(IP, now), Duration::from_secs(4));
        assert_eq!(t.check(IP, now + Duration::from_secs(1)), ThrottleDecision::RetryAfter(Duration::from_secs(3)));
        assert_eq!(t.check(IP, now + Duration::from_secs(4)), ThrottleDecision::Allow);
        for _ in 0..20 {
            t.record_failure(IP, now);
        }
        assert_eq!(t.record_failure(IP, now), MAX_DELAY);
    }

    #[test]
    fn success_and_idle_time_reset() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..10 {
            t.record_failure(IP, now);
        }
        assert_ne!(t.check(IP, now), ThrottleDecision::Allow);
        assert_eq!(t.check(IP, now + RESET_AFTER), ThrottleDecision::Allow);
        t.record_failure(IP, now);
        t.record_success(IP);
        assert_eq!(t.check(IP, now), ThrottleDecision::Allow);
    }

    #[test]
    fn ipv6_counts_per_prefix() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for i in 0..10u16 {
            t.record_failure(format!("fd00::{i:x}").parse().unwrap(), now);
        }
        assert_ne!(t.check("fd00::ffff".parse().unwrap(), now), ThrottleDecision::Allow);
        assert_eq!(t.check("fd00:0:0:1::1".parse().unwrap(), now), ThrottleDecision::Allow);
    }

    #[test]
    fn limits_parallel_checks() {
        let t = LoginThrottle::default();
        let held: Vec<_> = (0..CONCURRENT_LOGINS).map(|_| t.try_begin().unwrap()).collect();
        assert!(t.try_begin().is_none());
        drop(held);
        assert!(t.try_begin().is_some());
    }
}
