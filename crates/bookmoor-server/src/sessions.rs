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

#[derive(Debug, Default)]
pub struct Sessions {
    by_hash: HashMap<[u8; 32], Session>,
    tickets: HashMap<[u8; 32], Ticket>,
    /// Authenticator secrets being set up, by user id (not yet confirmed).
    setups: HashMap<String, String>,
}

fn key(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
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
        self.setups.remove(user_id);
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
