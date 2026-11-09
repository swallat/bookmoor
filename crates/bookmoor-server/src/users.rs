//! Users (`docs/multi-user.md`). Every user owns one bookmark tree; tokens
//! and devices belong to a user. A user may also have a password for the
//! web UI. Passwords are stored as Argon2id hashes in PHC string format.

use crate::auth::random_id;
use argon2::password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub const MAX_USERS: usize = 100;
pub const MIN_PASSWORD_CHARS: usize = 10;
/// Upper bound so a huge password cannot be used to burn CPU.
pub const MAX_PASSWORD_BYTES: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    /// May manage all users in the web UI.
    pub admin: bool,
    /// `None`: the user cannot sign in to the web UI (tokens still work).
    pub password_hash: Option<String>,
    /// Who the OIDC identity provider says this user is (single sign-on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<crate::oidc::Identity>,
    /// Cannot sign in, and their tokens stop working; nothing is deleted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Password logins need a second factor; set by an admin.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mfa_required: bool,
    /// Authenticator app for the password login (two-factor authentication).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp: Option<crate::mfa::Totp>,
    /// SHA-256 hashes of the unused recovery codes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery: Vec<String>,
    /// Passkeys (WebAuthn): sign in without a password, or as the second
    /// factor after it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passkeys: Vec<crate::webauthn::Passkey>,
}

impl User {
    /// A second factor is set up (authenticator app or a passkey).
    pub fn has_second_factor(&self) -> bool {
        self.totp.is_some() || !self.passkeys.is_empty()
    }

    /// Defaults for the fields a caller usually does not care about.
    pub fn blank() -> User {
        User {
            id: String::new(),
            name: String::new(),
            admin: false,
            password_hash: None,
            oidc: None,
            disabled: false,
            mfa_required: false,
            totp: None,
            recovery: Vec::new(),
            passkeys: Vec::new(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Users {
    by_id: BTreeMap<String, User>,
}

fn argon2() -> Argon2<'static> {
    // Cheap parameters under `cargo test` keep the suite fast; the hash
    // string records its parameters, so verification is unaffected.
    let params = if cfg!(test) {
        Params::new(8, 1, 1, None)
    } else {
        // OWASP's second recommended profile: 19 MiB, 2 passes.
        Params::new(19 * 1024, 2, 1, None)
    }
    .expect("valid argon2 parameters");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

pub fn check_password(pw: &str) -> Result<(), String> {
    if pw.chars().count() < MIN_PASSWORD_CHARS {
        return Err(format!(
            "password must have at least {MIN_PASSWORD_CHARS} characters"
        ));
    }
    if pw.len() > MAX_PASSWORD_BYTES {
        return Err(format!(
            "password must be at most {MAX_PASSWORD_BYTES} bytes"
        ));
    }
    Ok(())
}

pub fn hash_password(pw: &str) -> Result<String, String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("system randomness");
    argon2()
        .hash_password_with_salt(pw.as_bytes(), &salt)
        .map(|h: PasswordHash| h.to_string())
        .map_err(|e| e.to_string())
}

/// Verify against `stored`, or against a throwaway hash when there is none,
/// so that unknown users and users without a password cost the same time.
pub fn verify_password(stored: Option<&str>, pw: &str) -> bool {
    static DUMMY: OnceLock<String> = OnceLock::new();
    let dummy = DUMMY.get_or_init(|| hash_password("bookmoor dummy").expect("hash"));
    let real = stored.is_some();
    let ok = argon2()
        .verify_password(pw.as_bytes(), stored.unwrap_or(dummy.as_str()))
        .is_ok();
    ok && real
}

pub fn check_name(name: &str) -> Result<(), String> {
    let n = name.chars().count();
    // `u_` is the prefix of generated ids; names must never look like one.
    if name.starts_with("u_") {
        return Err("user names cannot start with u_".into());
    }
    if !(1..=32).contains(&n)
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err("user name must be 1 to 32 characters of letters, digits, . _ -".into());
    }
    Ok(())
}

impl Users {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_records(records: Vec<User>) -> Result<Self, String> {
        let mut u = Users::new();
        for r in records {
            let key = r.name.to_lowercase();
            if u.by_id.values().any(|x| x.name.to_lowercase() == key) {
                return Err(format!("duplicate user name {}", r.name));
            }
            u.by_id.insert(r.id.clone(), r);
        }
        Ok(u)
    }

    pub fn records(&self) -> Vec<User> {
        self.by_id.values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&User> {
        self.by_id.get(id)
    }

    pub fn list(&self) -> impl Iterator<Item = &User> {
        self.by_id.values()
    }

    /// Case-insensitive lookup by name.
    pub fn by_name(&self, name: &str) -> Option<&User> {
        let key = name.trim().to_lowercase();
        self.by_id.values().find(|u| u.name.to_lowercase() == key)
    }

    pub fn create(
        &mut self,
        name: &str,
        admin: bool,
        password: Option<&str>,
    ) -> Result<User, String> {
        let hash = match password {
            Some(p) => {
                check_password(p)?;
                Some(hash_password(p)?)
            }
            None => None,
        };
        self.create_hashed(name, admin, hash)
    }

    /// Like [`create`](Self::create) with a hash made earlier, so the slow
    /// hashing can happen outside the state lock.
    pub fn create_hashed(
        &mut self,
        name: &str,
        admin: bool,
        password_hash: Option<String>,
    ) -> Result<User, String> {
        let name = name.trim();
        check_name(name)?;
        if self.by_name(name).is_some() {
            return Err(format!("user {name} already exists"));
        }
        if self.by_id.len() >= MAX_USERS {
            return Err(format!("at most {MAX_USERS} users"));
        }
        let user = User {
            id: random_id("u_"),
            name: name.to_string(),
            admin,
            password_hash,
            ..User::blank()
        };
        self.by_id.insert(user.id.clone(), user.clone());
        Ok(user)
    }

    pub fn set_password(&mut self, id: &str, password: &str) -> Result<(), String> {
        check_password(password)?;
        let hash = hash_password(password)?;
        let u = self.by_id.get_mut(id).ok_or("no such user")?;
        u.password_hash = Some(hash);
        Ok(())
    }

    /// Store a hash made earlier (see [`hash_password`]).
    pub fn set_hash(&mut self, id: &str, hash: String) -> Result<(), String> {
        let u = self.by_id.get_mut(id).ok_or("no such user")?;
        u.password_hash = Some(hash);
        Ok(())
    }

    pub fn set_admin(&mut self, id: &str, admin: bool) -> Result<(), String> {
        let u = self.by_id.get_mut(id).ok_or("no such user")?;
        u.admin = admin;
        Ok(())
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<(), String> {
        let name = name.trim();
        check_name(name)?;
        if self.by_name(name).is_some_and(|u| u.id != id) {
            return Err(format!("user {name} already exists"));
        }
        let u = self.by_id.get_mut(id).ok_or("no such user")?;
        u.name = name.to_string();
        Ok(())
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut User> {
        self.by_id.get_mut(id)
    }

    pub fn admin_count(&self) -> usize {
        self.by_id.values().filter(|u| u.admin).count()
    }

    pub fn any_password(&self) -> bool {
        self.by_id.values().any(|u| u.password_hash.is_some())
    }

    /// Somebody can sign in to the web UI (by password or single sign-on).
    pub fn any_login(&self) -> bool {
        self.by_id
            .values()
            .any(|u| u.password_hash.is_some() || u.oidc.is_some())
    }

    /// Whoever owns the passkey with this credential id.
    pub fn by_passkey(&self, id: &str) -> Option<&User> {
        self.by_id
            .values()
            .find(|u| u.passkeys.iter().any(|p| p.id == id))
    }

    pub fn by_identity(&self, id: &crate::oidc::Identity) -> Option<&User> {
        self.by_id.values().find(|u| u.oidc.as_ref() == Some(id))
    }

    /// Link (or with `None`, unlink) a single sign-on identity. One identity
    /// belongs to one user.
    pub fn set_oidc(
        &mut self,
        user_id: &str,
        identity: Option<crate::oidc::Identity>,
    ) -> Result<(), String> {
        if let Some(i) = &identity {
            if self.by_identity(i).is_some_and(|u| u.id != user_id) {
                return Err("that single sign-on account is linked to another user".into());
            }
        }
        let u = self.by_id.get_mut(user_id).ok_or("no such user")?;
        u.oidc = identity;
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> bool {
        self.by_id.remove(id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_verify_and_unknown_users_never_do() {
        let mut u = Users::new();
        let a = u
            .create("Alice", true, Some("correct horse battery"))
            .unwrap();
        let h = a.password_hash.as_deref();
        assert!(verify_password(h, "correct horse battery"));
        assert!(!verify_password(h, "wrong horse battery"));
        assert!(!verify_password(None, "bookmoor dummy"));
    }

    #[test]
    fn hashes_written_by_argon2_0_5_still_verify() {
        // Produced by argon2 0.5 with the production parameters, so stored
        // user passwords keep working across the 0.6 upgrade.
        let legacy = "$argon2id$v=19$m=19456,t=2,p=1$BwcHBwcHBwcHBwcHBwcHBw$WLRGUa13/B3MXznFse33FId4O++DaYL6ulYUMEhpRqE";
        assert!(verify_password(Some(legacy), "correct horse battery"));
        assert!(!verify_password(Some(legacy), "wrong horse battery"));
        assert!(!verify_password(
            Some("not a phc string"),
            "correct horse battery"
        ));
    }

    #[test]
    fn names_are_unique_ignoring_case_and_validated() {
        let mut u = Users::new();
        u.create("Alice", false, None).unwrap();
        assert!(u.create("alice", false, None).is_err());
        assert!(u.create("", false, None).is_err());
        assert!(u.create("a b", false, None).is_err());
        assert!(u.create("bob", false, Some("short")).is_err());
        assert_eq!(u.by_name(" ALICE ").unwrap().name, "Alice");
    }
}
