//! The web UI (`docs/multi-user.md`): static files plus a small JSON API
//! under `/ui/api`, secured by a session cookie. Users manage their own
//! devices and tokens; admin users also manage users.
//!
//! Every write needs the `X-Bookmoor-UI` header on top of the
//! `SameSite=Strict` cookie, so a cross-site form post cannot act as a user.

use crate::api::{retry_after, save, ApiError, AppState, Shared};
use crate::auth::{Scope, TokenRecord};
use crate::ratelimit::client_key;
use crate::users::{check_password, hash_password, verify_password, User};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

const INDEX: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const APP_CSS: &str = include_str!("web/app.css");
const LOGO: &str = include_str!("web/logo.svg");

const COOKIE: &str = "bookmoor_session";
const CSRF_HEADER: &str = "x-bookmoor-ui";
/// Devices (read and sync tokens) one user may have.
const MAX_DEVICES: usize = 50;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", INDEX) }),
        )
        .route(
            "/ui/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/ui/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/ui/logo.svg",
            get(|| async { asset("image/svg+xml", LOGO) }),
        )
        .route("/ui/api/session", get(session))
        .route("/ui/api/setup", post(setup))
        .route("/ui/api/login", post(login))
        .route("/ui/api/logout", post(logout))
        .route("/ui/api/devices", get(devices).post(add_device))
        .route("/ui/api/devices/{id}", delete(remove_device))
        .route("/ui/api/connect", post(connect))
        .route("/ui/api/oidc/start", get(oidc_start))
        .route("/ui/api/oidc/callback", get(oidc_callback))
        .route("/ui/api/oidc/unlink", post(oidc_unlink))
        .route("/ui/api/sso", get(sso_get).put(sso_put).delete(sso_delete))
        .route("/ui/api/sso/test", post(sso_test))
        .route("/ui/api/profiles", get(profiles).post(add_profile))
        .route("/ui/api/profiles/{id}", delete(remove_profile))
        .route("/ui/api/profiles/{id}/rename", post(rename_profile))
        .route("/ui/api/profiles/{id}/clear", post(clear_profile))
        .route("/ui/api/profiles/{id}/dedupe", post(dedupe_profile))
        .route("/ui/api/profiles/{id}/restore", post(restore_profile))
        .route("/ui/api/profiles/{id}/export", get(export_profile))
        .route("/ui/api/profiles/{id}/import", post(import_profile))
        .route("/ui/api/password", post(change_password))
        .route("/ui/api/users", get(users).post(add_user))
        .route("/ui/api/users/{id}", delete(remove_user))
        .route("/ui/api/users/{id}/password", post(reset_password))
        .route("/ui/api/users/{id}/admin", post(set_admin))
        .layer(middleware::from_fn(page_headers))
}

fn asset(kind: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, kind)], body).into_response()
}

/// Strict headers for the UI: nothing but our own files may load or run.
async fn page_headers(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/ui/api/");
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if is_api { "no-store" } else { "no-cache" }),
    );
    resp
}

/// The client address, if the server was started with connect info.
struct Peer(Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(Peer(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| client_key(c.0.ip())),
        ))
    }
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    cookie_named(headers, COOKIE)
}

fn cookie_named(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(&format!("{name}=")).map(String::from))
}

/// A signed-in user, from the session cookie.
struct Authed {
    user: User,
    session: String,
}

impl FromRequestParts<Shared> for Authed {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, s: &Shared) -> Result<Self, ApiError> {
        if !matches!(parts.method, Method::GET | Method::HEAD)
            && !parts.headers.contains_key(CSRF_HEADER)
        {
            return Err(ApiError::Denied("missing request header"));
        }
        let session = cookie_value(&parts.headers).ok_or(ApiError::Unauthorized)?;
        let st = s.lock().unwrap();
        let user = st
            .sessions
            .user_for(&session, Instant::now())
            .and_then(|id| st.users.get(id))
            .cloned()
            .ok_or(ApiError::Unauthorized)?;
        Ok(Authed { user, session })
    }
}

/// A signed-in user who is an admin.
struct Admin(Authed);

impl FromRequestParts<Shared> for Admin {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, s: &Shared) -> Result<Self, ApiError> {
        let a = Authed::from_request_parts(parts, s).await?;
        if a.user.admin {
            Ok(Admin(a))
        } else {
            Err(ApiError::Forbidden)
        }
    }
}

fn set_cookie(token: &str, max_age: u64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}{secure}"
    ))
    .expect("cookie is ascii")
}

#[derive(Serialize)]
struct Me {
    id: String,
    name: String,
    admin: bool,
    /// Linked to a single sign-on account.
    oidc: bool,
}

impl From<&User> for Me {
    fn from(u: &User) -> Me {
        Me {
            id: u.id.clone(),
            name: u.name.clone(),
            admin: u.admin,
            oidc: u.oidc.is_some(),
        }
    }
}

/// The reply that signs a user in.
fn signed_in(st: &mut AppState, user: &User) -> Response {
    let token = st.sessions.create(&user.id, Instant::now());
    let max_age = crate::sessions::SESSION_TTL.as_secs();
    let mut resp =
        Json(serde_json::json!({ "state": "user", "user": Me::from(user) })).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        set_cookie(&token, max_age, st.secure_cookies),
    );
    resp
}

/// Every login-like attempt costs one token of the per-address bucket, so
/// guessing is bounded per address.
fn throttle(st: &mut AppState, peer: Option<IpAddr>) -> Result<(), ApiError> {
    match peer {
        Some(p) => st
            .limits
            .failed_auth
            .take(&p, Instant::now())
            .map_err(retry_after),
        None => Ok(()),
    }
}

/// At most this many Argon2 computations at once: each takes tens of MiB.
fn hashing_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(4))
}

async fn hash_blocking(pw: String) -> Result<String, ApiError> {
    check_password(&pw).map_err(ApiError::Unprocessable)?;
    let _slot = hashing_slots()
        .acquire()
        .await
        .map_err(|_| ApiError::Storage)?;
    tokio::task::spawn_blocking(move || hash_password(&pw))
        .await
        .map_err(|_| ApiError::Storage)?
        .map_err(ApiError::Unprocessable)
}

async fn verify_blocking(hash: Option<String>, pw: String) -> bool {
    if pw.len() > crate::users::MAX_PASSWORD_BYTES {
        return false;
    }
    let Ok(_slot) = hashing_slots().acquire().await else {
        return false;
    };
    tokio::task::spawn_blocking(move || verify_password(hash.as_deref(), &pw))
        .await
        .unwrap_or(false)
}

async fn session(State(s): State<Shared>, headers: HeaderMap) -> Json<serde_json::Value> {
    let st = s.lock().unwrap();
    let user = cookie_value(&headers)
        .and_then(|c| st.sessions.user_for(&c, Instant::now()).map(String::from))
        .and_then(|id| st.users.get(&id));
    let mut out = match user {
        Some(u) => serde_json::json!({ "state": "user", "user": Me::from(u) }),
        None if !st.users.any_login() => serde_json::json!({ "state": "setup" }),
        None => serde_json::json!({ "state": "login" }),
    };
    if let Some(o) = &st.oidc {
        out["sso"] = serde_json::json!({ "label": o.cfg.label });
    }
    Json(out)
}

#[derive(Deserialize)]
struct Login {
    name: String,
    password: String,
}

async fn login(
    State(s): State<Shared>,
    Peer(peer): Peer,
    Json(b): Json<Login>,
) -> Result<Response, ApiError> {
    // Every attempt costs a token, so guessing is bounded per address.
    let found = {
        let mut st = s.lock().unwrap();
        throttle(&mut st, peer)?;
        st.users
            .by_name(&b.name)
            .map(|u| (u.id.clone(), u.password_hash.clone()))
    };
    let ok = verify_blocking(found.as_ref().and_then(|f| f.1.clone()), b.password).await;
    let mut st = s.lock().unwrap();
    match found {
        Some((id, _)) if ok => match st.users.get(&id).cloned() {
            Some(user) => Ok(signed_in(&mut st, &user)),
            None => Err(ApiError::Unauthorized),
        },
        _ => Err(ApiError::Unauthorized),
    }
}

#[derive(Deserialize)]
struct Setup {
    admin_token: String,
    name: String,
    password: String,
}

/// First setup: while nobody can sign in yet, the admin token creates the
/// first admin (or gives a password to an existing user, such as the `owner`
/// left behind by an upgrade).
async fn setup(
    State(s): State<Shared>,
    Peer(peer): Peer,
    Json(b): Json<Setup>,
) -> Result<Response, ApiError> {
    {
        let mut st = s.lock().unwrap();
        throttle(&mut st, peer)?;
        if st.users.any_password() {
            return Err(ApiError::Denied("setup is already done"));
        }
        let ok = st
            .tokens
            .authenticate(b.admin_token.trim())
            .is_some_and(|r| r.scope == Scope::Admin);
        if !ok {
            return Err(ApiError::Unauthorized);
        }
    }
    let hash = hash_blocking(b.password).await?;
    let mut st = s.lock().unwrap();
    if st.users.any_password() {
        return Err(ApiError::Denied("setup is already done"));
    }
    let existing = st.users.by_name(&b.name).map(|u| u.id.clone());
    let user = match existing {
        Some(id) => {
            st.users
                .set_hash(&id, hash)
                .map_err(ApiError::Unprocessable)?;
            st.users
                .set_admin(&id, true)
                .map_err(ApiError::Unprocessable)?;
            st.users.get(&id).cloned().expect("just updated")
        }
        None => st
            .add_user_hashed(&b.name, true, Some(hash))
            .map_err(ApiError::Unprocessable)?,
    };
    save(&mut st)?;
    Ok(signed_in(&mut st, &user))
}

async fn logout(State(s): State<Shared>, a: Authed) -> Response {
    let mut st = s.lock().unwrap();
    st.sessions.end(&a.session);
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, set_cookie("", 0, st.secure_cookies));
    resp
}

#[derive(Serialize)]
struct Device {
    id: String,
    name: String,
    scope: Scope,
    profile_id: Option<String>,
    endpoint_id: Option<String>,
}

impl From<TokenRecord> for Device {
    fn from(t: TokenRecord) -> Device {
        Device {
            id: t.id,
            name: t.name,
            scope: t.scope,
            profile_id: t.profile_id,
            endpoint_id: t.endpoint_id,
        }
    }
}

async fn devices(State(s): State<Shared>, a: Authed) -> Json<Vec<Device>> {
    let st = s.lock().unwrap();
    Json(
        st.tokens
            .for_user(&a.user.id)
            .into_iter()
            .map(Device::from)
            .collect(),
    )
}

#[derive(Deserialize)]
struct NewDevice {
    name: String,
    scope: Scope,
    /// Profile id; may be left out while the user has only one profile.
    profile: Option<String>,
}

async fn add_device(
    State(s): State<Shared>,
    a: Authed,
    Json(b): Json<NewDevice>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if b.scope == Scope::Admin {
        return Err(ApiError::Forbidden);
    }
    let mut st = s.lock().unwrap();
    if st.tokens.count_for_user(&a.user.id) >= MAX_DEVICES {
        return Err(ApiError::Unprocessable(format!(
            "at most {MAX_DEVICES} devices"
        )));
    }
    let (token, rec) = st
        .issue_token(Some(&a.user.id), b.profile.as_deref(), &b.name, b.scope)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "token": token,
            "device": Device::from(rec),
        })),
    ))
}

async fn remove_device(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    let rec = st
        .tokens
        .get(&id)
        .filter(|t| t.user_id.as_deref() == Some(a.user.id.as_str()))
        .cloned()
        .ok_or(ApiError::Forbidden)?;
    st.tokens.revoke(&rec.id);
    if let (Some(ep), Some(e)) = (
        &rec.endpoint_id,
        rec.profile_id.as_ref().and_then(|p| st.engines.get_mut(p)),
    ) {
        e.remove_endpoint(ep);
    }
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Where the browser extension's sign-in may send a new token: the fixed
/// redirect URLs the browsers give an extension (`<id>.chromiumapp.org` in
/// Chrome, `<uuid>.extensions.allizom.org` in Firefox), nothing else. A page
/// that asks for another target never gets a token.
fn extension_redirect_ok(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    if url.len() > 512 || url.chars().any(|c| c.is_control() || c == ' ' || c == '#') {
        return false;
    }
    let host = rest.split(['/', '?']).next().unwrap_or("");
    ["chromiumapp.org", "extensions.allizom.org"]
        .iter()
        .any(|suffix| {
            host.strip_suffix(suffix)
                .and_then(|h| h.strip_suffix('.'))
                .is_some_and(|label| {
                    !label.is_empty()
                        && label
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                })
        })
}

#[derive(Deserialize)]
struct Connect {
    redirect: String,
    state: String,
    name: String,
    profile: Option<String>,
}

/// The extension's "sign in": after the user confirmed on the page, issue a
/// new `sync` token for this device and hand it back in the URL fragment of
/// the extension's redirect, so it reaches no server or proxy log.
async fn connect(
    State(s): State<Shared>,
    a: Authed,
    Json(b): Json<Connect>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !extension_redirect_ok(&b.redirect) {
        return Err(ApiError::Unprocessable(
            "that redirect address does not belong to a browser extension".into(),
        ));
    }
    if b.state.is_empty()
        || b.state.len() > 128
        || !b
            .state
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::Unprocessable("invalid state".into()));
    }
    let mut st = s.lock().unwrap();
    if st.tokens.count_for_user(&a.user.id) >= MAX_DEVICES {
        return Err(ApiError::Unprocessable(format!(
            "at most {MAX_DEVICES} devices"
        )));
    }
    let (token, rec) = st
        .issue_token(Some(&a.user.id), b.profile.as_deref(), &b.name, Scope::Sync)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    Ok(Json(serde_json::json!({
        "url": format!("{}#token={token}&state={}", b.redirect, b.state),
        "device": Device::from(rec),
    })))
}

// ---- single sign-on (OIDC) --------------------------------------------------

const OIDC_COOKIE: &str = "bookmoor_oidc";

/// Where the sign-in leads back to: the UI root (two levels above the
/// callback, so it works below a proxy path prefix), plus the query string
/// the sign-in began with.
fn back_to(ret: &str, error: Option<&str>) -> Response {
    let query = match error {
        Some(e) => format!("?sso_error={e}"),
        None => ret.to_string(),
    };
    let mut resp = StatusCode::SEE_OTHER.into_response();
    resp.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(&format!("../../{query}")).expect("ascii"),
    );
    resp
}

fn oidc_cookie(value: &str, max_age: u64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    // Lax: the browser comes back from the identity provider with a top-level GET.
    HeaderValue::from_str(&format!(
        "{OIDC_COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/ui/api/oidc; Max-Age={max_age}{secure}"
    ))
    .expect("cookie is ascii")
}

#[derive(Deserialize)]
struct OidcStart {
    /// `1`: link the signed-in user's account instead of signing in.
    link: Option<String>,
    #[serde(rename = "return", default)]
    ret: String,
}

async fn oidc_start(
    State(s): State<Shared>,
    Peer(peer): Peer,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<OidcStart>,
) -> Response {
    let (oidc, link_user, secure) = {
        let mut st = s.lock().unwrap();
        // Each start costs memory and a request to the provider, so it costs a token too.
        if throttle(&mut st, peer).is_err() {
            return back_to("", Some("rate"));
        }
        let link_user = if q.link.is_some() {
            let uid = cookie_value(&headers)
                .and_then(|c| st.sessions.user_for(&c, Instant::now()).map(String::from));
            if uid.is_none() {
                return back_to("", Some("session"));
            }
            uid
        } else {
            None
        };
        (st.oidc.clone(), link_user, st.secure_cookies)
    };
    let Some(oidc) = oidc else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !crate::oidc::return_ok(&q.ret) {
        return back_to("", Some("request"));
    }
    match oidc.start(link_user, q.ret).await {
        Ok(started) => {
            s.lock()
                .unwrap()
                .oidc_flows
                .insert(started.state, started.flow, Instant::now());
            let mut resp = StatusCode::SEE_OTHER.into_response();
            resp.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_str(&started.url).expect("url is ascii"),
            );
            resp.headers_mut().insert(
                header::SET_COOKIE,
                oidc_cookie(&started.cookie, 600, secure),
            );
            resp
        }
        Err(e) => {
            eprintln!("single sign-on: {e}");
            back_to("", Some("provider"))
        }
    }
}

#[derive(Deserialize)]
struct OidcCallback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn oidc_callback(
    State(s): State<Shared>,
    Peer(peer): Peer,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<OidcCallback>,
) -> Response {
    let (oidc, flow, secure) = {
        let mut st = s.lock().unwrap();
        if throttle(&mut st, peer).is_err() {
            return back_to("", Some("rate"));
        }
        let flow = match (&q.state, cookie_named(&headers, OIDC_COOKIE)) {
            (Some(state), Some(cookie)) => st.oidc_flows.take(state, &cookie, Instant::now()),
            _ => None,
        };
        (st.oidc.clone(), flow, st.secure_cookies)
    };
    let mut resp = match (oidc, flow) {
        (Some(oidc), Some(flow)) if q.error.is_none() && q.code.is_some() => {
            let ret = flow.ret.clone();
            let link_user = flow.link_user.clone();
            match oidc.finish(flow, q.code.unwrap_or_default()).await {
                Ok(v) => finish_sso(&s, &oidc, v, link_user, &ret),
                Err(e) => {
                    eprintln!("single sign-on: {e}");
                    back_to("", Some("provider"))
                }
            }
        }
        (Some(_), Some(_)) => back_to("", Some("denied")),
        _ => back_to("", Some("expired")),
    };
    resp.headers_mut()
        .append(header::SET_COOKIE, oidc_cookie("", 0, secure));
    resp
}

/// Turn a verified identity into a session (or link it to the signed-in user).
fn finish_sso(
    s: &Shared,
    oidc: &crate::oidc::Oidc,
    v: crate::oidc::Verified,
    link_user: Option<String>,
    ret: &str,
) -> Response {
    let mut st = s.lock().unwrap();
    if let Some(uid) = link_user {
        return match st.users.set_oidc(&uid, Some(v.identity)) {
            Ok(()) if save(&mut st).is_ok() => back_to("", Some("linked")),
            Ok(()) => back_to("", Some("save")),
            Err(_) => back_to("", Some("taken")),
        };
    }
    let user = match st.users.by_identity(&v.identity).cloned() {
        Some(u) => u,
        None if oidc.cfg.create_users => {
            let name = crate::oidc::user_name_from(v.username.as_deref(), |n| {
                st.users.by_name(n).is_some()
            });
            let created = st
                .users
                .create_hashed(&name, false, None)
                .and_then(|u| st.users.set_oidc(&u.id, Some(v.identity)).map(|()| u.id));
            match created.map(|id| st.users.get(&id).cloned()) {
                Ok(Some(u)) if save(&mut st).is_ok() => u,
                _ => return back_to("", Some("create")),
            }
        }
        None => return back_to("", Some("unlinked")),
    };
    let mut resp = back_to(ret, None);
    let token = st.sessions.create(&user.id, Instant::now());
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(
            &token,
            crate::sessions::SESSION_TTL.as_secs(),
            st.secure_cookies,
        ),
    );
    resp
}

/// Remove the link to the single sign-on account; only while a password
/// remains, so nobody locks themselves out.
async fn oidc_unlink(State(s): State<Shared>, a: Authed) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    if a.user.password_hash.is_none() {
        return Err(ApiError::Unprocessable(
            "set a password first, or you could not sign in any more".into(),
        ));
    }
    st.users
        .set_oidc(&a.user.id, None)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- single sign-on settings (admins) ---------------------------------------

#[derive(Serialize)]
struct SsoShown {
    issuer: String,
    client_id: String,
    public_url: String,
    label: String,
    create_users: bool,
    /// Whether a client secret is stored. It is never sent back.
    has_secret: bool,
}

impl From<&crate::oidc::OidcConfig> for SsoShown {
    fn from(c: &crate::oidc::OidcConfig) -> Self {
        SsoShown {
            issuer: c.issuer.clone(),
            client_id: c.client_id.clone(),
            public_url: c.public_url.clone(),
            label: c.label.clone(),
            create_users: c.create_users,
            has_secret: c.client_secret.is_some(),
        }
    }
}

/// `environment`: set by variables or flags, read-only here and winning over
/// the settings. `settings`: set up in the web UI. `none`: single sign-on is off.
async fn sso_get(State(s): State<Shared>, _: Admin) -> Json<serde_json::Value> {
    Json(sso_view(&s.lock().unwrap()))
}

fn sso_view(st: &AppState) -> serde_json::Value {
    let source = if st.oidc_env {
        "environment"
    } else if st.oidc.is_some() || st.oidc_config.is_some() {
        "settings"
    } else {
        "none"
    };
    // What is in effect, else what is saved.
    let shown = if st.oidc_env {
        st.oidc.as_ref().map(|o| SsoShown::from(&o.cfg))
    } else {
        st.oidc_config.as_ref().map(SsoShown::from)
    };
    serde_json::json!({
        "source": source,
        // Saved settings the server could not use, so single sign-on is off.
        "broken": !st.oidc_env && st.oidc.is_none() && st.oidc_config.is_some(),
        "config": shown,
        "callback_url": shown_callback(st),
        // Saved settings that the environment overrides.
        "overridden": st.oidc_env && st.oidc_config.is_some(),
    })
}

fn shown_callback(st: &AppState) -> Option<String> {
    let url = if st.oidc_env {
        st.oidc.as_ref().map(|o| o.cfg.public_url.clone())
    } else {
        st.oidc_config.as_ref().map(|c| c.public_url.clone())
    }?;
    Some(crate::oidc::callback_url(&url))
}

#[derive(Deserialize)]
struct SsoBody {
    issuer: String,
    client_id: String,
    /// Absent or empty keeps the saved secret (if the provider is unchanged).
    #[serde(default)]
    client_secret: Option<String>,
    /// Remove the saved secret (a public client).
    #[serde(default)]
    clear_secret: bool,
    public_url: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    create_users: bool,
}

/// The settings a form describes. A saved secret carries over only to the
/// same issuer and client ID, never to another provider.
fn sso_config(st: &AppState, b: SsoBody) -> Result<crate::oidc::OidcConfig, ApiError> {
    let issuer = b.issuer.trim().to_string();
    let client_id = b.client_id.trim().to_string();
    let typed = b.client_secret.filter(|v| !v.is_empty());
    // With the environment in charge, the saved settings are not in use.
    let saved = st
        .oidc_config
        .as_ref()
        .filter(|_| !st.oidc_env)
        .filter(|c| c.issuer == issuer && c.client_id == client_id)
        .and_then(|c| c.client_secret.clone());
    // A secret typed in wins over "remove the saved one".
    let client_secret = typed.or(if b.clear_secret { None } else { saved });
    Ok(crate::oidc::OidcConfig {
        issuer,
        client_id,
        client_secret,
        public_url: b.public_url,
        label: b.label,
        create_users: b.create_users,
    })
}

fn not_from_environment(st: &AppState) -> Result<(), ApiError> {
    if st.oidc_env {
        return Err(ApiError::Unprocessable(
            "single sign-on is set by the server's environment or flags; remove them to manage it here"
                .into(),
        ));
    }
    Ok(())
}

async fn sso_put(
    State(s): State<Shared>,
    Admin(a): Admin,
    Json(b): Json<SsoBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    not_from_environment(&st)?;
    let cfg = sso_config(&st, b)?;
    // Same checks as at start-up, so what is saved can always be loaded again.
    crate::oidc::Oidc::new(cfg.clone()).map_err(ApiError::Unprocessable)?;
    let before = st.oidc_config.clone();
    st.apply_oidc(Some(cfg));
    if let Err(e) = save(&mut st) {
        // persist_or_revert reloads from disk when there is a store; without
        // one the old settings have to be put back by hand.
        if st.store.is_none() {
            st.apply_oidc(before);
        }
        return Err(e);
    }
    Ok(Json(sso_view(&st)))
}

async fn sso_delete(State(s): State<Shared>, Admin(a): Admin) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    not_from_environment(&st)?;
    let before = st.oidc_config.clone();
    st.apply_oidc(None);
    if let Err(e) = save(&mut st) {
        if st.store.is_none() {
            st.apply_oidc(before);
        }
        return Err(e);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Contact the provider with the settings in the form (nothing is saved).
async fn sso_test(
    State(s): State<Shared>,
    _: Admin,
    Json(b): Json<SsoBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cfg = {
        let st = s.lock().unwrap();
        sso_config(&st, b)?
    };
    let oidc = crate::oidc::Oidc::new(cfg).map_err(ApiError::Unprocessable)?;
    let endpoint = oidc.check().await.map_err(ApiError::Unprocessable)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "authorization_endpoint": endpoint,
        "callback_url": crate::oidc::callback_url(&oidc.cfg.public_url),
    })))
}

#[derive(Serialize)]
struct ProfileRow {
    id: String,
    name: String,
    devices: usize,
    bookmarks: usize,
    /// The tree before the last clear or tidy-up, if it can be restored.
    backup: Option<BackupInfo>,
}

#[derive(Serialize)]
struct BackupInfo {
    at: String,
    bookmarks: usize,
}

async fn profiles(State(s): State<Shared>, a: Authed) -> Json<Vec<ProfileRow>> {
    let st = s.lock().unwrap();
    let counts = st.tokens.counts_by_profile();
    Json(
        st.profiles
            .for_user(&a.user.id)
            .into_iter()
            .map(|p| ProfileRow {
                id: p.id.clone(),
                name: p.name.clone(),
                devices: counts.get(p.id.as_str()).copied().unwrap_or(0),
                bookmarks: st.engines.get(&p.id).map_or(0, |e| e.bookmark_count()),
                backup: st.backups.get(&p.id).map(|b| BackupInfo {
                    at: b.at.clone(),
                    bookmarks: b.bookmarks(),
                }),
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct ProfileName {
    name: String,
}

async fn add_profile(
    State(s): State<Shared>,
    a: Authed,
    Json(b): Json<ProfileName>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    st.add_profile(&a.user.id, &b.name)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    Ok(StatusCode::CREATED)
}

async fn rename_profile(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
    Json(b): Json<ProfileName>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    if !st.profiles.get(&id).is_some_and(|p| p.user_id == a.user.id) {
        return Err(ApiError::Forbidden);
    }
    st.profiles
        .rename(&id, &b.name)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Applies `change` to one of the caller's own profiles, saves, and tells
/// the devices of the profile (they resync) and the Dynacat file.
fn change_tree<T>(
    s: &Shared,
    a: &Authed,
    id: &str,
    change: impl FnOnce(&mut crate::api::AppState, &str) -> Result<T, String>,
) -> Result<T, ApiError> {
    let mut st = s.lock().unwrap();
    if !st.profiles.get(id).is_some_and(|p| p.user_id == a.user.id) {
        return Err(ApiError::Forbidden);
    }
    let out = change(&mut st, id).map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    if let Err(e) = st.publish_dynacat(Some(id)) {
        eprintln!("warning: cannot write the Dynacat file: {e}");
    }
    Ok(out)
}

/// Removes every bookmark of the profile. Its devices follow on their next
/// sync; the old tree stays restorable.
async fn clear_profile(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    change_tree(&s, &a, &id, |st, id| st.clear_profile(id))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct Tidied {
    bookmarks_removed: usize,
    folders_merged: usize,
}

/// Merges same-named folders and drops repeated URLs within a folder.
async fn dedupe_profile(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
) -> Result<Json<Tidied>, ApiError> {
    let done = change_tree(&s, &a, &id, |st, id| st.dedupe_profile(id))?;
    Ok(Json(Tidied {
        bookmarks_removed: done.bookmarks_removed,
        folders_merged: done.folders_merged,
    }))
}

/// The file a profile is downloaded as and imported from.
#[derive(Serialize, Deserialize)]
struct ProfileFile {
    format: String,
    version: u32,
    exported_at: Option<String>,
    nodes: Vec<bookmoor_core::wire::WireNode>,
}

const FILE_FORMAT: &str = "bookmoor-profile";

/// The profile's whole tree as a JSON file.
async fn export_profile(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let st = s.lock().unwrap();
    let p = st
        .profiles
        .get(&id)
        .filter(|p| p.user_id == a.user.id)
        .ok_or(ApiError::Forbidden)?;
    let nodes = st.engines.get(&id).ok_or(ApiError::Forbidden)?.tree().nodes;
    let file = ProfileFile {
        format: FILE_FORMAT.into(),
        version: 1,
        exported_at: Some(crate::api::utc_now()),
        nodes,
    };
    let name: String = p
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut resp = Json(file).into_response();
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"bookmoor-{name}.json\"")
            .parse()
            .unwrap(),
    );
    Ok(resp)
}

/// Replaces the profile's tree by a downloaded file; the devices follow.
async fn import_profile(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
    Json(f): Json<ProfileFile>,
) -> Result<StatusCode, ApiError> {
    if f.format != FILE_FORMAT || f.version != 1 {
        return Err(ApiError::Unprocessable(
            "this is not a bookmoor profile file".into(),
        ));
    }
    change_tree(&s, &a, &id, |st, id| st.import_profile(id, &f.nodes))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Brings back the tree from before the last clear or tidy-up.
async fn restore_profile(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    change_tree(&s, &a, &id, |st, id| st.restore_profile(id))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes the profile's bookmarks and revokes its devices.
async fn remove_profile(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    match st
        .remove_profile(&a.user.id, &id)
        .map_err(ApiError::Unprocessable)?
    {
        true => save(&mut st)?,
        false => return Err(ApiError::Forbidden),
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ChangePassword {
    current: String,
    new: String,
}

async fn change_password(
    State(s): State<Shared>,
    Peer(peer): Peer,
    a: Authed,
    Json(b): Json<ChangePassword>,
) -> Result<Response, ApiError> {
    {
        let mut st = s.lock().unwrap();
        throttle(&mut st, peer)?;
    }
    if !verify_blocking(a.user.password_hash.clone(), b.current).await {
        return Err(ApiError::Denied("the current password is wrong"));
    }
    let hash = hash_blocking(b.new).await?;
    let mut st = s.lock().unwrap();
    st.users
        .set_hash(&a.user.id, hash)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    // Other browsers are signed out; this one gets a fresh session.
    st.sessions.end_user(&a.user.id);
    let user = st
        .users
        .get(&a.user.id)
        .cloned()
        .ok_or(ApiError::Unauthorized)?;
    Ok(signed_in(&mut st, &user))
}

#[derive(Serialize)]
struct UserRow {
    id: String,
    name: String,
    admin: bool,
    has_password: bool,
    devices: usize,
}

async fn users(State(s): State<Shared>, _: Admin) -> Json<Vec<UserRow>> {
    let st = s.lock().unwrap();
    let counts = st.tokens.counts_by_user();
    Json(
        st.users
            .list()
            .map(|u| UserRow {
                id: u.id.clone(),
                name: u.name.clone(),
                admin: u.admin,
                has_password: u.password_hash.is_some(),
                devices: counts.get(u.id.as_str()).copied().unwrap_or(0),
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct NewUser {
    name: String,
    password: String,
    #[serde(default)]
    admin: bool,
}

async fn add_user(
    State(s): State<Shared>,
    Admin(a): Admin,
    Json(b): Json<NewUser>,
) -> Result<StatusCode, ApiError> {
    let hash = hash_blocking(b.password).await?;
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    st.add_user_hashed(&b.name, b.admin, Some(hash))
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    Ok(StatusCode::CREATED)
}

/// The extractor checked admin rights on a copy; check again under the lock
/// the change runs in, so two admins cannot undo each other's rights.
fn still_admin(st: &AppState, a: &Authed) -> Result<(), ApiError> {
    if st.users.get(&a.user.id).is_some_and(|u| u.admin) {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

async fn remove_user(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if id == a.user.id {
        return Err(ApiError::Denied("you cannot remove yourself"));
    }
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    if st.remove_user(&id) {
        save(&mut st)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Password {
    password: String,
}

async fn reset_password(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
    Json(b): Json<Password>,
) -> Result<StatusCode, ApiError> {
    let hash = hash_blocking(b.password).await?;
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    st.users
        .set_hash(&id, hash)
        .map_err(ApiError::Unprocessable)?;
    st.sessions.end_user(&id);
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Role {
    admin: bool,
}

async fn set_admin(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
    Json(b): Json<Role>,
) -> Result<StatusCode, ApiError> {
    if id == a.user.id {
        return Err(ApiError::Denied("you cannot change your own role"));
    }
    let mut st = s.lock().unwrap();
    st.users
        .set_admin(&id, b.admin)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::extension_redirect_ok;
    use crate::api::{router, AppState, Shared};
    use crate::auth::Scope;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    struct Ui {
        app: axum::Router,
        state: Shared,
        admin: String,
    }

    fn ui() -> Ui {
        let mut st = AppState::default();
        let (admin, _) = st.tokens.create("admin", Scope::Admin, None, None, None);
        let state = Arc::new(Mutex::new(st));
        Ui {
            app: router(state.clone()),
            state,
            admin,
        }
    }

    impl Ui {
        async fn call(
            &self,
            method: &str,
            uri: &str,
            cookie: Option<&str>,
            body: Option<serde_json::Value>,
            csrf: bool,
        ) -> (StatusCode, serde_json::Value, Option<String>) {
            let mut b = Request::builder().method(method).uri(uri);
            if let Some(c) = cookie {
                b = b.header("cookie", format!("bookmoor_session={c}"));
            }
            if csrf {
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
            let set = resp
                .headers()
                .get("set-cookie")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
                set,
            )
        }

        async fn post(
            &self,
            uri: &str,
            c: Option<&str>,
            body: serde_json::Value,
        ) -> (StatusCode, serde_json::Value, Option<String>) {
            self.call("POST", uri, c, Some(body), true).await
        }

        /// Set up the first admin and return the session cookie value.
        async fn setup(&self, name: &str) -> String {
            let (st, _, set) = self
                .post(
                    "/ui/api/setup",
                    None,
                    serde_json::json!({"admin_token": self.admin, "name": name, "password": "correct horse battery"}),
                )
                .await;
            assert_eq!(st, StatusCode::OK);
            cookie_of(&set.unwrap())
        }
    }

    fn cookie_of(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .unwrap()
            .trim_start_matches("bookmoor_session=")
            .to_string()
    }

    #[tokio::test]
    async fn setup_then_login_then_manage_devices() {
        let u = ui();
        let (_, v, _) = u.call("GET", "/ui/api/session", None, None, false).await;
        assert_eq!(v["state"], "setup");
        // Wrong token, then a real one.
        let bad = serde_json::json!({"admin_token":"bkm_a_b","name":"anna","password":"correct horse battery"});
        assert_eq!(
            u.post("/ui/api/setup", None, bad).await.0,
            StatusCode::UNAUTHORIZED
        );
        let cookie = u.setup("anna").await;
        // Setup closes once someone can sign in.
        let again = serde_json::json!({"admin_token": u.admin, "name":"eve","password":"correct horse battery"});
        assert_eq!(
            u.post("/ui/api/setup", None, again).await.0,
            StatusCode::FORBIDDEN
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/session", Some(&cookie), None, false)
            .await;
        assert_eq!(
            (v["state"].as_str(), v["user"]["name"].as_str()),
            (Some("user"), Some("anna"))
        );
        let (_, v, _) = u.call("GET", "/ui/api/session", None, None, false).await;
        assert_eq!(v["state"], "login");

        // A device token created in the UI syncs on the API.
        let (st, v, _) = u
            .post(
                "/ui/api/devices",
                Some(&cookie),
                serde_json::json!({"name":"laptop","scope":"sync"}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED);
        let token = v["token"].as_str().unwrap().to_string();
        let ep = v["device"]["endpoint_id"].as_str().unwrap().to_string();
        let sync = Request::builder()
            .method("POST")
            .uri("/v1/sync")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"endpoint_id": ep, "since_rev": 0, "ops": []}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            u.app.clone().oneshot(sync).await.unwrap().status(),
            StatusCode::OK
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/devices", Some(&cookie), None, false)
            .await;
        assert_eq!(v.as_array().unwrap().len(), 1);
        // Revoking kills the token.
        let id = v[0]["id"].as_str().unwrap();
        let (st, _, _) = u
            .call(
                "DELETE",
                &format!("/ui/api/devices/{id}"),
                Some(&cookie),
                None,
                true,
            )
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let tree = Request::builder()
            .uri("/v1/tree")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            u.app.clone().oneshot(tree).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        // Admin tokens cannot be minted from the UI.
        let (st, _, _) = u
            .post(
                "/ui/api/devices",
                Some(&cookie),
                serde_json::json!({"name":"x","scope":"admin"}),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn login_logout_and_cookie_flags() {
        let u = ui();
        u.setup("anna").await;
        let bad = serde_json::json!({"name":"anna","password":"wrong wrong wrong"});
        assert_eq!(
            u.post("/ui/api/login", None, bad).await.0,
            StatusCode::UNAUTHORIZED
        );
        let ghost = serde_json::json!({"name":"nobody","password":"correct horse battery"});
        assert_eq!(
            u.post("/ui/api/login", None, ghost).await.0,
            StatusCode::UNAUTHORIZED
        );
        u.state.lock().unwrap().secure_cookies = true;
        let ok = serde_json::json!({"name":"ANNA","password":"correct horse battery"});
        let (st, _, set) = u.post("/ui/api/login", None, ok).await;
        assert_eq!(st, StatusCode::OK);
        let set = set.unwrap();
        for flag in ["HttpOnly", "SameSite=Strict", "Secure", "Path=/"] {
            assert!(set.contains(flag), "{set}");
        }
        let cookie = cookie_of(&set);
        let (st, _, _) = u
            .call("POST", "/ui/api/logout", Some(&cookie), None, true)
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (st, _, _) = u
            .call("GET", "/ui/api/devices", Some(&cookie), None, false)
            .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn writes_need_the_request_header() {
        let u = ui();
        let cookie = u.setup("anna").await;
        let body = serde_json::json!({"name":"laptop","scope":"sync"});
        let (st, _, _) = u
            .call("POST", "/ui/api/devices", Some(&cookie), Some(body), false)
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn only_admins_manage_users_and_not_themselves() {
        let u = ui();
        let admin = u.setup("anna").await;
        let new = serde_json::json!({"name":"bob","password":"another good password"});
        assert_eq!(
            u.post("/ui/api/users", Some(&admin), new).await.0,
            StatusCode::CREATED
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/users", Some(&admin), None, false)
            .await;
        let bob_id = v
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "bob")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let anna_id = v
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "anna")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let login = serde_json::json!({"name":"bob","password":"another good password"});
        let (_, _, set) = u.post("/ui/api/login", None, login).await;
        let bob = cookie_of(&set.unwrap());
        // Bob is no admin.
        assert_eq!(
            u.call("GET", "/ui/api/users", Some(&bob), None, false)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let pw = serde_json::json!({"password":"hijacked password"});
        assert_eq!(
            u.post(&format!("/ui/api/users/{anna_id}/password"), Some(&bob), pw)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        // Bob's devices are his own: anna cannot revoke them, and vice versa.
        let (_, v, _) = u
            .post(
                "/ui/api/devices",
                Some(&bob),
                serde_json::json!({"name":"phone","scope":"read"}),
            )
            .await;
        let dev = v["device"]["id"].as_str().unwrap().to_string();
        let (st, _, _) = u
            .call(
                "DELETE",
                &format!("/ui/api/devices/{dev}"),
                Some(&admin),
                None,
                true,
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        // Admin cannot remove or demote themselves.
        let (st, _, _) = u
            .call(
                "DELETE",
                &format!("/ui/api/users/{anna_id}"),
                Some(&admin),
                None,
                true,
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _, _) = u
            .post(
                &format!("/ui/api/users/{anna_id}/admin"),
                Some(&admin),
                serde_json::json!({"admin": false}),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        // Removing bob ends his session.
        let (st, _, _) = u
            .call(
                "DELETE",
                &format!("/ui/api/users/{bob_id}"),
                Some(&admin),
                None,
                true,
            )
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        assert_eq!(
            u.call("GET", "/ui/api/devices", Some(&bob), None, false)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn password_change_signs_other_sessions_out() {
        let u = ui();
        let first = u.setup("anna").await;
        let login = serde_json::json!({"name":"anna","password":"correct horse battery"});
        let (_, _, set) = u.post("/ui/api/login", None, login).await;
        let second = cookie_of(&set.unwrap());
        let wrong = serde_json::json!({"current":"not it at all","new":"a brand new password"});
        assert_eq!(
            u.post("/ui/api/password", Some(&first), wrong).await.0,
            StatusCode::FORBIDDEN
        );
        let change =
            serde_json::json!({"current":"correct horse battery","new":"a brand new password"});
        let (st, _, set) = u.post("/ui/api/password", Some(&first), change).await;
        assert_eq!(st, StatusCode::OK);
        let fresh = cookie_of(&set.unwrap());
        assert_eq!(
            u.call("GET", "/ui/api/devices", Some(&second), None, false)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            u.call("GET", "/ui/api/devices", Some(&fresh), None, false)
                .await
                .0,
            StatusCode::OK
        );
        let old = serde_json::json!({"name":"anna","password":"correct horse battery"});
        assert_eq!(
            u.post("/ui/api/login", None, old).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn upgrade_owner_claims_the_old_tree_in_setup() {
        let mut st = AppState::default();
        st.add_user("owner", true, None).unwrap();
        let (admin, _) = st.tokens.create("admin", Scope::Admin, None, None, None);
        let u = Ui {
            app: router(Arc::new(Mutex::new(st))),
            state: Arc::new(Mutex::new(AppState::default())),
            admin,
        };
        let cookie = u.setup("owner").await;
        let (_, v, _) = u
            .call("GET", "/ui/api/session", Some(&cookie), None, false)
            .await;
        assert_eq!(v["user"]["name"], "owner");
    }

    #[tokio::test]
    async fn pages_are_served_with_a_strict_policy() {
        let u = ui();
        for path in ["/", "/ui/app.js", "/ui/app.css", "/ui/logo.svg"] {
            let r = u
                .app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK, "{path}");
            let csp = r.headers()["content-security-policy"].to_str().unwrap();
            assert!(
                csp.contains("default-src 'none'") && !csp.contains("unsafe"),
                "{csp}"
            );
            assert_eq!(r.headers()["x-frame-options"], "DENY");
        }
    }

    #[tokio::test]
    async fn profiles_are_managed_per_user_and_devices_pick_one() {
        let u = ui();
        let anna = u.setup("anna").await;
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        let home = v[0]["id"].as_str().unwrap().to_string();
        assert_eq!(v[0]["name"], "Default");
        let (st, _, _) = u
            .post(
                "/ui/api/profiles",
                Some(&anna),
                serde_json::json!({"name":"Work"}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED);
        let dup = serde_json::json!({"name":"work"});
        assert_eq!(
            u.post("/ui/api/profiles", Some(&anna), dup).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        let work = v
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "Work")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        // With two profiles a device has to name one.
        let no = serde_json::json!({"name":"laptop","scope":"sync"});
        assert_eq!(
            u.post("/ui/api/devices", Some(&anna), no).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let yes = serde_json::json!({"name":"laptop","scope":"sync","profile":work});
        let (st, v, _) = u.post("/ui/api/devices", Some(&anna), yes).await;
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(v["device"]["profile_id"], work.as_str());
        let token = v["token"].as_str().unwrap().to_string();
        // Bob can neither see nor touch anna's profiles.
        let new = serde_json::json!({"name":"bob","password":"another good password"});
        u.post("/ui/api/users", Some(&anna), new).await;
        let login = serde_json::json!({"name":"bob","password":"another good password"});
        let bob = cookie_of(&u.post("/ui/api/login", None, login).await.2.unwrap());
        let rename = serde_json::json!({"name":"Mine now"});
        assert_eq!(
            u.post(
                &format!("/ui/api/profiles/{work}/rename"),
                Some(&bob),
                rename
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            u.call(
                "DELETE",
                &format!("/ui/api/profiles/{work}"),
                Some(&bob),
                None,
                true
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let steal = serde_json::json!({"name":"x","scope":"read","profile":work});
        assert_eq!(
            u.post("/ui/api/devices", Some(&bob), steal).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        // Rename, then remove: the device token stops working.
        let rename = serde_json::json!({"name":"Job"});
        assert_eq!(
            u.post(
                &format!("/ui/api/profiles/{work}/rename"),
                Some(&anna),
                rename
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        let (st, _, _) = u
            .call(
                "DELETE",
                &format!("/ui/api/profiles/{work}"),
                Some(&anna),
                None,
                true,
            )
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let tree = Request::builder()
            .uri("/v1/tree")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            u.app.clone().oneshot(tree).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        // The last profile stays.
        let (st, _, _) = u
            .call(
                "DELETE",
                &format!("/ui/api/profiles/{home}"),
                Some(&anna),
                None,
                true,
            )
            .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn profiles_can_be_tidied_cleared_and_restored() {
        let u = ui();
        let anna = u.setup("anna").await;
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        let pid = v[0]["id"].as_str().unwrap().to_string();
        assert!(v[0]["backup"].is_null());
        let dev = serde_json::json!({"name":"laptop","scope":"sync"});
        let (_, v, _) = u.post("/ui/api/devices", Some(&anna), dev).await;
        let token = v["token"].as_str().unwrap().to_string();
        let bearer = |m: &str, uri: &str, body: serde_json::Value| {
            Request::builder()
                .method(m)
                .uri(uri.to_string())
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let send = |req: Request<Body>| {
            let app = u.app.clone();
            async move {
                let res = app.oneshot(req).await.unwrap();
                let st = res.status();
                let b = axum::body::to_bytes(res.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (
                    st,
                    serde_json::from_slice::<serde_json::Value>(&b).unwrap_or_default(),
                )
            }
        };
        let (_, ep) = send(bearer("POST", "/v1/endpoints", serde_json::json!({}))).await;
        let ep = ep["endpoint_id"].as_str().unwrap().to_string();
        let add = |n: &str, op: &str| {
            serde_json::json!({"op_id": op, "type":"add", "id": n, "parent":"toolbar",
                "position": if n == "b1" { "V" } else { "W" }, "kind":"bookmark",
                "title": n, "url":"https://same.example"})
        };
        let sync = serde_json::json!({"endpoint_id": ep, "since_rev": 0,
            "ops": [add("b1", "o1"), add("b2", "o2")]});
        send(bearer("POST", "/v1/sync", sync)).await;

        // Two copies of one link: tidy up keeps one and a way back.
        let (st, done, _) = (
            u.call(
                "POST",
                &format!("/ui/api/profiles/{pid}/dedupe"),
                Some(&anna),
                None,
                true,
            )
            .await
            .0,
            u.call("GET", "/ui/api/profiles", Some(&anna), None, false)
                .await
                .1,
            0,
        );
        assert_eq!(st, StatusCode::OK);
        assert_eq!(done[0]["bookmarks"], 1);
        assert_eq!(done[0]["backup"]["bookmarks"], 2);
        // The device must resync; the cursor it holds is no longer valid.
        let (_, r) = send(bearer(
            "POST",
            "/v1/sync",
            serde_json::json!({"endpoint_id": ep, "since_rev": 2, "ops": []}),
        ))
        .await;
        assert_eq!(r["resync"], true);

        // Clear, then bring everything back.
        let clear = u
            .call(
                "POST",
                &format!("/ui/api/profiles/{pid}/clear"),
                Some(&anna),
                None,
                true,
            )
            .await;
        assert_eq!(clear.0, StatusCode::NO_CONTENT);
        let (_, t) = send(bearer("GET", "/v1/tree", serde_json::json!({}))).await;
        assert_eq!(t["nodes"].as_array().unwrap().len(), 4);
        let restore = u
            .call(
                "POST",
                &format!("/ui/api/profiles/{pid}/restore"),
                Some(&anna),
                None,
                true,
            )
            .await;
        assert_eq!(restore.0, StatusCode::NO_CONTENT);
        let (_, t) = send(bearer("GET", "/v1/tree", serde_json::json!({}))).await;
        assert_eq!(t["nodes"].as_array().unwrap().len(), 5);

        // Others cannot touch it; without a backup there is nothing to restore.
        let new = serde_json::json!({"name":"bob","password":"another good password"});
        u.post("/ui/api/users", Some(&anna), new).await;
        let login = serde_json::json!({"name":"bob","password":"another good password"});
        let bob = cookie_of(&u.post("/ui/api/login", None, login).await.2.unwrap());
        for what in ["clear", "dedupe", "restore"] {
            let r = u
                .call(
                    "POST",
                    &format!("/ui/api/profiles/{pid}/{what}"),
                    Some(&bob),
                    None,
                    true,
                )
                .await;
            assert_eq!(r.0, StatusCode::FORBIDDEN, "{what}");
        }
    }

    #[tokio::test]
    async fn a_profile_can_be_downloaded_and_imported_again() {
        let u = ui();
        let anna = u.setup("anna").await;
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        let pid = v[0]["id"].as_str().unwrap().to_string();
        let (st, file, _) = u
            .call(
                "GET",
                &format!("/ui/api/profiles/{pid}/export"),
                Some(&anna),
                None,
                false,
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(file["format"], "bookmoor-profile");
        // Add a bookmark to the file and import it.
        let mut edited = file.clone();
        edited["nodes"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
            "id":"n_1","parent":"toolbar","position":"V","kind":"bookmark",
            "title":"Rust","url":"https://rust-lang.org"}));
        let imp = format!("/ui/api/profiles/{pid}/import");
        assert_eq!(
            u.post(&imp, Some(&anna), edited).await.0,
            StatusCode::NO_CONTENT
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        assert_eq!(v[0]["bookmarks"], 1);
        // Broken or foreign files change nothing.
        let mut broken = file.clone();
        broken["nodes"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
            "id":"n_2","parent":"nowhere","position":"V","kind":"folder","title":"x"}));
        assert_eq!(
            u.post(&imp, Some(&anna), broken).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let mut foreign = file.clone();
        foreign["format"] = "something".into();
        assert_eq!(
            u.post(&imp, Some(&anna), foreign).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        assert_eq!(v[0]["bookmarks"], 1);
        // The previous (empty) tree is not a backup, but a second import is.
        assert_eq!(
            u.post(&imp, Some(&anna), file).await.0,
            StatusCode::NO_CONTENT
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        assert_eq!(
            (
                v[0]["bookmarks"].as_u64(),
                v[0]["backup"]["bookmarks"].as_u64()
            ),
            (Some(0), Some(1))
        );
        // Not for other users.
        let new = serde_json::json!({"name":"bob","password":"another good password"});
        u.post("/ui/api/users", Some(&anna), new).await;
        let login = serde_json::json!({"name":"bob","password":"another good password"});
        let bob = cookie_of(&u.post("/ui/api/login", None, login).await.2.unwrap());
        let (st, _, _) = u
            .call(
                "GET",
                &format!("/ui/api/profiles/{pid}/export"),
                Some(&bob),
                None,
                false,
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }
    #[test]
    fn only_extension_redirects_are_allowed() {
        for ok in [
            "https://abcdefghijklmnop.chromiumapp.org/",
            "https://abcdefghijklmnop.chromiumapp.org/cb?x=1",
            "https://0123abcd-45ef.extensions.allizom.org/",
        ] {
            assert!(extension_redirect_ok(ok), "{ok}");
        }
        for bad in [
            "http://abc.chromiumapp.org/",
            "https://chromiumapp.org/",
            "https://evil.example/",
            "https://abc.chromiumapp.org.evil.example/",
            "https://evil.example/.chromiumapp.org/",
            "https://evil.example?.chromiumapp.org",
            "https://user@abc.chromiumapp.org/",
            "https://abc.chromiumapp.org:8443/",
            "https://a.b.chromiumapp.org/",
            "https://abc.chromiumapp.org/#frag",
            "https://ABC.chromiumapp.org/",
            "javascript:alert(1)",
        ] {
            assert!(!extension_redirect_ok(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn the_extension_signs_in_and_gets_a_fresh_sync_token() {
        let u = ui();
        let cookie = u.setup("anna").await;
        let body = |redirect: &str, state: &str| serde_json::json!({"redirect": redirect, "state": state, "name": "Chrome on Linux"});
        let good = "https://abcdefghijklmnop.chromiumapp.org/";

        // Needs a session and the request header.
        let (st, _, _) = u.post("/ui/api/connect", None, body(good, "s1")).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, _, _) = u
            .call(
                "POST",
                "/ui/api/connect",
                Some(&cookie),
                Some(body(good, "s1")),
                false,
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        // A foreign redirect or a bad state gets no token.
        let (st, _, _) = u
            .post(
                "/ui/api/connect",
                Some(&cookie),
                body("https://evil.example/", "s1"),
            )
            .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let (st, _, _) = u
            .post("/ui/api/connect", Some(&cookie), body(good, "a b"))
            .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

        // Every sign-in issues a new device token, handed back in the fragment.
        let mut tokens = Vec::new();
        for _ in 0..2 {
            let (st, v, _) = u
                .post("/ui/api/connect", Some(&cookie), body(good, "s1"))
                .await;
            assert_eq!(st, StatusCode::OK);
            let url = v["url"].as_str().unwrap();
            let (base, frag) = url.split_once('#').unwrap();
            assert_eq!(base, good);
            let token = frag
                .strip_prefix("token=")
                .and_then(|t| t.strip_suffix("&state=s1"))
                .unwrap();
            assert_eq!(v["device"]["scope"], "sync");
            tokens.push(token.to_string());
        }
        assert_ne!(tokens[0], tokens[1]);
        let (_, devices, _) = u
            .call("GET", "/ui/api/devices", Some(&cookie), None, false)
            .await;
        assert_eq!(devices.as_array().unwrap().len(), 2);
    }
}
