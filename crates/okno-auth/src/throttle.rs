use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Failures allowed before any delay, to forgive typos.
const FREE_ATTEMPTS: u32 = 3;
const MAX_DELAY: Duration = Duration::from_secs(60);
/// Failures from all sources together before every attempt is spaced out;
/// stops a guesser who keeps changing addresses.
const GLOBAL_FREE_ATTEMPTS: u32 = 30;
const GLOBAL_MAX_DELAY: Duration = Duration::from_secs(30);
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
    fn delay(&self, free: u32, first: Duration, max: Duration) -> Duration {
        if self.count < free {
            return Duration::ZERO;
        }
        let exp = (self.count - free).min(6);
        (first * 2u32.pow(exp)).min(max)
    }

    fn source_delay(&self) -> Duration {
        self.delay(FREE_ATTEMPTS, Duration::from_secs(2), MAX_DELAY)
    }

    fn global_delay(&self) -> Duration {
        self.delay(GLOBAL_FREE_ATTEMPTS, Duration::from_secs(1), GLOBAL_MAX_DELAY)
    }

    fn wait(&self, delay: Duration, now: Instant) -> Option<Duration> {
        let ready_at = self.last + delay;
        (now < ready_at).then(|| ready_at - now)
    }

    fn bump(&mut self, now: Instant) {
        self.count = self.count.saturating_add(1);
        self.last = now;
    }
}

#[derive(Default)]
struct State {
    sources: HashMap<IpAddr, Failures>,
    global: Option<Failures>,
}

impl State {
    fn forget_idle(&mut self, now: Instant) {
        self.sources.retain(|_, f| now.duration_since(f.last) < RESET_AFTER);
        if self.global.is_some_and(|g| now.duration_since(g.last) >= RESET_AFTER) {
            self.global = None;
        }
    }
}

/// Exponential backoff after failed logins, per source and over all
/// sources, plus a cap on how many password checks run in parallel.
///
/// An attempt counts as a failure from the moment it is admitted
/// ([`attempt`](Self::attempt)) until it succeeds, so parallel connections
/// from one source cannot all slip through the same opening.
pub struct LoginThrottle {
    state: Mutex<State>,
    permits: Arc<Semaphore>,
}

/// Held while one password check runs.
pub struct LoginPermit(#[allow(dead_code)] OwnedSemaphorePermit);

impl Default for LoginThrottle {
    fn default() -> Self {
        Self { state: Mutex::default(), permits: Arc::new(Semaphore::new(CONCURRENT_LOGINS)) }
    }
}

impl LoginThrottle {
    /// Admits an attempt from `source` and counts it as failed until
    /// [`record_success`](Self::record_success).
    pub fn attempt(&self, source: IpAddr, now: Instant) -> ThrottleDecision {
        let mut state = self.state.lock().unwrap();
        state.forget_idle(now);
        let source_wait = state.sources.get(&key(source)).and_then(|f| f.wait(f.source_delay(), now));
        let global_wait = state.global.and_then(|g| g.wait(g.global_delay(), now));
        if let Some(wait) = source_wait.max(global_wait) {
            return ThrottleDecision::RetryAfter(wait);
        }
        state.sources.entry(key(source)).or_insert(Failures { count: 0, last: now }).bump(now);
        state.global.get_or_insert(Failures { count: 0, last: now }).bump(now);
        ThrottleDecision::Allow
    }

    /// How long `source` has to wait before its next attempt.
    pub fn retry_after(&self, source: IpAddr, now: Instant) -> Duration {
        let state = self.state.lock().unwrap();
        let source_wait = state.sources.get(&key(source)).map(|f| f.source_delay());
        let global_wait = state.global.and_then(|g| g.wait(g.global_delay(), now));
        source_wait.max(global_wait).unwrap_or_default()
    }

    /// Returns `None` when too many checks are already running.
    pub fn try_begin(&self) -> Option<LoginPermit> {
        self.permits.clone().try_acquire_owned().ok().map(LoginPermit)
    }

    /// Forgives `source` and takes its admitted attempt back from the
    /// global count.
    pub fn record_success(&self, source: IpAddr) {
        let mut state = self.state.lock().unwrap();
        state.sources.remove(&key(source));
        if let Some(global) = state.global.as_mut() {
            global.count = global.count.saturating_sub(1);
        }
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

    fn fail(t: &LoginThrottle, ip: IpAddr, now: Instant) -> Duration {
        assert_eq!(t.attempt(ip, now), ThrottleDecision::Allow);
        t.retry_after(ip, now)
    }

    #[test]
    fn backoff_grows_and_caps() {
        let t = LoginThrottle::default();
        let mut now = Instant::now();
        for _ in 0..FREE_ATTEMPTS - 1 {
            assert_eq!(fail(&t, IP, now), Duration::ZERO);
        }
        assert_eq!(fail(&t, IP, now), Duration::from_secs(2));
        assert_eq!(t.attempt(IP, now + Duration::from_secs(1)), ThrottleDecision::RetryAfter(Duration::from_secs(1)));
        now += Duration::from_secs(2);
        assert_eq!(fail(&t, IP, now), Duration::from_secs(4));
        for _ in 0..20 {
            now += MAX_DELAY;
            fail(&t, IP, now);
        }
        assert_eq!(t.retry_after(IP, now), MAX_DELAY);
    }

    #[test]
    fn parallel_attempts_share_one_opening() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..FREE_ATTEMPTS {
            fail(&t, IP, now);
        }
        let later = now + Duration::from_secs(2);
        assert_eq!(t.attempt(IP, later), ThrottleDecision::Allow);
        assert!(matches!(t.attempt(IP, later), ThrottleDecision::RetryAfter(_)));
    }

    #[test]
    fn success_and_idle_time_reset() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..FREE_ATTEMPTS + 1 {
            t.attempt(IP, now);
        }
        assert_ne!(t.attempt(IP, now), ThrottleDecision::Allow);
        assert_eq!(t.attempt(IP, now + RESET_AFTER), ThrottleDecision::Allow);
        t.record_success(IP);
        assert_eq!(t.attempt(IP, now + RESET_AFTER), ThrottleDecision::Allow);
    }

    #[test]
    fn ipv6_counts_per_prefix() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for i in 0..FREE_ATTEMPTS as u16 {
            fail(&t, format!("fd00::{i:x}").parse().unwrap(), now);
        }
        assert_ne!(t.attempt("fd00::ffff".parse().unwrap(), now), ThrottleDecision::Allow);
        assert_eq!(t.attempt("fd00:0:0:1::1".parse().unwrap(), now), ThrottleDecision::Allow);
    }

    #[test]
    fn many_sources_hit_the_global_limit() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for i in 0..GLOBAL_FREE_ATTEMPTS {
            let ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, (i / 250) as u8, (i % 250) as u8));
            assert_eq!(t.attempt(ip, now), ThrottleDecision::Allow);
        }
        let fresh = IpAddr::V4(std::net::Ipv4Addr::new(10, 1, 0, 1));
        assert_eq!(t.attempt(fresh, now), ThrottleDecision::RetryAfter(Duration::from_secs(1)));
        assert_eq!(t.attempt(fresh, now + Duration::from_secs(1)), ThrottleDecision::Allow);
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
