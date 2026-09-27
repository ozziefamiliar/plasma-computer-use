//! Action rate limiter: a token bucket between the model and the backends.
//!
//! The executor is the only thing between a looping model and the input
//! devices. Screenshots self-throttle (450ms settle), but presses, types,
//! and moves do not — a model stuck in a retry loop can issue hundreds of
//! actions per second. The limiter caps *sustained* action throughput while
//! allowing a burst, so the worst case is a nuisance, not a lockup.
//!
//! Design (see `references/rate-limit-design.md`):
//!
//! - One token per action that passes the guard review (denied actions touch
//!   no backend and cost nothing). Uniform cost = trivial mental model.
//! - When the bucket is empty, the current and remaining actions drain as
//!   [`crate::ExecError::RateLimited`] — partial-drain shape, like cancel.
//! - `RateLimited` is never retried inside the executor: the model must see
//!   the limit (with a `retry_after` hint) to adapt its loop. Transparent
//!   sleeping would turn clickspam into a slow clickstorm.

use std::time::{Duration, Instant};

/// Rate-limit configuration. Applied by the executor at every action
/// boundary; a fresh limiter starts with a full bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimit {
    /// Burst size: how many actions may fire immediately.
    pub capacity: u32,
    /// Sustained rate: tokens refill this many per second.
    pub refill_per_sec: f64,
}

impl RateLimit {
    /// The default safety posture: 20-action bursts, 5 actions/s sustained.
    /// A healthy vision loop (screenshot + 1–2 actions per call) runs at
    /// ~2–3 batches/s and never notices the cap; a clickspam loop at 100/s
    /// burns the burst in ~0.2s and then runs at 5/s.
    pub fn default() -> Self {
        Self {
            capacity: 20,
            refill_per_sec: 5.0,
        }
    }
}

impl Default for RateLimit {
    fn default() -> Self {
        Self::default()
    }
}

/// A token bucket over wall-clock time. Not thread-safe on its own — the
/// executor owns one and `execute()` takes `&mut self`.
#[derive(Debug)]
pub struct RateLimiter {
    config: RateLimit,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// New limiter with a full bucket. `now` is the current clock reading
    /// (the executor passes `clock.now()`).
    pub fn new(config: RateLimit, now: Instant) -> Self {
        Self {
            tokens: config.capacity as f64,
            last: now,
            config,
        }
    }

    /// Swap the configuration; the bucket restarts full.
    pub fn reconfigure(&mut self, config: RateLimit, now: Instant) {
        self.config = config;
        self.tokens = config.capacity as f64;
        self.last = now;
    }

    /// Try to consume one token for an action.
    ///
    /// `Ok(())` if an action may run. `Err(retry_after)`: the bucket is
    /// empty and the action must not run; `retry_after` is `None` when no
    /// refill is configured (the bucket is frozen), otherwise the time until
    /// one token is available.
    pub fn try_acquire(&mut self, now: Instant) -> Result<(), Option<Duration>> {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 && self.config.refill_per_sec > 0.0 {
            self.tokens = (self.tokens + elapsed * self.config.refill_per_sec)
                .min(self.config.capacity as f64);
        }
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            let retry_after = if self.config.refill_per_sec > 0.0 {
                let deficit = 1.0 - self.tokens;
                Some(Duration::from_secs_f64(deficit / self.config.refill_per_sec))
            } else {
                None
            };
            Err(retry_after)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A clock the test advances by hand.
    struct ManualClock {
        now: Cell<Instant>,
    }

    impl ManualClock {
        fn new() -> Self {
            Self {
                now: Cell::new(Instant::now()),
            }
        }
        fn advance(&self, d: Duration) {
            let t = self.now.get() + d;
            self.now.set(t);
        }
        fn now(&self) -> Instant {
            self.now.get()
        }
    }

    fn full_bucket() -> (RateLimiter, ManualClock) {
        let clock = ManualClock::new();
        let limiter = RateLimiter::new(RateLimit::default(), clock.now());
        (limiter, clock)
    }

    #[test]
    fn burst_of_capacity_all_acquires() {
        let (mut lim, clock) = full_bucket();
        for _ in 0..20 {
            assert!(lim.try_acquire(clock.now()).is_ok());
        }
        // Bucket empty: 21st action is limited.
        assert!(lim.try_acquire(clock.now()).is_err());
    }

    #[test]
    fn refill_is_linear_and_clamps_at_capacity() {
        let (mut lim, clock) = full_bucket();
        for _ in 0..20 {
            lim.try_acquire(clock.now()).unwrap();
        }
        clock.advance(Duration::from_secs(2)); // +10 tokens at 5/s
        let mut ok = 0;
        while lim.try_acquire(clock.now()).is_ok() {
            ok += 1;
        }
        assert_eq!(ok, 10);
        // Long idle: bucket never exceeds capacity.
        clock.advance(Duration::from_secs(3600));
        let mut ok = 0;
        while lim.try_acquire(clock.now()).is_ok() {
            ok += 1;
        }
        assert_eq!(ok, 20);
    }

    #[test]
    fn retry_after_reflects_deficit_and_rate() {
        let (mut lim, clock) = full_bucket();
        for _ in 0..20 {
            lim.try_acquire(clock.now()).unwrap();
        }
        match lim.try_acquire(clock.now()) {
            Err(Some(wait)) => assert!((wait.as_secs_f64() - 0.2).abs() < 1e-6),
            other => panic!("expected a retry_after of 0.2s, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn zero_refill_is_frozen_with_unknown_retry_after() {
        let clock = ManualClock::new();
        let mut lim = RateLimiter::new(
            RateLimit {
                capacity: 2,
                refill_per_sec: 0.0,
            },
            clock.now(),
        );
        assert!(lim.try_acquire(clock.now()).is_ok());
        assert!(lim.try_acquire(clock.now()).is_ok());
        clock.advance(Duration::from_secs(3600));
        assert_eq!(lim.try_acquire(clock.now()), Err(None));
    }

    #[test]
    fn reconfigure_restarts_with_full_bucket() {
        let (mut lim, clock) = full_bucket();
        for _ in 0..20 {
            lim.try_acquire(clock.now()).unwrap();
        }
        lim.reconfigure(
            RateLimit {
                capacity: 3,
                refill_per_sec: 100.0,
            },
            clock.now(),
        );
        for _ in 0..3 {
            assert!(lim.try_acquire(clock.now()).is_ok());
        }
        assert!(lim.try_acquire(clock.now()).is_err());
    }
}
