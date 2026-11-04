//! API tokens (`docs/sync-architecture.md`, section 6).
//!
//! Format `bkm_<id>_<secret>`: `id` is 8 random bytes in hex, `secret` 32
//! random bytes in hex. Only `sha256(secret)` is stored; lookup is by `id`
//! and the hash comparison is constant time.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use subtle::ConstantTimeEq;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Read the tree.
    Read,
    /// Read and write the tree through the token's own endpoint.
    Sync,
    /// Manage tokens and endpoints.
    Admin,
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Scope::Read => "read",
            Scope::Sync => "sync",
            Scope::Admin => "admin",
        })
    }
}

impl std::str::FromStr for Scope {
    type Err = String;
    fn from_str(s: &str) -> Result<Scope, String> {
        match s {
            "read" => Ok(Scope::Read),
            "sync" => Ok(Scope::Sync),
            "admin" => Ok(Scope::Admin),
            other => Err(format!(
                "unknown scope {other:?}, expected read, sync or admin"
            )),
        }
    }
}

impl Scope {
    /// Whether a token of this scope may do what `required` asks for.
    pub fn allows(self, required: Scope) -> bool {
        match self {
            Scope::Admin => true,
            Scope::Sync => matches!(required, Scope::Sync | Scope::Read),
            Scope::Read => required == Scope::Read,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    /// The user whose bookmarks this token reaches. Admin tokens belong to
    /// no user: they manage the server, not a bookmark tree.
    #[serde(default)]
    pub user_id: Option<String>,
    /// A sync token maps to exactly one endpoint (revocation = stop a device).
    pub endpoint_id: Option<String>,
    #[serde(with = "hex_hash")]
    secret_hash: [u8; 32],
}

mod hex_hash {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(h: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(h))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(d)?;
        let bytes = hex::decode(&text).map_err(serde::de::Error::custom)?;
        bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom("secret hash must be 32 bytes"))
    }
}

#[derive(Debug, Default)]
pub struct Tokens {
    by_id: HashMap<String, TokenRecord>,
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("system randomness");
    hex::encode(buf)
}

pub fn random_id(prefix: &str) -> String {
    format!("{prefix}{}", random_hex(8))
}

fn hash(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// Split `bkm_<id>_<secret>`.
fn parse(token: &str) -> Option<(&str, &str)> {
    token.strip_prefix("bkm_")?.split_once('_')
}

impl Tokens {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a token and return its full text, which is shown only once.
    pub fn create(
        &mut self,
        name: &str,
        scope: Scope,
        user_id: Option<String>,
        endpoint_id: Option<String>,
    ) -> (String, TokenRecord) {
        let id = random_hex(8);
        let secret = random_hex(32);
        let rec = TokenRecord {
            id: id.clone(),
            name: name.to_string(),
            scope,
            user_id,
            endpoint_id,
            secret_hash: hash(&secret),
        };
        self.by_id.insert(id.clone(), rec.clone());
        (format!("bkm_{id}_{secret}"), rec)
    }

    /// Register a token whose text is supplied from outside (the admin token
    /// from the environment). Returns `None` if the text is malformed.
    pub fn insert_known(&mut self, name: &str, scope: Scope, token: &str) -> Option<TokenRecord> {
        let (id, secret) = parse(token)?;
        if id.is_empty() || secret.len() < 32 {
            return None;
        }
        let rec = TokenRecord {
            id: id.to_string(),
            name: name.to_string(),
            scope,
            user_id: None,
            endpoint_id: None,
            secret_hash: hash(secret),
        };
        self.by_id.insert(id.to_string(), rec.clone());
        Some(rec)
    }

    /// Check a presented token. Unknown id, malformed text and wrong secret
    /// are indistinguishable to the caller (and take similar time).
    pub fn authenticate(&self, token: &str) -> Option<&TokenRecord> {
        let (id, secret) = parse(token).unwrap_or(("", ""));
        let presented = hash(secret);
        let rec = self.by_id.get(id);
        let stored = rec.map_or([0u8; 32], |r| r.secret_hash);
        let ok: bool = presented.ct_eq(&stored).into();
        if ok {
            rec
        } else {
            None
        }
    }

    pub fn records(&self) -> Vec<TokenRecord> {
        let mut v: Vec<_> = self.by_id.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn from_records(records: Vec<TokenRecord>) -> Self {
        Tokens {
            by_id: records.into_iter().map(|r| (r.id.clone(), r)).collect(),
        }
    }

    pub fn has_admin(&self) -> bool {
        self.by_id.values().any(|r| r.scope == Scope::Admin)
    }

    /// Drop all tokens of `scope` called `name` (used when the admin token
    /// from the environment is rotated).
    pub fn remove_named(&mut self, name: &str, scope: Scope) {
        self.by_id
            .retain(|_, r| !(r.name == name && r.scope == scope));
    }

    /// Revoke every token of a user.
    pub fn revoke_user(&mut self, user_id: &str) {
        self.by_id
            .retain(|_, r| r.user_id.as_deref() != Some(user_id));
    }

    /// Revoke every token bound to `endpoint_id`.
    pub fn revoke_endpoint(&mut self, endpoint_id: &str) -> bool {
        let before = self.by_id.len();
        self.by_id
            .retain(|_, r| r.endpoint_id.as_deref() != Some(endpoint_id));
        self.by_id.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_token_authenticates_and_wrong_ones_do_not() {
        let mut t = Tokens::new();
        let (text, rec) = t.create(
            "laptop",
            Scope::Sync,
            Some("u_1".into()),
            Some("ep_1".into()),
        );
        assert!(text.starts_with("bkm_"));
        assert_eq!(t.authenticate(&text).unwrap().id, rec.id);
        let tampered = format!("{}0", &text[..text.len() - 1]);
        assert!(t.authenticate(&tampered).is_none() || tampered == text);
        assert!(t.authenticate("bkm_nope_nope").is_none());
        assert!(t.authenticate("").is_none());
        assert!(t.authenticate("Bearer x").is_none());
    }

    #[test]
    fn scopes_nest() {
        assert!(Scope::Admin.allows(Scope::Sync));
        assert!(Scope::Sync.allows(Scope::Read));
        assert!(!Scope::Read.allows(Scope::Sync));
        assert!(!Scope::Sync.allows(Scope::Admin));
    }

    #[test]
    fn revoking_an_endpoint_kills_its_tokens() {
        let mut t = Tokens::new();
        let (text, _) = t.create(
            "laptop",
            Scope::Sync,
            Some("u_1".into()),
            Some("ep_1".into()),
        );
        t.revoke_endpoint("ep_1");
        assert!(t.authenticate(&text).is_none());
    }

    #[test]
    fn known_token_from_environment() {
        let mut t = Tokens::new();
        let text = format!("bkm_abc_{}", "f".repeat(64));
        assert!(t.insert_known("admin", Scope::Admin, &text).is_some());
        assert_eq!(t.authenticate(&text).unwrap().scope, Scope::Admin);
        assert!(t.insert_known("admin", Scope::Admin, "short").is_none());
    }
}
