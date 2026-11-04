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
}

#[derive(Debug, Default)]
pub struct Sessions {
    by_hash: HashMap<[u8; 32], Session>,
}

fn key(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Sessions {
    /// Start a session and return the cookie value.
    pub fn create(&mut self, user_id: &str, now: Instant) -> String {
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
            },
        );
        token
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
}
