//! Passkeys (WebAuthn): verification of registrations and sign-ins.
//!
//! Only what bookmoor needs: ES256 (ECDSA P-256 with SHA-256, which every
//! platform authenticator and security key speaks), attestation `none`, user
//! verification always required. The server stores each credential's public
//! key; the private key never leaves the authenticator.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_PASSKEYS: usize = 10;
pub const MAX_NAME_CHARS: usize = 40;

/// COSE algorithm identifier of ES256.
pub const ALG_ES256: i64 = -7;

const FLAG_UP: u8 = 0x01;
const FLAG_UV: u8 = 0x04;
const FLAG_AT: u8 = 0x40;

/// A registered credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Passkey {
    /// Credential id, base64url.
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<u64>,
    /// The relying party id (host name) it was registered for.
    pub rp_id: String,
    /// SEC1 uncompressed P-256 public key, base64url.
    pub public_key: String,
    #[serde(default)]
    pub sign_count: u32,
}

pub fn new_challenge() -> String {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).expect("system randomness");
    B64.encode(raw)
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    B64.decode(s.trim_end_matches('='))
        .map_err(|_| "invalid base64".to_string())
}

pub fn b64_encode(b: &[u8]) -> String {
    B64.encode(b)
}

pub fn check_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > MAX_NAME_CHARS || n.chars().any(char::is_control) {
        return Err(format!(
            "the passkey name must be 1 to {MAX_NAME_CHARS} characters"
        ));
    }
    Ok(n.to_string())
}

/// The relying party id and origin for a browser `Origin` header: the host
/// name, and the origin itself. Only https (or http on localhost, which
/// browsers treat as secure) can use WebAuthn.
pub fn rp_from_origin(origin: &str) -> Result<(String, String), String> {
    let origin = origin.trim_end_matches('/');
    let (scheme, rest) = origin
        .split_once("://")
        .ok_or("the request has no valid Origin")?;
    let host = rest
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(rest, |(h, _)| h);
    let local = host == "localhost" || host.ends_with(".localhost");
    if host.is_empty()
        || rest.contains(['/', '@', '?', '#'])
        || !(scheme == "https" || (scheme == "http" && local))
        || host.starts_with('[')
        || host.parse::<std::net::Ipv4Addr>().is_ok()
    {
        return Err("passkeys need an https address with a host name".into());
    }
    Ok((host.to_ascii_lowercase(), origin.to_ascii_lowercase()))
}

#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
    origin: String,
    #[serde(default, rename = "crossOrigin")]
    cross_origin: bool,
}

fn check_client_data(json: &[u8], kind: &str, challenge: &str, origin: &str) -> Result<(), String> {
    let c: ClientData = serde_json::from_slice(json).map_err(|_| "invalid client data")?;
    if c.kind != kind {
        return Err("wrong ceremony".into());
    }
    if c.challenge != challenge {
        return Err("the challenge does not match".into());
    }
    if c.origin.trim_end_matches('/').to_ascii_lowercase() != origin || c.cross_origin {
        return Err("the origin does not match".into());
    }
    Ok(())
}

struct AuthData<'a> {
    rp_hash: &'a [u8],
    flags: u8,
    sign_count: u32,
    rest: &'a [u8],
}

fn parse_auth_data(d: &[u8]) -> Result<AuthData<'_>, String> {
    if d.len() < 37 {
        return Err("authenticator data is too short".into());
    }
    Ok(AuthData {
        rp_hash: &d[..32],
        flags: d[32],
        sign_count: u32::from_be_bytes([d[33], d[34], d[35], d[36]]),
        rest: &d[37..],
    })
}

fn check_auth_data(a: &AuthData, rp_id: &str) -> Result<(), String> {
    if a.rp_hash != Sha256::digest(rp_id.as_bytes()).as_slice() {
        return Err("the passkey belongs to another site".into());
    }
    if a.flags & FLAG_UP == 0 {
        return Err("the passkey was not touched".into());
    }
    if a.flags & FLAG_UV == 0 {
        return Err("the passkey did not verify you (PIN, fingerprint or face)".into());
    }
    Ok(())
}

/// A verified registration: credential id, public key, signature counter.
pub struct Registered {
    pub id: Vec<u8>,
    pub public_key: Vec<u8>,
    pub sign_count: u32,
}

fn cbor_int(v: &ciborium::Value) -> Option<i64> {
    v.as_integer()
        .and_then(|i| i64::try_from(i128::from(i)).ok())
}

/// The public key from a COSE_Key, as SEC1 uncompressed bytes. Only ES256.
fn cose_to_sec1(cose: &ciborium::Value) -> Result<Vec<u8>, String> {
    let map = cose.as_map().ok_or("invalid public key")?;
    let get = |label: i64| {
        map.iter()
            .find(|(k, _)| cbor_int(k) == Some(label))
            .map(|(_, v)| v)
    };
    let int = |label| get(label).and_then(cbor_int);
    if int(1) != Some(2) || int(3) != Some(ALG_ES256) || int(-1) != Some(1) {
        return Err("only ES256 passkeys are supported".into());
    }
    let coord = |label| {
        get(label)
            .and_then(|v| v.as_bytes())
            .filter(|b| b.len() == 32)
            .ok_or("invalid public key")
    };
    let mut key = vec![0x04];
    key.extend_from_slice(coord(-2)?);
    key.extend_from_slice(coord(-3)?);
    VerifyingKey::from_sec1_bytes(&key).map_err(|_| "invalid public key")?;
    Ok(key)
}

/// Check a registration (`navigator.credentials.create` with attestation
/// `none`).
pub fn verify_registration(
    client_data_json: &[u8],
    attestation_object: &[u8],
    challenge: &str,
    origin: &str,
    rp_id: &str,
) -> Result<Registered, String> {
    check_client_data(client_data_json, "webauthn.create", challenge, origin)?;
    let att: ciborium::Value =
        ciborium::from_reader(attestation_object).map_err(|_| "invalid attestation")?;
    let auth_data = att
        .as_map()
        .and_then(|m| {
            m.iter()
                .find(|(k, _)| k.as_text() == Some("authData"))
                .and_then(|(_, v)| v.as_bytes())
        })
        .ok_or("invalid attestation")?;
    let a = parse_auth_data(auth_data)?;
    check_auth_data(&a, rp_id)?;
    if a.flags & FLAG_AT == 0 || a.rest.len() < 18 {
        return Err("the passkey sent no credential".into());
    }
    let id_len = u16::from_be_bytes([a.rest[16], a.rest[17]]) as usize;
    let after = &a.rest[18..];
    if id_len == 0 || id_len > 1023 || after.len() < id_len {
        return Err("invalid credential id".into());
    }
    let (id, mut key_bytes) = after.split_at(id_len);
    let cose: ciborium::Value =
        ciborium::from_reader(&mut key_bytes).map_err(|_| "invalid public key")?;
    Ok(Registered {
        id: id.to_vec(),
        public_key: cose_to_sec1(&cose)?,
        sign_count: a.sign_count,
    })
}

/// Check a sign-in (`navigator.credentials.get`) against a stored passkey;
/// returns the new signature counter.
pub fn verify_assertion(
    client_data_json: &[u8],
    authenticator_data: &[u8],
    signature: &[u8],
    challenge: &str,
    origin: &str,
    key: &Passkey,
) -> Result<u32, String> {
    check_client_data(client_data_json, "webauthn.get", challenge, origin)?;
    let a = parse_auth_data(authenticator_data)?;
    check_auth_data(&a, &key.rp_id)?;
    let public = b64_decode(&key.public_key)?;
    let vk = VerifyingKey::from_sec1_bytes(&public).map_err(|_| "stored key is invalid")?;
    let sig = Signature::from_der(signature).map_err(|_| "invalid signature")?;
    let mut signed = authenticator_data.to_vec();
    signed.extend_from_slice(&Sha256::digest(client_data_json));
    vk.verify(&signed, &sig)
        .map_err(|_| "the signature is wrong")?;
    // A counter that does not move forward hints at a cloned authenticator.
    // Many passkeys always report 0, which is fine.
    if (a.sign_count != 0 || key.sign_count != 0) && a.sign_count <= key.sign_count {
        return Err("the passkey looks cloned".into());
    }
    Ok(a.sign_count)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use p256::ecdsa::{signature::Signer, SigningKey};

    pub const ORIGIN: &str = "https://bookmoor.example";
    pub const RP: &str = "bookmoor.example";

    /// A software authenticator for the tests.
    pub struct Authenticator {
        pub key: SigningKey,
        pub id: Vec<u8>,
        pub counter: u32,
    }

    pub fn client_data(kind: &str, challenge: &str, origin: &str) -> Vec<u8> {
        serde_json::json!({"type": kind, "challenge": challenge, "origin": origin})
            .to_string()
            .into_bytes()
    }

    fn auth_data(rp: &str, flags: u8, counter: u32) -> Vec<u8> {
        let mut d = Sha256::digest(rp.as_bytes()).to_vec();
        d.push(flags);
        d.extend_from_slice(&counter.to_be_bytes());
        d
    }

    impl Authenticator {
        #[allow(clippy::new_without_default)]
        pub fn new() -> Self {
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).unwrap();
            seed[0] |= 1;
            let mut id = vec![0u8; 16];
            getrandom::fill(&mut id).unwrap();
            Authenticator {
                key: SigningKey::from_slice(&seed).unwrap(),
                id,
                counter: 0,
            }
        }

        /// (clientDataJSON, attestationObject)
        pub fn create(
            &self,
            challenge: &str,
            origin: &str,
            rp: &str,
            flags: u8,
        ) -> (Vec<u8>, Vec<u8>) {
            let point = self.key.verifying_key().to_sec1_point(false);
            use ciborium::Value as V;
            let cose = V::Map(vec![
                (V::Integer(1.into()), V::Integer(2.into())),
                (V::Integer(3.into()), V::Integer((-7).into())),
                (V::Integer((-1).into()), V::Integer(1.into())),
                (
                    V::Integer((-2).into()),
                    V::Bytes(point.x().unwrap().to_vec()),
                ),
                (
                    V::Integer((-3).into()),
                    V::Bytes(point.y().unwrap().to_vec()),
                ),
            ]);
            let mut d = auth_data(rp, flags | FLAG_AT, self.counter);
            d.extend_from_slice(&[0u8; 16]);
            d.extend_from_slice(&(self.id.len() as u16).to_be_bytes());
            d.extend_from_slice(&self.id);
            ciborium::into_writer(&cose, &mut d).unwrap();
            let att = V::Map(vec![
                (V::Text("fmt".into()), V::Text("none".into())),
                (V::Text("attStmt".into()), V::Map(vec![])),
                (V::Text("authData".into()), V::Bytes(d)),
            ]);
            let mut out = Vec::new();
            ciborium::into_writer(&att, &mut out).unwrap();
            (client_data("webauthn.create", challenge, origin), out)
        }

        /// (clientDataJSON, authenticatorData, signature)
        pub fn get(
            &mut self,
            challenge: &str,
            origin: &str,
            rp: &str,
            flags: u8,
        ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            self.counter += 1;
            let cd = client_data("webauthn.get", challenge, origin);
            let ad = auth_data(rp, flags, self.counter);
            let mut signed = ad.clone();
            signed.extend_from_slice(&Sha256::digest(&cd));
            let sig: Signature = self.key.sign(&signed);
            (cd, ad, sig.to_der().as_bytes().to_vec())
        }

        pub fn passkey(&self, reg: &Registered) -> Passkey {
            Passkey {
                id: b64_encode(&reg.id),
                name: "test".into(),
                created: 0,
                last_used: None,
                rp_id: RP.into(),
                public_key: b64_encode(&reg.public_key),
                sign_count: reg.sign_count,
            }
        }
    }

    pub const OK: u8 = FLAG_UP | FLAG_UV;

    #[test]
    fn registration_and_sign_in_round_trip() {
        let mut a = Authenticator::new();
        let (cd, att) = a.create("chal", ORIGIN, RP, OK);
        let reg = verify_registration(&cd, &att, "chal", ORIGIN, RP).unwrap();
        assert_eq!(reg.id, a.id);
        let key = a.passkey(&reg);

        let (cd, ad, sig) = a.get("c2", ORIGIN, RP, OK);
        let n = verify_assertion(&cd, &ad, &sig, "c2", ORIGIN, &key).unwrap();
        assert_eq!(n, 1);

        // Wrong challenge, origin, site, tampered signature, missing UV.
        assert!(verify_assertion(&cd, &ad, &sig, "other", ORIGIN, &key).is_err());
        assert!(verify_assertion(&cd, &ad, &sig, "c2", "https://evil.example", &key).is_err());
        let mut bad = sig.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(verify_assertion(&cd, &ad, &bad, "c2", ORIGIN, &key).is_err());
        let (cd, ad, sig) = a.get("c3", ORIGIN, RP, FLAG_UP);
        assert!(verify_assertion(&cd, &ad, &sig, "c3", ORIGIN, &key).is_err());
        let (cd, ad, sig) = a.get("c4", ORIGIN, "evil.example", OK);
        assert!(verify_assertion(&cd, &ad, &sig, "c4", ORIGIN, &key).is_err());
    }

    #[test]
    fn registration_rejects_bad_input() {
        let a = Authenticator::new();
        let (cd, att) = a.create("chal", ORIGIN, RP, OK);
        assert!(verify_registration(&cd, &att, "x", ORIGIN, RP).is_err());
        assert!(verify_registration(&cd, &att, "chal", "https://evil.example", RP).is_err());
        assert!(verify_registration(&cd, &att, "chal", ORIGIN, "evil.example").is_err());
        let (cd, att) = a.create("chal", ORIGIN, RP, FLAG_UP);
        assert!(verify_registration(&cd, &att, "chal", ORIGIN, RP).is_err());
        assert!(verify_registration(&cd, b"junk", "chal", ORIGIN, RP).is_err());
    }

    #[test]
    fn a_counter_that_stands_still_is_refused() {
        let mut a = Authenticator::new();
        let (cd, att) = a.create("c", ORIGIN, RP, OK);
        let reg = verify_registration(&cd, &att, "c", ORIGIN, RP).unwrap();
        let mut key = a.passkey(&reg);
        key.sign_count = 5; // a.get() will report 1
        let (cd, ad, sig) = a.get("c2", ORIGIN, RP, OK);
        assert!(verify_assertion(&cd, &ad, &sig, "c2", ORIGIN, &key).is_err());
    }

    #[test]
    fn origins_become_rp_ids() {
        assert_eq!(
            rp_from_origin("https://Bookmoor.Example:8443").unwrap(),
            (
                "bookmoor.example".into(),
                "https://bookmoor.example:8443".into()
            )
        );
        assert!(rp_from_origin("http://localhost:8080").is_ok());
        assert!(rp_from_origin("http://bookmoor.example").is_err());
        assert!(rp_from_origin("https://10.0.0.1").is_err());
        assert!(rp_from_origin("https://a.example/x").is_err());
        assert!(rp_from_origin("null").is_err());
    }
}
