//! Rate limiting and single-flight (DESIGN §5.4).
//!
//! Two separate concerns that both exist to keep us welcome at somebody
//! else's API: a token bucket bounds how fast we ask, and single-flight stops
//! us asking the same question twice at once. Neither ever fails a request —
//! they delay it or fold it into one already running.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

/// A leaky token bucket.
///
/// `tokio::time::Instant` rather than `std::time::Instant` so tests can drive
/// the clock instead of sleeping through a real minute.
pub struct Bucket {
    state: Mutex<State>,
}

struct State {
    tokens: f64,
    capacity: f64,
    per_sec: f64,
    last: Instant,
    /// Set by a 429. Pauses the whole bucket, not just the caller that was
    /// refused — the server is telling us about our IP, not about that one
    /// request (DESIGN §5.4).
    paused_until: Option<Instant>,
}

impl Bucket {
    /// A bucket refilling at `n` tokens per minute.
    ///
    /// The bucket starts full: a device that has just booted, or has been
    /// silent all afternoon, should not be made to wait for its first album.
    pub fn per_minute(n: u32) -> Bucket {
        Bucket::new(f64::from(n.max(1)) / 60.0, f64::from(n.max(1)))
    }

    /// A bucket refilling at `n` tokens per second, capacity `n`.
    ///
    /// MusicBrainz's published limit is one request per second sustained, and
    /// unlike Apple they enforce it per user-agent. Capacity is deliberately
    /// not larger than the rate: there is no burst allowance to spend.
    pub fn per_second(n: f64) -> Bucket {
        Bucket::new(n, n)
    }

    fn new(per_sec: f64, capacity: f64) -> Bucket {
        Bucket {
            state: Mutex::new(State {
                tokens: capacity,
                capacity,
                per_sec,
                last: Instant::now(),
                paused_until: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("rate limiter mutex poisoned")
    }

    /// Take a token, waiting if necessary. Returns how long we slept.
    ///
    /// Time actually slept, not wall time elapsed: the caller treats a
    /// non-zero return as a rate-limiting event for the diagnostics page, and
    /// two `Instant::now()` calls either side of an uncontended acquire are
    /// always a few nanoseconds apart — which would report every request as
    /// rate limited.
    pub async fn acquire(&self) -> Duration {
        let mut slept = Duration::ZERO;
        loop {
            let wait = {
                let mut s = self.lock();
                s.refill();
                s.take()
            };
            match wait {
                None => return slept,
                // Re-check after sleeping rather than assuming the token is
                // ours: another worker may have taken it meanwhile.
                Some(d) => {
                    tokio::time::sleep(d).await;
                    slept += d;
                }
            }
        }
    }

    /// Stop issuing tokens for `d`, after a 429.
    pub fn pause(&self, d: Duration) {
        let mut s = self.lock();
        let until = Instant::now() + d;
        // Never shorten an existing pause: two 429s in flight must not let
        // the second one's shorter Retry-After undo the first.
        if s.paused_until.is_none_or(|cur| until > cur) {
            s.paused_until = Some(until);
        }
    }

    /// Whether the bucket is currently paused by a 429.
    pub fn is_paused(&self) -> bool {
        self.lock().paused_until.is_some_and(|t| t > Instant::now())
    }
}

impl State {
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.per_sec).min(self.capacity);
    }

    /// `None` if a token was taken, otherwise how long to wait before retrying.
    fn take(&mut self) -> Option<Duration> {
        if let Some(until) = self.paused_until {
            let now = Instant::now();
            if until > now {
                return Some(until - now);
            }
            self.paused_until = None;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return None;
        }
        let deficit = 1.0 - self.tokens;
        Some(Duration::from_secs_f64(deficit / self.per_sec))
    }
}

/// One in-flight operation per key.
///
/// A repeated track — the same album's bundle resent, or the next track off
/// the same record — must not issue a second identical search.
#[derive(Default, Clone)]
pub struct SingleFlight {
    keys: Arc<Mutex<HashSet<String>>>,
}

impl SingleFlight {
    /// Claim `key`, or `None` if somebody already holds it.
    pub fn enter(&self, key: &str) -> Option<Flight> {
        let mut g = self.keys.lock().expect("single-flight mutex poisoned");
        if !g.insert(key.to_string()) {
            return None;
        }
        Some(Flight {
            keys: self.keys.clone(),
            key: key.to_string(),
        })
    }

    pub fn in_flight(&self) -> usize {
        self.keys
            .lock()
            .expect("single-flight mutex poisoned")
            .len()
    }
}

/// Releases its key on drop, including on panic or early return — which is
/// the point of doing it this way rather than with an explicit release call.
pub struct Flight {
    keys: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl Drop for Flight {
    fn drop(&mut self) {
        if let Ok(mut g) = self.keys.lock() {
            g.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_full_bucket_does_not_delay_the_first_requests() {
        let b = Bucket::per_minute(15);
        for i in 0..15 {
            assert_eq!(b.acquire().await, Duration::ZERO, "request {i} waited");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_bucket_paces_requests_at_the_configured_rate() {
        // 60/min is one per second, which makes the arithmetic checkable.
        let b = Bucket::per_minute(60);
        for _ in 0..60 {
            b.acquire().await;
        }
        let waited = b.acquire().await;
        assert!(
            (waited.as_millis() as i64 - 1_000).abs() < 50,
            "waited {waited:?}, expected about a second"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_429_pauses_the_whole_bucket_for_retry_after() {
        let b = Bucket::per_minute(15);
        b.acquire().await;
        b.pause(Duration::from_secs(30));
        assert!(b.is_paused());

        let waited = b.acquire().await;
        assert!(
            waited >= Duration::from_secs(29),
            "the pause was not honoured: waited {waited:?}"
        );
        assert!(!b.is_paused());
    }

    #[tokio::test(start_paused = true)]
    async fn a_longer_pause_is_never_shortened_by_a_later_one() {
        let b = Bucket::per_minute(15);
        b.pause(Duration::from_secs(120));
        b.pause(Duration::from_secs(5));
        let waited = b.acquire().await;
        assert!(waited >= Duration::from_secs(119), "waited {waited:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn tokens_come_back_over_time() {
        let b = Bucket::per_second(1.0);
        b.acquire().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        // Capacity equals the rate, so at most one token is ever banked.
        assert_eq!(b.acquire().await, Duration::ZERO);
        assert!(b.acquire().await >= Duration::from_millis(900));
    }

    #[test]
    fn a_key_can_only_be_claimed_once_at_a_time() {
        let sf = SingleFlight::default();
        let first = sf.enter("mezzanine").expect("first claim");
        assert!(sf.enter("mezzanine").is_none(), "claimed twice");
        let other = sf.enter("kid a").expect("a different key was blocked");
        assert_eq!(sf.in_flight(), 2);
        drop(other);

        drop(first);
        assert!(
            sf.enter("mezzanine").is_some(),
            "the key was never released"
        );
    }

    #[test]
    fn a_panicking_holder_still_releases_its_key() {
        let sf = SingleFlight::default();
        let sf2 = sf.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _flight = sf2.enter("k").unwrap();
            panic!("work failed");
        }));
        assert!(sf.enter("k").is_some(), "a panic leaked the key forever");
    }
}
