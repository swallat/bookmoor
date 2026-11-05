//! Single sign-on against a small fake identity provider (HS256 ID tokens
//! signed with the client secret), over real HTTP.

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use bookmoor_server::api::{router, AppState, Shared};
use bookmoor_server::auth::Scope;
use bookmoor_server::oidc::{Oidc, OidcConfig};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::json;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SECRET: &str = "a-client-secret-of-some-length";

/// What the fake provider puts into the next ID token.
#[derive(Clone, Default)]
struct Idp {
    base: String,
    sub: String,
    username: Option<String>,
    nonce: String,
}

fn jwt(claims: serde_json::Value) -> String {
    let head = B64.encode(json!({"alg": "HS256", "typ": "JWT"}).to_string());
    let body = B64.encode(claims.to_string());
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(format!("{head}.{body}").as_bytes());
    format!("{head}.{body}.{}", B64.encode(mac.finalize().into_bytes()))
}

async fn fake_idp() -> Arc<Mutex<Idp>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let idp = Arc::new(Mutex::new(Idp {
        base: format!("http://{}", listener.local_addr().unwrap()),
        ..Default::default()
    }));
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(|State(i): State<Arc<Mutex<Idp>>>| async move {
                let b = i.lock().unwrap().base.clone();
                Json(json!({
                    "issuer": b,
                    "authorization_endpoint": format!("{b}/authorize"),
                    "token_endpoint": format!("{b}/token"),
                    "jwks_uri": format!("{b}/jwks"),
                    "response_types_supported": ["code"],
                    "subject_types_supported": ["public"],
                    "id_token_signing_alg_values_supported": ["HS256"],
                }))
            }),
        )
        .route("/jwks", get(|| async { Json(json!({"keys": []})) }))
        .route(
            "/token",
            post(
                |State(i): State<Arc<Mutex<Idp>>>, Form(f): Form<HashMap<String, String>>| async move {
                    assert!(f.contains_key("code_verifier"), "PKCE");
                    let i = i.lock().unwrap().clone();
                    let mut claims = json!({
                        "iss": i.base, "sub": i.sub, "aud": "bookmoor",
                        "exp": 4_000_000_000u64, "iat": 1_700_000_000u64, "nonce": i.nonce,
                    });
                    if let Some(u) = i.username {
                        claims["preferred_username"] = u.into();
                    }
                    Json(json!({
                        "access_token": "at", "token_type": "Bearer", "id_token": jwt(claims),
                    }))
                },
            ),
        )
        .with_state(idp.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    idp
}

struct Env {
    app: Router,
    state: Shared,
    idp: Arc<Mutex<Idp>>,
}

async fn env(create_users: bool) -> Env {
    let idp = fake_idp().await;
    let mut st = AppState {
        oidc: Some(Arc::new(
            Oidc::new(OidcConfig {
                issuer: idp.lock().unwrap().base.clone(),
                client_id: "bookmoor".into(),
                client_secret: Some(SECRET.into()),
                public_url: "https://bookmoor.example".into(),
                label: "Test IdP".into(),
                create_users,
            })
            .unwrap(),
        )),
        ..Default::default()
    };
    st.tokens.create("admin", Scope::Admin, None, None, None);
    let state = Arc::new(Mutex::new(st));
    Env {
        app: router(state.clone()),
        state,
        idp,
    }
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    cookies: Vec<String>,
    body: serde_json::Value,
}

impl Reply {
    fn cookie(&self, name: &str) -> Option<String> {
        self.cookies
            .iter()
            .find_map(|c| c.strip_prefix(&format!("{name}=")))
            .map(|c| c.split(';').next().unwrap().to_string())
            .filter(|c| !c.is_empty())
    }
}

impl Env {
    async fn get(&self, uri: &str, cookies: &[(&str, &str)]) -> Reply {
        self.send("GET", uri, cookies, None).await
    }

    async fn send(
        &self,
        method: &str,
        uri: &str,
        cookies: &[(&str, &str)],
        body: Option<serde_json::Value>,
    ) -> Reply {
        let mut b = Request::builder().method(method).uri(uri);
        if !cookies.is_empty() {
            let c: Vec<String> = cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
            b = b.header("cookie", c.join("; "));
        }
        if method != "GET" {
            b = b.header("x-bookmoor-ui", "1");
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let location = resp
            .headers()
            .get("location")
            .map(|v| v.to_str().unwrap().to_string());
        let cookies = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            location,
            cookies,
            body: serde_json::from_slice(&bytes).unwrap_or_default(),
        }
    }

    /// Run a sign-in up to the provider's answer. Returns the callback reply.
    async fn sso(
        &self,
        start: &str,
        sub: &str,
        username: Option<&str>,
        cookies: &[(&str, &str)],
    ) -> Reply {
        let started = self.get(start, cookies).await;
        assert_eq!(started.status, StatusCode::SEE_OTHER);
        let url = url_params(started.location.as_deref().unwrap());
        let browser = started.cookie("bookmoor_oidc").expect("binding cookie");
        {
            let mut i = self.idp.lock().unwrap();
            i.sub = sub.into();
            i.username = username.map(String::from);
            i.nonce = url["nonce"].clone();
        }
        self.get(
            &format!("/ui/api/oidc/callback?code=abc&state={}", url["state"]),
            &[("bookmoor_oidc", &browser)],
        )
        .await
    }

    async fn password_user(&self, name: &str) -> String {
        let hash = bookmoor_server::users::hash_password("correct horse battery").unwrap();
        let id = {
            let mut st = self.state.lock().unwrap();
            let u = st.users.create_hashed(name, false, Some(hash)).unwrap();
            u.id
        };
        let sid = self
            .state
            .lock()
            .unwrap()
            .sessions
            .create(&id, std::time::Instant::now());
        sid
    }
}

fn url_params(url: &str) -> HashMap<String, String> {
    let q = url.split_once('?').unwrap().1;
    q.split('&')
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap();
            (k.to_string(), urldecode(v))
        })
        .collect()
}

/// Like the browser's `encodeURIComponent`.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn sign_in_needs_a_linked_account_or_creates_one() {
    let e = env(false).await;
    // The login page learns the button text.
    let s = e.get("/ui/api/session", &[]).await;
    assert_eq!(s.body["sso"]["label"], "Test IdP");

    // Nobody is linked yet.
    let r = e
        .sso("/ui/api/oidc/start", "sub-1", Some("anna"), &[])
        .await;
    assert_eq!(r.location.as_deref(), Some("../../?sso_error=unlinked"));
    assert!(r.cookie("bookmoor_session").is_none());

    // A signed-in user links the account, then signs in with it.
    let session = e.password_user("anna").await;
    let r = e
        .sso(
            "/ui/api/oidc/start?link=1",
            "sub-1",
            None,
            &[("bookmoor_session", &session)],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("../../?sso_error=linked"));
    let r = e.sso("/ui/api/oidc/start", "sub-1", None, &[]).await;
    assert_eq!(r.location.as_deref(), Some("../../"));
    let cookie = r.cookie("bookmoor_session").expect("session cookie");
    let me = e
        .get("/ui/api/session", &[("bookmoor_session", &cookie)])
        .await;
    assert_eq!(me.body["user"]["name"], "anna");
    assert_eq!(me.body["user"]["oidc"], true);

    // Another account cannot take the same identity.
    let other = e.password_user("bob").await;
    let r = e
        .sso(
            "/ui/api/oidc/start?link=1",
            "sub-1",
            None,
            &[("bookmoor_session", &other)],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("../../?sso_error=taken"));

    // Unlinking works while there is a password.
    let r = e
        .send(
            "POST",
            "/ui/api/oidc/unlink",
            &[("bookmoor_session", &session)],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = e.sso("/ui/api/oidc/start", "sub-1", None, &[]).await;
    assert_eq!(r.location.as_deref(), Some("../../?sso_error=unlinked"));
}

#[tokio::test]
async fn users_can_be_created_on_first_sign_in() {
    let e = env(true).await;
    e.state
        .lock()
        .unwrap()
        .users
        .create("anna", false, None)
        .unwrap();
    let r = e
        .sso("/ui/api/oidc/start", "sub-9", Some("anna"), &[])
        .await;
    let cookie = r.cookie("bookmoor_session").expect("session cookie");
    let me = e
        .get("/ui/api/session", &[("bookmoor_session", &cookie)])
        .await;
    // The name is taken, so the new user gets another one; never the existing user.
    assert_eq!(me.body["user"]["name"], "anna-2");
    assert_eq!(me.body["user"]["admin"], false);
    // The same identity signs in as the same user afterwards.
    let again = e
        .sso("/ui/api/oidc/start", "sub-9", Some("renamed"), &[])
        .await;
    let cookie = again.cookie("bookmoor_session").unwrap();
    let me = e
        .get("/ui/api/session", &[("bookmoor_session", &cookie)])
        .await;
    assert_eq!(me.body["user"]["name"], "anna-2");
    assert_eq!(e.state.lock().unwrap().users.len(), 2);
}

#[tokio::test]
async fn a_callback_needs_the_browser_that_started_it_and_works_once() {
    let e = env(true).await;
    let started = e.get("/ui/api/oidc/start", &[]).await;
    let url = url_params(started.location.as_deref().unwrap());
    let browser = started.cookie("bookmoor_oidc").unwrap();
    e.idp.lock().unwrap().sub = "s".into();
    e.idp.lock().unwrap().nonce = url["nonce"].clone();
    let cb = format!("/ui/api/oidc/callback?code=abc&state={}", url["state"]);

    // No cookie, or somebody else's: nothing happens, and the flow is spent.
    let r = e.get(&cb, &[("bookmoor_oidc", "not-mine")]).await;
    assert_eq!(r.location.as_deref(), Some("../../?sso_error=expired"));
    let r = e.get(&cb, &[("bookmoor_oidc", &browser)]).await;
    assert_eq!(r.location.as_deref(), Some("../../?sso_error=expired"));

    // A forged state, a provider error, and a wrong nonce.
    let r = e
        .get(
            "/ui/api/oidc/callback?code=abc&state=nope",
            &[("bookmoor_oidc", &browser)],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("../../?sso_error=expired"));
    let started = e.get("/ui/api/oidc/start", &[]).await;
    let url = url_params(started.location.as_deref().unwrap());
    let browser = started.cookie("bookmoor_oidc").unwrap();
    let r = e
        .get(
            &format!(
                "/ui/api/oidc/callback?error=access_denied&state={}",
                url["state"]
            ),
            &[("bookmoor_oidc", &browser)],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("../../?sso_error=denied"));
    let started = e.get("/ui/api/oidc/start", &[]).await;
    let url = url_params(started.location.as_deref().unwrap());
    let browser = started.cookie("bookmoor_oidc").unwrap();
    e.idp.lock().unwrap().nonce = "another-nonce".into();
    let r = e
        .get(
            &format!("/ui/api/oidc/callback?code=abc&state={}", url["state"]),
            &[("bookmoor_oidc", &browser)],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("../../?sso_error=provider"));
    assert_eq!(e.state.lock().unwrap().users.len(), 0);
}

#[tokio::test]
async fn the_extension_sign_in_survives_the_round_trip() {
    let e = env(true).await;
    let ret = "?connect=https%3A%2F%2Fabc.chromiumapp.org%2F&state=s1&name=Chrome";
    let r = e
        .sso(
            &format!("/ui/api/oidc/start?return={}", encode(ret)),
            "s",
            Some("anna"),
            &[],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some(&format!("../../{ret}")[..]));
    // Anything but a bare query string is refused.
    let r = e
        .get(
            "/ui/api/oidc/start?return=https%3A%2F%2Fevil.example%2F",
            &[],
        )
        .await;
    assert_eq!(r.location.as_deref(), Some("../../?sso_error=request"));
}

#[tokio::test]
async fn without_single_sign_on_there_is_no_endpoint() {
    let state = Arc::new(Mutex::new(AppState::default()));
    let app = router(state);
    let resp = app
        .oneshot(
            Request::get("/ui/api/oidc/start")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---- settings in the web UI ------------------------------------------------

impl Env {
    /// A session of a (non-)admin user.
    async fn session_of(&self, name: &str, admin: bool) -> String {
        let mut st = self.state.lock().unwrap();
        let id = st.users.create_hashed(name, admin, None).unwrap().id;
        st.sessions.create(&id, std::time::Instant::now())
    }

    fn form(&self, secret: Option<&str>) -> serde_json::Value {
        let mut f = json!({
            "issuer": self.idp.lock().unwrap().base,
            "client_id": "bookmoor",
            "public_url": "https://bookmoor.example/",
            "label": "Office login",
            "create_users": true,
        });
        if let Some(s) = secret {
            f["client_secret"] = s.into();
        }
        f
    }
}

/// An environment without single sign-on, as before an admin sets it up.
async fn env_without_sso() -> Env {
    let e = env(false).await;
    e.state.lock().unwrap().oidc = None;
    e
}

#[tokio::test]
async fn admins_set_up_single_sign_on_without_a_restart() {
    let e = env_without_sso().await;
    let user = e.session_of("anna", false).await;
    let admin = e.session_of("root", true).await;
    let (u, a) = (
        [("bookmoor_session", &user[..])],
        [("bookmoor_session", &admin[..])],
    );

    // Only admins see or change the settings.
    assert_eq!(e.get("/ui/api/sso", &u).await.status, StatusCode::FORBIDDEN);
    let r = e
        .send("PUT", "/ui/api/sso", &u, Some(e.form(Some(SECRET))))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(
        e.get("/ui/api/sso", &[]).await.status,
        StatusCode::UNAUTHORIZED
    );

    let r = e.get("/ui/api/sso", &a).await;
    assert_eq!(r.body["source"], "none");

    // Testing contacts the provider and saves nothing.
    let r = e
        .send("POST", "/ui/api/sso/test", &a, Some(e.form(Some(SECRET))))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.body["callback_url"],
        "https://bookmoor.example/ui/api/oidc/callback"
    );
    assert!(e.state.lock().unwrap().oidc_config.is_none());
    let mut bad = e.form(None);
    bad["issuer"] = "http://127.0.0.1:1".into();
    let r = e.send("POST", "/ui/api/sso/test", &a, Some(bad)).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let mut bad = e.form(None);
    bad["issuer"] = "ftp://id.example".into();
    let r = e.send("PUT", "/ui/api/sso", &a, Some(bad)).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    // Saving takes effect at once, and the secret is never sent back.
    let r = e
        .send("PUT", "/ui/api/sso", &a, Some(e.form(Some(SECRET))))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["source"], "settings");
    assert_eq!(r.body["config"]["has_secret"], true);
    assert_eq!(
        r.body["callback_url"],
        "https://bookmoor.example/ui/api/oidc/callback"
    );
    assert!(!r.body.to_string().contains(SECRET));
    assert!(!e
        .get("/ui/api/sso", &a)
        .await
        .body
        .to_string()
        .contains(SECRET));
    let s = e.get("/ui/api/session", &[]).await;
    assert_eq!(s.body["sso"]["label"], "Office login");

    // The saved secret is in use: the fake provider signs tokens with it.
    let r = e
        .sso("/ui/api/oidc/start", "sub-7", Some("carla"), &[])
        .await;
    assert_eq!(r.location.as_deref(), Some("../../"));
    assert!(r.cookie("bookmoor_session").is_some());

    // Saving again without a secret keeps it; another client ID drops it.
    let mut f = e.form(None);
    f["label"] = "Renamed".into();
    let r = e.send("PUT", "/ui/api/sso", &a, Some(f)).await;
    assert_eq!(r.body["config"]["has_secret"], true);
    let r = e.sso("/ui/api/oidc/start", "sub-7", None, &[]).await;
    assert!(r.cookie("bookmoor_session").is_some());
    let mut f = e.form(None);
    f["client_id"] = "other".into();
    let r = e.send("PUT", "/ui/api/sso", &a, Some(f)).await;
    assert_eq!(r.body["config"]["has_secret"], false);
    // A secret can also be removed on purpose, unless a new one comes with it.
    let mut f = e.form(None);
    f["clear_secret"] = true.into();
    let r = e.send("PUT", "/ui/api/sso", &a, Some(f)).await;
    assert_eq!(r.body["config"]["has_secret"], false);
    let mut f = e.form(Some(SECRET));
    f["clear_secret"] = true.into();
    let r = e.send("PUT", "/ui/api/sso", &a, Some(f)).await;
    assert_eq!(r.body["config"]["has_secret"], true);

    // Switching it off.
    let r = e.send("DELETE", "/ui/api/sso", &a, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(e
        .get("/ui/api/session", &[])
        .await
        .body
        .get("sso")
        .is_none());
    assert_eq!(
        e.get("/ui/api/oidc/start", &[]).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_environment_wins_over_the_web_ui() {
    let e = env(false).await; // configured like by variables
    e.state.lock().unwrap().oidc_env = true;
    let admin = e.session_of("root", true).await;
    let a = [("bookmoor_session", &admin[..])];
    let r = e.get("/ui/api/sso", &a).await;
    assert_eq!(r.body["source"], "environment");
    assert_eq!(r.body["config"]["label"], "Test IdP");
    assert!(!r.body.to_string().contains(SECRET));
    let r = e
        .send("PUT", "/ui/api/sso", &a, Some(e.form(Some(SECRET))))
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = e.send("DELETE", "/ui/api/sso", &a, None).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        e.get("/ui/api/session", &[]).await.body["sso"]["label"],
        "Test IdP"
    );
}

#[test]
fn saved_settings_survive_a_restart() {
    let path = std::env::temp_dir().join(format!("bookmoor-sso-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg = OidcConfig {
        issuer: "https://id.example".into(),
        client_id: "bookmoor".into(),
        client_secret: Some(SECRET.into()),
        public_url: "https://bookmoor.example".into(),
        label: "Office".into(),
        create_users: true,
    };
    let mut st = AppState::open(bookmoor_server::store::Store::new(&path)).unwrap();
    st.apply_oidc(Some(cfg));
    st.persist().unwrap();
    let again = AppState::open(bookmoor_server::store::Store::new(&path)).unwrap();
    let saved = again.oidc_config.as_ref().expect("settings are saved");
    assert_eq!(saved.client_secret.as_deref(), Some(SECRET));
    assert_eq!(saved.label, "Office");
    assert!(
        !format!("{saved:?}").contains(SECRET),
        "Debug hides the secret"
    );
    let _ = std::fs::remove_file(&path);
}
