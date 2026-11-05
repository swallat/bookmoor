//! Web UI sessions: random 256-bit cookie values, kept in memory as SHA-256
//! hashes with an expiry. A restart signs everybody out, which is fine for
//! an admin page and means no session secret ever reaches the state file.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const MAX_SESSIONS: usize = 10_000;

#[derive(Debug)]
struct Session {
    user_id: String,
    expires: Instant,
    /// Signed in through single sign-on: the provider did any second factor.
    sso: bool,
}

/// A correct password that still waits for the second factor.
#[derive(Debug)]
struct Ticket {
    user_id: String,
    expires: Instant,
    tries: u8,
}

pub const TICKET_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_TICKETS: usize = 1_000;
const MAX_TRIES: u8 = 5;

/// A passkey ceremony in flight: the challenge the browser has to sign.
#[derive(Debug)]
struct Ceremony {
    challenge: String,
    origin: String,
    /// Set when the user is already known (second factor, registration).
    user_id: Option<String>,
    expires: Instant,
}

const MAX_CEREMONIES: usize = 10_000;

#[derive(Default)]
pub struct Sessions {
    /// Passkey sign-ins and registrations, by (hashed) ceremony id. Each
    /// is good for one answer.
    ceremonies: HashMap<[u8; 32], Ceremony>,
    by_hash: HashMap<[u8; 32], Session>,
    tickets: HashMap<[u8; 32], Ticket>,
    /// Authenticator secrets being set up, by user id (not yet confirmed).
    setups: HashMap<String, String>,
}

fn key(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

// Hand-written so that authenticator secrets never reach a log line.
impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions")
            .field("sessions", &self.by_hash.len())
            .field("tickets", &self.tickets.len())
            .field("setups", &self.setups.len())
            .field("ceremonies", &self.ceremonies.len())
            .finish()
    }
}

impl Sessions {
    /// Start a session and return the cookie value.
    pub fn create(&mut self, user_id: &str, now: Instant) -> String {
        self.create_with(user_id, now, false)
    }

    pub fn create_with(&mut self, user_id: &str, now: Instant, sso: bool) -> String {
        self.by_hash.retain(|_, s| s.expires > now);
        if self.by_hash.len() >= MAX_SESSIONS {
            // Drop the oldest to stay bounded.
            if let Some(k) = self
                .by_hash
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(k, _)| *k)
            {
                self.by_hash.remove(&k);
            }
        }
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).expect("system randomness");
        let token = hex::encode(raw);
        self.by_hash.insert(
            key(&token),
            Session {
                user_id: user_id.to_string(),
                expires: now + SESSION_TTL,
                sso,
            },
        );
        token
    }

    /// How long ago the session was started.
    pub fn age(&self, token: &str, now: Instant) -> Option<Duration> {
        self.by_hash
            .get(&key(token))
            .map(|s| now.saturating_duration_since(s.expires - SESSION_TTL))
    }

    pub fn is_sso(&self, token: &str) -> bool {
        self.by_hash.get(&key(token)).is_some_and(|s| s.sso)
    }

    /// Park a user whose password was right until the second factor comes.
    pub fn start_ticket(&mut self, user_id: &str, now: Instant) -> String {
        self.tickets.retain(|_, t| t.expires > now);
        if self.tickets.len() >= MAX_TICKETS {
            if let Some(k) = self
                .tickets
                .iter()
                .min_by_key(|(_, t)| t.expires)
                .map(|(k, _)| *k)
            {
                self.tickets.remove(&k);
            }
        }
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).expect("system randomness");
        let token = hex::encode(raw);
        self.tickets.insert(
            key(&token),
            Ticket {
                user_id: user_id.to_string(),
                expires: now + TICKET_TTL,
                tries: 0,
            },
        );
        token
    }

    /// The user a ticket belongs to; counts one try and burns the ticket
    /// after too many.
    pub fn try_ticket(&mut self, token: &str, now: Instant) -> Option<String> {
        let k = key(token);
        let t = self.tickets.get_mut(&k)?;
        if t.expires <= now || t.tries >= MAX_TRIES {
            self.tickets.remove(&k);
            return None;
        }
        t.tries += 1;
        Some(t.user_id.clone())
    }

    /// The user a ticket belongs to, without counting a try (the passkey
    /// ceremony counts when it ends).
    pub fn ticket_user(&self, token: &str, now: Instant) -> Option<String> {
        self.tickets
            .get(&key(token))
            .filter(|t| t.expires > now && t.tries < MAX_TRIES)
            .map(|t| t.user_id.clone())
    }

    /// Start a passkey ceremony; returns its id.
    pub fn start_ceremony(
        &mut self,
        user_id: Option<&str>,
        challenge: &str,
        origin: &str,
        now: Instant,
    ) -> String {
        self.ceremonies.retain(|_, c| c.expires > now);
        if self.ceremonies.len() >= MAX_CEREMONIES {
            if let Some(k) = self
                .ceremonies
                .iter()
                .min_by_key(|(_, c)| c.expires)
                .map(|(k, _)| *k)
            {
                self.ceremonies.remove(&k);
            }
        }
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).expect("system randomness");
        let id = hex::encode(raw);
        self.ceremonies.insert(
            key(&id),
            Ceremony {
                challenge: challenge.to_string(),
                origin: origin.to_string(),
                user_id: user_id.map(String::from),
                expires: now + TICKET_TTL,
            },
        );
        id
    }

    /// Take a ceremony (it cannot be answered twice): challenge, origin and
    /// the user it was started for.
    pub fn take_ceremony(
        &mut self,
        id: &str,
        now: Instant,
    ) -> Option<(String, String, Option<String>)> {
        let c = self.ceremonies.remove(&key(id))?;
        (c.expires > now).then_some((c.challenge, c.origin, c.user_id))
    }

    pub fn end_ticket(&mut self, token: &str) {
        self.tickets.remove(&key(token));
    }

    pub fn set_setup(&mut self, user_id: &str, secret: String) {
        self.setups.insert(user_id.to_string(), secret);
    }

    pub fn setup(&self, user_id: &str) -> Option<&str> {
        self.setups.get(user_id).map(String::as_str)
    }

    pub fn end_setup(&mut self, user_id: &str) {
        self.setups.remove(user_id);
    }

    pub fn user_for(&self, token: &str, now: Instant) -> Option<&str> {
        self.by_hash
            .get(&key(token))
            .filter(|s| s.expires > now)
            .map(|s| s.user_id.as_str())
    }

    pub fn end(&mut self, token: &str) {
        self.by_hash.remove(&key(token));
    }

    /// Sign a user out everywhere (password change, removal).
    pub fn end_user(&mut self, user_id: &str) {
        self.by_hash.retain(|_, s| s.user_id != user_id);
        self.tickets.retain(|_, t| t.user_id != user_id);
        self.ceremonies
            .retain(|_, c| c.user_id.as_deref() != Some(user_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_expire_and_end() {
        let mut s = Sessions::default();
        let t0 = Instant::now();
        let a = s.create("u_1", t0);
        let b = s.create("u_1", t0);
        assert_eq!(s.user_for(&a, t0), Some("u_1"));
        assert_eq!(s.user_for("nope", t0), None);
        assert_eq!(s.user_for(&a, t0 + SESSION_TTL), None);
        s.end(&a);
        assert_eq!(s.user_for(&a, t0), None);
        s.end_user("u_1");
        assert_eq!(s.user_for(&b, t0), None);
    }

    #[test]
    fn sessions_know_their_age() {
        let mut s = Sessions::default();
        let t0 = Instant::now();
        let a = s.create("u_1", t0);
        assert_eq!(s.age(&a, t0), Some(Duration::ZERO));
        let later = t0 + Duration::from_secs(90);
        assert_eq!(s.age(&a, later), Some(Duration::from_secs(90)));
        assert_eq!(s.age("nope", t0), None);
    }

    #[test]
    fn passkey_ceremonies_work_once() {
        let mut s = Sessions::default();
        let t0 = Instant::now();
        let id = s.start_ceremony(Some("u_1"), "chal", "https://x.example", t0);
        let got = s.take_ceremony(&id, t0).unwrap();
        assert_eq!(got.0, "chal");
        assert_eq!(got.2.as_deref(), Some("u_1"));
        assert!(s.take_ceremony(&id, t0).is_none());
        let id = s.start_ceremony(None, "c", "o", t0);
        assert!(s.take_ceremony(&id, t0 + TICKET_TTL).is_none());
    }

    #[test]
    fn tickets_allow_a_few_tries_and_expire() {
        let mut s = Sessions::default();
        let t0 = Instant::now();
        let t = s.start_ticket("u_1", t0);
        for _ in 0..MAX_TRIES {
            assert_eq!(s.try_ticket(&t, t0).as_deref(), Some("u_1"));
        }
        assert_eq!(s.try_ticket(&t, t0), None, "burnt after too many tries");
        let t = s.start_ticket("u_1", t0);
        assert_eq!(s.try_ticket(&t, t0 + TICKET_TTL), None);
        let t = s.start_ticket("u_1", t0);
        s.end_ticket(&t);
        assert_eq!(s.try_ticket(&t, t0), None);
    }
}
