//! Password and session handling for the web interface (DESIGN §7.3).
//!
//! # What this defends against
//!
//! Other people and other devices on the same network. That is the whole
//! claim. The page exposes listening history and every device setting, and
//! the alternative — an unauthenticated page on the LAN — hands both to
//! anything that can reach port 8730, including a guest's phone and anything
//! already compromised on the network.
//!
//! # What it deliberately does not defend against
//!
//! * **Anyone who can watch the wire.** This is plain HTTP. The password
//!   crosses the LAN in the clear on every login, and the session cookie
//!   crosses it on every request. Someone on the same Wi-Fi with a packet
//!   capture, or upstream of the device, gets both. If that is in the threat
//!   model, bind to loopback and use an SSH tunnel, or put a
//!   TLS-terminating reverse proxy in front. We do not ship self-signed TLS:
//!   it trains people to click through certificate warnings and buys nothing
//!   against the attacker who is actually being described here.
//! * **Anyone with the device in their hands, or a shell on it.** The
//!   generated password is in a file on the device, by design, because the
//!   alternative is a device nobody can log into.
//! * **The internet.** There is no WAN mode. Port-forwarding this exposes a
//!   plain-HTTP login to the world and no amount of Argon2 makes that a good
//!   idea.
//! * **Offline cracking of a stolen hash.** Argon2id at the OWASP minimum
//!   makes it expensive rather than impossible. A 100-bit generated
//!   passphrase is what actually carries that; a user who replaces it with
//!   `password` is on their own.
//! * **An event stream that outlives its session.** `/api/events` is
//!   authorised when it is opened and not again while it runs, so logging out
//!   does not cut a stream already in flight. It carries only what Now
//!   Playing shows, and it dies with the tab or the daemon.
//!
//! # Choices worth defending
//!
//! * **Server-side sessions, not signed cookies.** A set held in memory is
//!   simpler to reason about — there is no signing key to leak, no clock
//!   skew, no algorithm confusion — and it means every session dies when the
//!   daemon restarts, which is the behaviour you want after changing
//!   anything security-relevant.
//! * **Constant-time comparison** of the presented token against each live
//!   session, so the response time says nothing about how much of a guessed
//!   token was right. The password itself is compared by Argon2, which is
//!   constant-time by construction.
//! * **Rate limiting on address, not on account.** There is one account. The
//!   limit exists to make online guessing hopeless against a 100-bit
//!   passphrase.
//! * **A hard cap on verifications in flight, independently of the address
//!   limit.** Per-address limiting says nothing about how much of the device
//!   an attacker can consume, because on a LAN an attacker has as many
//!   addresses as they want — a single host owns a whole IPv6 /64. Without a
//!   global cap, `spawn_blocking` would happily run Argon2 on all 512 of
//!   tokio's blocking threads, which at 19 MiB each is 9.7 GiB and a Pi that
//!   is dead rather than slow. [`Auth::verify_slot`] is what actually bounds
//!   the memory; the address limit sits on top of it.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::rngs::OsRng;
use rand::RngCore;
use subtle::ConstantTimeEq;

/// OWASP's minimum Argon2id parameters: 19 MiB, two passes, one lane.
///
/// Memory is the expensive dimension for an attacker with GPUs and the cheap
/// one for us — a Pi 4 has this to spare, and the daemon does at most five of
/// these a minute.
const MEMORY_KIB: u32 = 19 * 1024;
const ITERATIONS: u32 = 2;
const PARALLELISM: u32 = 1;

/// Failed logins allowed per client address per [`LOGIN_WINDOW`].
const LOGIN_ATTEMPTS: usize = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);

/// Password verifications permitted to run at once, across all addresses.
///
/// Two, because two is enough that the owner never waits behind anyone and
/// small enough that the worst case is 38 MiB — see the module header for why
/// the per-address limit does not cover this.
const MAX_VERIFICATIONS_IN_FLIGHT: usize = 2;

/// How long a login waits for a verification slot before giving up.
///
/// Long enough that a queue of legitimate logins all succeed, short enough
/// that a flood does not hold connections open indefinitely.
const VERIFY_WAIT: Duration = Duration::from_secs(5);

/// Addresses tracked for rate limiting at once.
///
/// Entries expire with [`LOGIN_WINDOW`], and [`MAX_VERIFICATIONS_IN_FLIGHT`]
/// bounds how fast new ones can be created, so this is a backstop rather than
/// a limit anything reaches in practice.
const MAX_TRACKED_ADDRESSES: usize = 4096;

/// Above this many entries, sweep the whole map before adding another. Below
/// it, the map is small enough that the per-entry prune is the only work
/// worth doing.
const SWEEP_THRESHOLD: usize = 256;

/// How long a session lives, matching the cookie's expiry.
pub const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 24 * 3600);

/// Live sessions kept. Enough for a phone, a laptop and a tablet several
/// times over; the cap exists so the constant-time scan stays bounded and a
/// login loop cannot grow the set without limit.
const MAX_SESSIONS: usize = 16;

/// The file the generated passphrase is written to, beside the local config.
pub const PASSWORD_FILE_NAME: &str = "web-password.txt";

/// Characters that survive being read off a terminal and typed on a phone:
/// no `l`, `o`, `0` or `1`. Exactly 32 of them, which is what makes the
/// mapping below unbiased.
const ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";

fn argon2() -> Argon2<'static> {
    let params = Params::new(MEMORY_KIB, ITERATIONS, PARALLELISM, None)
        .expect("the OWASP minimum parameters are in range");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a password for storage. Returns a PHC string, salt included.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = argon2()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing the web password: {e}"))?;
    Ok(hash.to_string())
}

/// Verify a password against a stored PHC string.
///
/// Costs about 19 MiB and tens of milliseconds by design, so callers run it
/// off the async runtime.
pub fn verify_password(stored: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        // An unparseable hash is a broken config, not a reason to let anyone
        // in. It is logged where it is written, not on every attempt.
        return false;
    };
    argon2()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// A fresh passphrase: 100 bits from the operating system's entropy source,
/// grouped for reading aloud.
pub fn generate_passphrase() -> String {
    let mut bytes = [0u8; 20];
    OsRng.fill_bytes(&mut bytes);
    let chars: Vec<u8> = bytes
        .iter()
        // 32 divides 256 exactly, so this is uniform. With any other
        // alphabet length it would quietly favour the first few characters.
        .map(|b| ALPHABET[(b % 32) as usize])
        .collect();
    chars
        .chunks(5)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Write the generated passphrase where `lpctl web-password` can reprint it.
///
/// Mode 0600 and nothing else in the file: it is the one place on the device
/// the plaintext exists.
pub fn write_password_file(dir: &Path, passphrase: &str) -> Result<std::path::PathBuf> {
    let path = dir.join(PASSWORD_FILE_NAME);
    lpframe_config::write_atomic(&path, format!("{passphrase}\n").as_bytes(), 0o600)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

struct Session {
    token: String,
    expires: Instant,
}

/// The password and the live sessions.
pub struct Auth {
    hash: String,
    sessions: Mutex<Vec<Session>>,
    /// Recent failed attempts per client address.
    attempts: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    /// Permits for [`Auth::verify`], bounding Argon2's memory across every
    /// client at once.
    verifiers: tokio::sync::Semaphore,
}

impl Auth {
    pub fn new(hash: String) -> Auth {
        Auth {
            hash,
            sessions: Mutex::new(Vec::new()),
            attempts: Mutex::new(HashMap::new()),
            verifiers: tokio::sync::Semaphore::new(MAX_VERIFICATIONS_IN_FLIGHT),
        }
    }

    /// Whether this address is allowed another attempt, recording it.
    ///
    /// The address is the peer of the TCP connection. `X-Forwarded-For` is
    /// deliberately not consulted: it is attacker-controlled unless something
    /// trusted is stripping it, and trusting it here would turn the rate
    /// limit into a header the attacker sets. Behind a reverse proxy every
    /// client therefore shares one bucket, which is the safe direction to be
    /// wrong in.
    pub fn note_attempt(&self, from: IpAddr, now: Instant) -> bool {
        let mut attempts = self.attempts.lock().expect("auth mutex poisoned");

        // An address that has not been seen before costs an entry, and the
        // supply of addresses is unlimited on a LAN. Drop everything that has
        // aged out before letting the map grow again.
        if attempts.len() > SWEEP_THRESHOLD && !attempts.contains_key(&from) {
            attempts.retain(|_, seen| {
                seen.retain(|t| now.duration_since(*t) < LOGIN_WINDOW);
                !seen.is_empty()
            });
            if attempts.len() >= MAX_TRACKED_ADDRESSES {
                // Only reachable while thousands of distinct addresses are
                // mid-flood. Refusing the newcomer is the conservative end:
                // an address already in the map keeps its own budget.
                return false;
            }
        }

        let recent = attempts.entry(from).or_default();
        recent.retain(|t| now.duration_since(*t) < LOGIN_WINDOW);
        if recent.len() >= LOGIN_ATTEMPTS {
            return false;
        }
        recent.push(now);
        true
    }

    /// Wait for permission to run one [`Auth::verify`].
    ///
    /// `None` means the device is already hashing as much as it will, and the
    /// caller should refuse rather than queue further. Holding the permit
    /// across the blocking call is what makes it mean anything.
    pub async fn verify_slot(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        tokio::time::timeout(VERIFY_WAIT, self.verifiers.acquire())
            .await
            .ok()?
            .ok()
    }

    /// Forget an address's failures, so a legitimate user who mistyped twice
    /// is not locked out of their own device for the rest of the minute.
    pub fn clear_attempts(&self, from: IpAddr) {
        self.attempts
            .lock()
            .expect("auth mutex poisoned")
            .remove(&from);
    }

    /// Check a password. Blocking and deliberately slow — call it off the
    /// async runtime.
    pub fn verify(&self, password: &str) -> bool {
        verify_password(&self.hash, password)
    }

    /// Mint a session and return its token.
    pub fn issue(&self, now: Instant) -> String {
        let token = generate_token();
        let mut sessions = self.sessions.lock().expect("auth mutex poisoned");
        sessions.retain(|s| s.expires > now);
        if sessions.len() >= MAX_SESSIONS {
            sessions.remove(0);
        }
        sessions.push(Session {
            token: token.clone(),
            expires: now + SESSION_LIFETIME,
        });
        token
    }

    /// Whether this token names a live session.
    pub fn validate(&self, token: &str, now: Instant) -> bool {
        let mut sessions = self.sessions.lock().expect("auth mutex poisoned");
        sessions.retain(|s| s.expires > now);
        // Every live session is compared, and the comparison itself is
        // constant-time, so neither the timing nor the loop count depends on
        // how much of the presented token was correct.
        let mut found = subtle::Choice::from(0u8);
        for session in sessions.iter() {
            found |= session.token.as_bytes().ct_eq(token.as_bytes());
        }
        bool::from(found)
    }

    /// Drop a session. Constant-time for the same reason as [`Auth::validate`].
    pub fn revoke(&self, token: &str) {
        let mut sessions = self.sessions.lock().expect("auth mutex poisoned");
        sessions.retain(|s| !bool::from(s.token.as_bytes().ct_eq(token.as_bytes())));
    }

    #[cfg(test)]
    fn session_count(&self) -> usize {
        self.sessions.lock().expect("auth mutex poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_verifies_its_own_password_and_nothing_else() {
        let hash = hash_password("correct horse").unwrap();
        assert!(verify_password(&hash, "correct horse"));
        assert!(!verify_password(&hash, "correct horse "));
        assert!(!verify_password(&hash, ""));
    }

    #[test]
    fn the_stored_hash_is_argon2id_at_the_owasp_minimum() {
        // Read back off the PHC string rather than off the constants, so a
        // change to the parameters cannot pass by editing the test's inputs.
        let hash = hash_password("x").unwrap();
        let parsed = PasswordHash::new(&hash).unwrap();
        assert_eq!(parsed.algorithm.as_str(), "argon2id");
        let params: HashMap<_, _> = parsed
            .params
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_string()))
            .collect();
        assert_eq!(params.get("m").map(String::as_str), Some("19456"));
        assert_eq!(params.get("t").map(String::as_str), Some("2"));
        assert_eq!(params.get("p").map(String::as_str), Some("1"));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        // A per-hash salt, which is what stops one precomputation covering
        // every device the installer ever set up.
        assert_ne!(
            hash_password("same").unwrap(),
            hash_password("same").unwrap()
        );
    }

    #[test]
    fn a_generated_passphrase_is_unpredictable_and_readable() {
        let a = generate_passphrase();
        assert_eq!(a.len(), 23, "four groups of five and three separators");
        assert!(a.chars().all(|c| c == '-' || ALPHABET.contains(&(c as u8))));
        // No l/o/0/1, which is the whole point of the alphabet.
        assert!(!a.contains(['l', 'o', '0', '1']));

        let many: std::collections::HashSet<String> =
            (0..64).map(|_| generate_passphrase()).collect();
        assert_eq!(many.len(), 64, "the generator repeated itself");
    }

    #[test]
    fn a_session_token_is_thirty_two_bytes_of_hex() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, generate_token());
    }

    #[test]
    fn only_an_issued_token_validates() {
        let auth = Auth::new(hash_password("pw").unwrap());
        let now = Instant::now();
        let token = auth.issue(now);
        assert!(auth.validate(&token, now));

        assert!(!auth.validate("", now));
        assert!(!auth.validate(&"0".repeat(64), now));
        // A prefix of a real token must not pass, however long.
        assert!(!auth.validate(&token[..63], now));
    }

    #[test]
    fn a_session_expires_and_is_forgotten() {
        let auth = Auth::new(hash_password("pw").unwrap());
        let now = Instant::now();
        let token = auth.issue(now);
        let later = now + SESSION_LIFETIME + Duration::from_secs(1);
        assert!(!auth.validate(&token, later));
        assert_eq!(auth.session_count(), 0, "the expired session was retained");
    }

    #[test]
    fn revoking_a_session_ends_it() {
        let auth = Auth::new(hash_password("pw").unwrap());
        let now = Instant::now();
        let a = auth.issue(now);
        let b = auth.issue(now);
        auth.revoke(&a);
        assert!(!auth.validate(&a, now));
        assert!(
            auth.validate(&b, now),
            "logout ended somebody else's session"
        );
    }

    #[test]
    fn the_session_set_is_capped() {
        let auth = Auth::new(hash_password("pw").unwrap());
        let now = Instant::now();
        let first = auth.issue(now);
        for _ in 0..MAX_SESSIONS {
            auth.issue(now);
        }
        assert_eq!(auth.session_count(), MAX_SESSIONS);
        assert!(!auth.validate(&first, now), "the oldest should have gone");
    }

    #[test]
    fn five_attempts_a_minute_per_address_and_then_no_more() {
        let auth = Auth::new(hash_password("pw").unwrap());
        let t0 = Instant::now();
        let ip: IpAddr = "192.0.2.7".parse().unwrap();

        for i in 0..LOGIN_ATTEMPTS {
            assert!(auth.note_attempt(ip, t0), "attempt {i} refused too early");
        }
        assert!(!auth.note_attempt(ip, t0), "a sixth attempt got through");

        // A different address has its own budget.
        let other: IpAddr = "192.0.2.8".parse().unwrap();
        assert!(auth.note_attempt(other, t0));

        // Still refused inside the window, allowed once it has rolled past.
        assert!(!auth.note_attempt(ip, t0 + Duration::from_secs(59)));
        assert!(auth.note_attempt(ip, t0 + Duration::from_secs(61)));
    }

    #[test]
    fn the_address_table_does_not_grow_without_bound() {
        // The attacker this models has a whole IPv6 /64 and uses each address
        // once, which is exactly the case per-address limiting cannot see.
        let auth = Auth::new(hash_password("pw").unwrap());
        let t0 = Instant::now();
        for i in 0..(SWEEP_THRESHOLD as u64 * 4) {
            let ip: IpAddr = format!("2001:db8::{i:x}").parse().unwrap();
            auth.note_attempt(ip, t0);
        }
        let grown = auth.attempts.lock().unwrap().len();

        // One more attempt after the window has rolled sweeps every stale
        // entry rather than adding to them.
        let fresh: IpAddr = "2001:db8:1::1".parse().unwrap();
        assert!(auth.note_attempt(fresh, t0 + LOGIN_WINDOW + Duration::from_secs(1)));
        let after = auth.attempts.lock().unwrap().len();
        assert!(
            after < grown && after == 1,
            "{grown} entries became {after}; stale addresses were retained"
        );
    }

    #[tokio::test]
    async fn only_a_bounded_number_of_verifications_run_at_once() {
        // Argon2 at 19 MiB times tokio's 512 blocking threads is more memory
        // than the device has, so the cap is the thing standing between a
        // login flood and an OOM.
        let auth = Auth::new(hash_password("pw").unwrap());
        let mut held = Vec::new();
        for i in 0..MAX_VERIFICATIONS_IN_FLIGHT {
            held.push(auth.verify_slot().await.unwrap_or_else(|| {
                panic!("slot {i} should have been free");
            }));
        }
        // The next one waits VERIFY_WAIT and then gives up rather than
        // queueing another 19 MiB behind the ones already running.
        tokio::time::pause();
        let refused = tokio::time::timeout(VERIFY_WAIT * 2, auth.verify_slot())
            .await
            .expect("verify_slot must give up on its own");
        assert!(refused.is_none());

        drop(held.pop());
        assert!(
            auth.verify_slot().await.is_some(),
            "a released slot was not reusable"
        );
    }

    #[test]
    fn a_successful_login_clears_the_failure_budget() {
        let auth = Auth::new(hash_password("pw").unwrap());
        let t0 = Instant::now();
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        for _ in 0..LOGIN_ATTEMPTS {
            auth.note_attempt(ip, t0);
        }
        auth.clear_attempts(ip);
        assert!(
            auth.note_attempt(ip, t0),
            "mistyping twice locked the owner out"
        );
    }
}
