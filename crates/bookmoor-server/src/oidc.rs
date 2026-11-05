//! Single sign-on with OpenID Connect (authorization code flow with PKCE) for
//! the web UI. The IdP only proves who someone is: a user is found by the
//! pair (issuer, subject) stored on the user, never by name, and a session
//! cookie is issued exactly as after a password login.
//!
//! The provider is discovered on first use, so the server starts even while
//! the IdP is down. Flows in progress live in memory (like sessions).

use openidconnect::core::{CoreClient, CoreProviderMetadata, CoreResponseType};
use openidconnect::reqwest;
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const FLOW_TTL: Duration = Duration::from_secs(600);
const MAX_FLOWS: usize = 1_000;

/// Who the IdP says someone is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub issuer: String,
    pub subject: String,
}

/// The provider settings; also what the web UI saves in the state file.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Public address of the UI, no trailing slash (the callback is below it).
    pub public_url: String,
    /// Text on the sign-in button.
    #[serde(default = "default_label")]
    pub label: String,
    /// Create a user on the first sign-in of someone not linked yet.
    #[serde(default)]
    pub create_users: bool,
}

pub fn default_label() -> String {
    "single sign-on".into()
}

/// Never prints the secret.
impl std::fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "***"))
            .field("public_url", &self.public_url)
            .field("label", &self.label)
            .field("create_users", &self.create_users)
            .finish()
    }
}

pub struct Oidc {
    pub cfg: OidcConfig,
    http: reqwest::Client,
    metadata: tokio::sync::OnceCell<CoreProviderMetadata>,
}

impl std::fmt::Debug for Oidc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Oidc")
            .field("issuer", &self.cfg.issuer)
            .finish()
    }
}

/// A sign-in in progress between the redirect to the IdP and its callback.
#[derive(Debug)]
pub struct Flow {
    nonce: Nonce,
    verifier: PkceCodeVerifier,
    /// SHA-256 of the cookie that ties the callback to the browser that started it.
    binder: [u8; 32],
    /// Set when a signed-in user links their account instead of signing in.
    pub link_user: Option<String>,
    /// Query string to append to the UI address afterwards (the extension's sign-in).
    pub ret: String,
    expires: Instant,
}

#[derive(Debug, Default)]
pub struct Flows {
    by_state: HashMap<String, Flow>,
}

fn bind(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

impl Flows {
    pub fn insert(&mut self, state: String, flow: Flow, now: Instant) {
        self.by_state.retain(|_, f| f.expires > now);
        if self.by_state.len() >= MAX_FLOWS {
            if let Some(k) = self
                .by_state
                .iter()
                .min_by_key(|(_, f)| f.expires)
                .map(|(k, _)| k.clone())
            {
                self.by_state.remove(&k);
            }
        }
        self.by_state.insert(state, flow);
    }

    /// The flow for `state`, if it is still valid and `cookie` is the secret
    /// of the browser that started it. A flow can be used once.
    pub fn take(&mut self, state: &str, cookie: &str, now: Instant) -> Option<Flow> {
        let flow = self.by_state.remove(state)?;
        (flow.expires > now && flow.binder == bind(cookie)).then_some(flow)
    }
}

/// What a callback learned, before it is turned into a session.
#[derive(Debug)]
pub struct Verified {
    pub identity: Identity,
    pub username: Option<String>,
}

/// A sign-in just begun: where to send the browser, the cookie value that
/// binds the callback to it, and the flow to remember under `state`.
pub struct Started {
    pub url: String,
    pub cookie: String,
    pub state: String,
    pub flow: Flow,
}

impl Oidc {
    pub fn new(mut cfg: OidcConfig) -> Result<Self, String> {
        cfg.public_url = cfg.public_url.trim().trim_end_matches('/').to_string();
        cfg.issuer = cfg.issuer.trim().to_string();
        cfg.client_id = cfg.client_id.trim().to_string();
        cfg.label = cfg.label.trim().to_string();
        if cfg.label.is_empty() {
            cfg.label = default_label();
        }
        if cfg.label.chars().count() > 40 {
            return Err("the button text is longer than 40 characters".into());
        }
        if cfg.client_id.is_empty() {
            return Err("the client ID is empty".into());
        }
        for (what, url) in [("issuer", &cfg.issuer), ("public URL", &cfg.public_url)] {
            let ok = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"))
                .is_some_and(|rest| !rest.is_empty());
            if !ok {
                return Err(format!("the {what} must start with https:// or http://"));
            }
        }
        IssuerUrl::new(cfg.issuer.clone()).map_err(|e| format!("invalid OIDC issuer: {e}"))?;
        RedirectUrl::new(callback_url(&cfg.public_url))
            .map_err(|e| format!("invalid public URL: {e}"))?;
        let http = reqwest::ClientBuilder::new()
            // Following redirects would open the client up to SSRF.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Oidc {
            cfg,
            http,
            metadata: tokio::sync::OnceCell::new(),
        })
    }

    async fn client(
        &self,
    ) -> Result<
        CoreClient<
            openidconnect::EndpointSet,
            openidconnect::EndpointNotSet,
            openidconnect::EndpointNotSet,
            openidconnect::EndpointNotSet,
            openidconnect::EndpointMaybeSet,
            openidconnect::EndpointMaybeSet,
        >,
        String,
    > {
        let meta = self
            .metadata
            .get_or_try_init(|| async {
                let issuer = IssuerUrl::new(self.cfg.issuer.clone()).map_err(|e| e.to_string())?;
                CoreProviderMetadata::discover_async(issuer, &self.http)
                    .await
                    .map_err(|e| format!("cannot reach the identity provider: {e}"))
            })
            .await?
            .clone();
        Ok(CoreClient::from_provider_metadata(
            meta,
            ClientId::new(self.cfg.client_id.clone()),
            self.cfg.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(
            RedirectUrl::new(callback_url(&self.cfg.public_url)).map_err(|e| e.to_string())?,
        ))
    }

    /// Ask the provider for its discovery document (never cached), to see
    /// whether these settings reach it. Returns its authorization endpoint.
    pub async fn check(&self) -> Result<String, String> {
        let issuer = IssuerUrl::new(self.cfg.issuer.clone()).map_err(|e| e.to_string())?;
        let meta = CoreProviderMetadata::discover_async(issuer, &self.http)
            .await
            .map_err(|e| format!("cannot reach the identity provider: {e}"))?;
        Ok(meta.authorization_endpoint().to_string())
    }

    /// Begin a sign-in: the IdP address to send the browser to.
    pub async fn start(&self, link_user: Option<String>, ret: String) -> Result<Started, String> {
        let client = self.client().await?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = client
            .authorize_url(
                AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("profile".into()))
            .add_scope(Scope::new("email".into()))
            .set_pkce_challenge(challenge)
            .url();
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).expect("system randomness");
        let cookie = hex::encode(raw);
        Ok(Started {
            url: url.to_string(),
            cookie: cookie.clone(),
            state: state.secret().clone(),
            flow: Flow {
                nonce,
                verifier,
                binder: bind(&cookie),
                link_user,
                ret,
                expires: Instant::now() + FLOW_TTL,
            },
        })
    }

    /// Finish a sign-in: trade the code for an ID token and check it.
    pub async fn finish(&self, flow: Flow, code: String) -> Result<Verified, String> {
        let client = self.client().await?;
        let tokens = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|e| e.to_string())?
            .set_pkce_verifier(flow.verifier)
            .request_async(&self.http)
            .await
            .map_err(|e| format!("the identity provider refused the code: {e}"))?;
        let id_token = tokens
            .id_token()
            .ok_or("the identity provider sent no ID token")?;
        let claims = id_token
            .claims(&client.id_token_verifier(), &flow.nonce)
            .map_err(|e| format!("the ID token is not valid: {e}"))?;
        let username = claims
            .preferred_username()
            .map(|u| u.as_str().to_string())
            .or_else(|| {
                claims
                    .email()
                    .map(|e| e.as_str().split('@').next().unwrap_or("").to_string())
            });
        Ok(Verified {
            identity: Identity {
                issuer: self.cfg.issuer.clone(),
                subject: claims.subject().as_str().to_string(),
            },
            username,
        })
    }
}

pub fn callback_url(public_url: &str) -> String {
    format!("{}/ui/api/oidc/callback", public_url.trim_end_matches('/'))
}

/// A valid, unused user name from what the IdP calls someone.
pub fn user_name_from(wanted: Option<&str>, taken: impl Fn(&str) -> bool) -> String {
    let mut base: String = wanted
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(28)
        .collect();
    if base.is_empty() {
        base = "user".into();
    }
    if base.starts_with("u_") {
        base.insert(0, 'x');
    }
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|n| !taken(n))
        .expect("endless range")
}

/// Only a bare query string may be carried through the sign-in.
pub fn return_ok(ret: &str) -> bool {
    ret.len() <= 1024
        && (ret.is_empty()
            || (ret.starts_with('?')
                && ret[1..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "=&_%.-~".contains(c))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flow_is_used_once_by_its_own_browser() {
        let mut flows = Flows::default();
        let now = Instant::now();
        let mk = |cookie: &str| Flow {
            nonce: Nonce::new("n".into()),
            verifier: PkceCodeVerifier::new("v".into()),
            binder: bind(cookie),
            link_user: None,
            ret: String::new(),
            expires: now + FLOW_TTL,
        };
        flows.insert("s1".into(), mk("mine"), now);
        assert!(flows.take("s1", "other", now).is_none(), "wrong browser");
        assert!(
            flows.take("s1", "mine", now).is_none(),
            "gone after a failed try"
        );
        flows.insert("s2".into(), mk("mine"), now);
        assert!(flows.take("s2", "mine", now).is_some());
        assert!(flows.take("s2", "mine", now).is_none(), "only once");
        flows.insert("s3".into(), mk("mine"), now);
        assert!(
            flows.take("s3", "mine", now + FLOW_TTL).is_none(),
            "expired"
        );
    }

    #[test]
    fn names_are_made_valid_and_unique() {
        let none = |_: &str| false;
        assert_eq!(user_name_from(Some("anna"), none), "anna");
        assert_eq!(user_name_from(Some("Anna Müller"), none), "Anna-M-ller");
        assert_eq!(user_name_from(None, none), "user");
        assert_eq!(user_name_from(Some("u_abc"), none), "xu_abc");
        assert_eq!(user_name_from(Some(&"a".repeat(80)), none).len(), 28);
        let taken = |n: &str| n == "anna" || n == "anna-2";
        assert_eq!(user_name_from(Some("anna"), taken), "anna-3");
    }

    #[test]
    fn only_a_bare_query_string_is_carried_through() {
        assert!(return_ok(""));
        assert!(return_ok(
            "?connect=https%3A%2F%2Fabc.chromiumapp.org%2F&state=s1&name=Chrome%20on%20Linux"
        ));
        for bad in [
            "https://evil.example/",
            "//evil.example",
            "/x",
            "?a=<b>",
            "?a#b",
            "?a b",
        ] {
            assert!(!return_ok(bad), "{bad}");
        }
        assert!(!return_ok(&format!("?{}", "a".repeat(1100))));
    }
}
