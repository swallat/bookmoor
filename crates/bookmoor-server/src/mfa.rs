//! Two-factor authentication for the password login: time-based one-time
//! codes (TOTP, RFC 6238: SHA-1, 6 digits, 30 seconds, what every
//! authenticator app speaks) and one-time recovery codes.
//!
//! The TOTP secret has to be readable to check a code, so it lives in the
//! state file next to the token hashes (protect that file like the tokens).
//! Recovery codes are stored as SHA-256 hashes, like tokens.

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};

pub const STEP_SECONDS: u64 = 30;
/// Codes of the previous and the next step are accepted too (clock drift).
const WINDOW: i64 = 1;
const SECRET_BYTES: usize = 20;
pub const RECOVERY_CODES: usize = 10;

/// A user's enrolled authenticator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totp {
    /// Base32, as authenticator apps expect it.
    pub secret: String,
    /// Last time step that was accepted; a code is good once.
    #[serde(default)]
    pub last_step: u64,
}

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let (mut buf, mut bits) = (0u32, 0);
    let mut out = Vec::new();
    for c in text
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
    {
        let v = ALPHABET.iter().position(|a| *a == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

pub fn new_secret() -> String {
    let mut raw = [0u8; SECRET_BYTES];
    getrandom::fill(&mut raw).expect("system randomness");
    base32_encode(&raw)
}

/// The code for time step `step`.
pub fn code_at(secret: &str, step: u64) -> Option<String> {
    let key = base32_decode(secret)?;
    let mut mac = Hmac::<Sha1>::new_from_slice(&key).ok()?;
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 0xf) as usize;
    let n = u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]);
    Some(format!("{:06}", n % 1_000_000))
}

pub fn step_at(unix_seconds: u64) -> u64 {
    unix_seconds / STEP_SECONDS
}

/// Check `code` (spaces allowed) near `now`. Returns the step it matched,
/// which must be later than `after` so that a code cannot be used twice.
pub fn verify(secret: &str, code: &str, now: u64, after: u64) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let now = step_at(now) as i64;
    let mut found = None;
    // Compare every candidate (no early exit on the first mismatch).
    for d in -WINDOW..=WINDOW {
        let step = (now + d).max(0) as u64;
        if step <= after {
            continue;
        }
        let ok = code_at(secret, step)
            .is_some_and(|c| subtle::ConstantTimeEq::ct_eq(c.as_bytes(), code.as_bytes()).into());
        if ok {
            found = Some(step);
        }
    }
    found
}

/// `otpauth://` link that authenticator apps read from a QR code, or open directly.
pub fn otpauth_uri(user: &str, secret: &str) -> String {
    // User names are limited to letters, digits and . _ -, so no escaping.
    format!("otpauth://totp/bookmoor:{user}?secret={secret}&issuer=bookmoor&algorithm=SHA1&digits=6&period=30")
}

fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn hash_recovery(code: &str) -> String {
    hex::encode(Sha256::digest(normalize(code).as_bytes()))
}

/// Fresh recovery codes: the plain texts (shown once) and the hashes to store.
pub fn new_recovery_codes() -> (Vec<String>, Vec<String>) {
    let mut plain = Vec::new();
    for _ in 0..RECOVERY_CODES {
        let mut raw = [0u8; 7];
        getrandom::fill(&mut raw).expect("system randomness");
        let s = base32_encode(&raw); // 56 bits -> 12 chars, keep 10 (50 bits)
        plain.push(format!("{}-{}", &s[..5], &s[5..10]));
    }
    let hashes = plain.iter().map(|c| hash_recovery(c)).collect();
    (plain, hashes)
}

/// Use up a recovery code if `code` is one of `hashes`.
pub fn take_recovery(hashes: &mut Vec<String>, code: &str) -> bool {
    let h = hash_recovery(code);
    let mut hit = false;
    hashes.retain(|x| {
        let same: bool = subtle::ConstantTimeEq::ct_eq(x.as_bytes(), h.as_bytes()).into();
        hit |= same;
        !same
    });
    hit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rfc_6238_vector() {
        // RFC 6238 appendix B, SHA-1, secret "12345678901234567890", T=59 -> 94287082.
        let secret = base32_encode(b"12345678901234567890");
        assert_eq!(code_at(&secret, 59 / 30).as_deref(), Some("287082"));
        assert_eq!(code_at(&secret, 1111111109 / 30).as_deref(), Some("081804"));
    }

    #[test]
    fn base32_round_trips() {
        for n in 0..12 {
            let data: Vec<u8> = (0..n).map(|i| i * 17 + 3).collect();
            assert_eq!(base32_decode(&base32_encode(&data)).unwrap(), data);
        }
        assert!(base32_decode("!!").is_none());
    }

    #[test]
    fn a_code_works_once_and_only_nearby() {
        let secret = new_secret();
        let now = 1_000_000;
        let code = code_at(&secret, step_at(now)).unwrap();
        let step = verify(&secret, &code, now, 0).unwrap();
        assert_eq!(step, step_at(now));
        assert!(verify(&secret, &code, now, step).is_none(), "replay");
        assert!(
            verify(&secret, &code, now + 30, 0).is_some(),
            "one step of drift"
        );
        assert!(verify(&secret, &code, now + 120, 0).is_none(), "too old");
        assert!(verify(&secret, "12345", now, 0).is_none());
        assert!(verify(&secret, "abcdef", now, 0).is_none());
    }

    #[test]
    fn recovery_codes_are_single_use() {
        let (plain, mut hashes) = new_recovery_codes();
        assert_eq!(plain.len(), RECOVERY_CODES);
        let code = plain[3].to_lowercase().replace('-', " ");
        assert!(take_recovery(&mut hashes, &code));
        assert!(!take_recovery(&mut hashes, &code));
        assert_eq!(hashes.len(), RECOVERY_CODES - 1);
        assert!(!take_recovery(&mut hashes, "AAAAA-AAAAA"));
    }
}
