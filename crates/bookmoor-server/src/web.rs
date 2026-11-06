//! The web UI (`docs/multi-user.md`): static files plus a small JSON API
//! under `/ui/api`, secured by a session cookie. Users manage their own
//! devices and tokens; admin users also manage users.
//!
//! Every write needs the `X-Bookmoor-UI` header on top of the
//! `SameSite=Strict` cookie, so a cross-site form post cannot act as a user.

use crate::api::{retry_after, save, ApiError, AppState, Shared};
use crate::auth::{random_id, Scope, TokenRecord};
use crate::ratelimit::client_key;
use crate::users::{check_password, hash_password, verify_password, User};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

mod mfa;
mod passkeys;

const INDEX: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const APP_CSS: &str = include_str!("web/app.css");
const LOGO: &str = include_str!("web/logo.svg");
const BOOKMARKS_JS: &str = include_str!("web/bookmarks.js");
const BOOKMARKS_CSS: &str = include_str!("web/bookmarks.css");

const COOKIE: &str = "bookmoor_session";
const CSRF_HEADER: &str = "x-bookmoor-ui";
/// Devices (read and sync tokens) one user may have.
const MAX_DEVICES: usize = 50;

/// The page with a build stamp on its scripts and styles, so a browser or
/// reverse proxy that kept an older `app.js` fetches the one of this build.
fn versioned_index() -> String {
    let stamp = concat!(env!("CARGO_PKG_VERSION"), "-", env!("BOOKMOOR_GIT_COMMIT"));
    let mut page = INDEX.to_string();
    for file in ["app.js", "app.css", "bookmarks.js", "bookmarks.css"] {
        page = page.replace(&format!("ui/{file}\""), &format!("ui/{file}?v={stamp}\""));
    }
    page
}

pub fn routes() -> Router<Shared> {
    Router::new()
        .route(
            "/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    versioned_index(),
                )
                    .into_response()
            }),
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
            "/ui/bookmarks.js",
            get(|| async { asset("text/javascript; charset=utf-8", BOOKMARKS_JS) }),
        )
        .route(
            "/ui/bookmarks.css",
            get(|| async { asset("text/css; charset=utf-8", BOOKMARKS_CSS) }),
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
        .route("/ui/api/profiles/{id}/tree", get(profile_tree))
        .route("/ui/api/profiles/{id}/edit", post(edit_profile))
        .route("/ui/api/profiles/{id}/clear", post(clear_profile))
        .route("/ui/api/profiles/{id}/dedupe", post(dedupe_profile))
        .route("/ui/api/profiles/{id}/duplicates", get(duplicates))
        .route(
            "/ui/api/profiles/{id}/duplicates/remove",
            post(remove_duplicates),
        )
        .route("/ui/api/profiles/{id}/restore", post(restore_profile))
        .route("/ui/api/profiles/{id}/export", get(export_profile))
        .route("/ui/api/profiles/{id}/import", post(import_profile))
        .route("/ui/api/dynacat", get(dynacat).post(add_dynacat))
        .route("/ui/api/dynacat/folders", get(dynacat_folders))
        .route(
            "/ui/api/dynacat/page",
            put(set_dynacat_page).delete(remove_dynacat_page),
        )
        .route(
            "/ui/api/dynacat/{id}",
            delete(remove_dynacat).put(change_dynacat),
        )
        .route(
            "/ui/api/linkwarden",
            get(linkwarden_list).post(add_linkwarden),
        )
        .route(
            "/ui/api/linkwarden/{id}",
            delete(remove_linkwarden).put(change_linkwarden),
        )
        .route("/ui/api/linkwarden/{id}/sync", post(sync_linkwarden))
        .route("/ui/api/password", post(change_password))
        .route("/ui/api/users", get(users).post(add_user))
        .route("/ui/api/users/{id}", delete(remove_user))
        .route("/ui/api/users/{id}/password", post(reset_password))
        .route("/ui/api/users/{id}/admin", post(set_admin))
        .merge(mfa::routes())
        .merge(passkeys::routes())
        .layer(middleware::from_fn(page_headers))
}

fn asset(kind: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, kind)], body).into_response()
}

/// Strict headers for the UI: nothing but our own files may load or run.
async fn page_headers(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/ui/api/");
    let is_page = req.uri().path() == "/";
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    // Only the page itself may load images from other sites: the opt-in
    // site icons of the Bookmarks tab. Scripts, styles and connections stay
    // limited to this server.
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(if is_page {
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: https:; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
        } else {
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
        }),
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
            .filter(|u| !u.disabled)
            .cloned()
            .ok_or(ApiError::Unauthorized)?;
        // An admin can require a second factor: until it is set up, the
        // session may only do what the setup needs.
        if user.mfa_required
            && !user.has_second_factor()
            && !st.sessions.is_sso(&session)
            && !mfa::allowed_before_setup(parts.uri.path())
        {
            return Err(ApiError::Denied("set up two-factor authentication first"));
        }
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
    /// Has an authenticator app for the password login.
    mfa: bool,
    /// Number of passkeys.
    passkeys: usize,
    /// Has a local password (single sign-on and passkey-only users may not).
    has_password: bool,
    /// An admin requires one.
    mfa_required: bool,
    /// Required but not set up yet: the UI offers nothing else.
    mfa_setup_needed: bool,
}

impl From<&User> for Me {
    fn from(u: &User) -> Me {
        Me {
            id: u.id.clone(),
            name: u.name.clone(),
            admin: u.admin,
            oidc: u.oidc.is_some(),
            mfa: u.totp.is_some(),
            passkeys: u.passkeys.len(),
            has_password: u.password_hash.is_some(),
            mfa_required: u.mfa_required,
            mfa_setup_needed: u.mfa_required && !u.has_second_factor(),
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
    let cookie = cookie_value(&headers);
    let user = cookie
        .as_deref()
        .and_then(|c| st.sessions.user_for(c, Instant::now()).map(String::from))
        .and_then(|id| st.users.get(&id));
    let sso = cookie.as_deref().is_some_and(|c| st.sessions.is_sso(c));
    let mut out = match user {
        Some(u) => {
            let mut me = Me::from(u);
            // Single sign-on leaves the second factor to the provider.
            me.mfa_setup_needed &= !sso;
            serde_json::json!({ "state": "user", "user": me })
        }
        None if !st.users.any_login() => serde_json::json!({ "state": "setup" }),
        None => serde_json::json!({ "state": "login" }),
    };
    if let Some(o) = &st.oidc {
        out["sso"] = serde_json::json!({ "label": o.cfg.label });
    }
    out["version"] = env!("CARGO_PKG_VERSION").into();
    out["commit"] = env!("BOOKMOOR_GIT_COMMIT").into();
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
            .filter(|u| !u.disabled)
            .map(|u| (u.id.clone(), u.password_hash.clone()))
    };
    let ok = verify_blocking(found.as_ref().and_then(|f| f.1.clone()), b.password).await;
    let mut st = s.lock().unwrap();
    match found {
        Some((id, _)) if ok => match st.users.get(&id).cloned() {
            // The password was right; the second factor comes next.
            Some(user) if user.has_second_factor() && !user.disabled => {
                let ticket = st.sessions.start_ticket(&user.id, Instant::now());
                Ok(Json(serde_json::json!({
                    "state": "mfa",
                    "ticket": ticket,
                    "totp": user.totp.is_some(),
                    "passkey": !user.passkeys.is_empty(),
                }))
                .into_response())
            }
            Some(user) if !user.disabled => Ok(signed_in(&mut st, &user)),
            _ => Err(ApiError::Unauthorized),
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
    if user.disabled {
        return back_to("", Some("disabled"));
    }
    let mut resp = back_to(ret, None);
    let token = st.sessions.create_with(&user.id, Instant::now(), true);
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
struct DynacatRow {
    /// `None` for outputs from the command line (read only here).
    id: Option<String>,
    /// Profile id (web UI outputs), to preselect it when editing.
    profile_id: Option<String>,
    file: String,
    /// Where the file is written: the full path.
    path: String,
    /// When the file on disk was last changed (Unix seconds); `None` if it does not exist.
    written: Option<u64>,
    profile: String,
    folder: String,
    style: bookmoor_core::dynacat::Style,
    error: Option<String>,
}

/// Modification time of an output file, in Unix seconds.
fn file_written(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[derive(Serialize)]
struct DynacatProfile {
    id: String,
    label: String,
    name: String,
}

/// The collecting page: from the web UI (editable) or the command line.
#[derive(Serialize)]
struct DynacatPage {
    /// Set in the web UI, so it can be changed here.
    editable: bool,
    /// Plain file name (web UI page) or the full path.
    file: String,
    path: String,
    icons: &'static str,
    name: String,
    columns: usize,
    /// `outputs` or `profiles`.
    source: &'static str,
    written: Option<u64>,
    error: Option<String>,
}

#[derive(Serialize)]
struct DynacatInfo {
    page: Option<DynacatPage>,
    /// The directory web-UI outputs are written to; `None` = not enabled.
    dir: Option<String>,
    outputs: Vec<DynacatRow>,
    profiles: Vec<DynacatProfile>,
}

fn profile_label(st: &AppState, id: &str) -> String {
    match st.profiles.get(id) {
        Some(p) => match st.users.get(&p.user_id) {
            Some(u) => format!("{} / {}", u.name, p.name),
            None => p.name.clone(),
        },
        None => "(removed)".into(),
    }
}

/// Dynacat output settings; admins only, because the files are written by
/// the server process.
async fn dynacat(State(s): State<Shared>, _: Admin) -> Json<DynacatInfo> {
    let st = s.lock().unwrap();
    let mut outputs = Vec::new();
    for o in &st.dynacat_outputs {
        let file = o.file.display().to_string();
        let profile = match st.dynacat_target(o.profile.as_deref()) {
            Ok(id) => profile_label(&st, &id),
            Err(e) => e,
        };
        outputs.push(DynacatRow {
            id: None,
            profile_id: None,
            error: st.dynacat_errors.get(&file).cloned(),
            path: file.clone(),
            written: file_written(&o.file),
            file,
            profile,
            folder: o.folder.join("/"),
            style: o.style.clone(),
        });
    }
    for o in &st.dynacat_ui {
        let path = st.dynacat_dir.as_ref().map(|d| d.join(&o.file));
        outputs.push(DynacatRow {
            id: Some(o.id.clone()),
            profile_id: Some(o.profile_id.clone()),
            file: o.file.clone(),
            path: st.dynacat_key(&o.file),
            written: path.as_deref().and_then(file_written),
            profile: profile_label(&st, &o.profile_id),
            folder: o.folder.join("/"),
            style: o.style.clone(),
            error: if st.dynacat_dir.is_none() {
                Some("not written: the server runs without --dynacat-dir".into())
            } else {
                st.dynacat_errors.get(&st.dynacat_key(&o.file)).cloned()
            },
        });
    }
    let profiles = st
        .profiles
        .list()
        .map(|p| DynacatProfile {
            id: p.id.clone(),
            label: profile_label(&st, &p.id),
            name: p.name.clone(),
        })
        .collect();
    let page = st.effective_page().map(|p| {
        let path = p.file.display().to_string();
        let editable = st.dynacat_page.is_none();
        DynacatPage {
            editable,
            error: st.dynacat_errors.get(&path).cloned(),
            written: file_written(&p.file),
            file: match (&st.dynacat_page_ui, editable) {
                (Some(u), true) => u.file.clone(),
                _ => path.clone(),
            },
            path,
            name: p.name.clone(),
            columns: p.columns,
            source: match p.source {
                crate::dynacat::PageSource::Outputs => "outputs",
                crate::dynacat::PageSource::Profiles => "profiles",
            },
            icons: match p.icons {
                bookmoor_core::dynacat::IconSource::None => "none",
                bookmoor_core::dynacat::IconSource::Known => "known",
                bookmoor_core::dynacat::IconSource::Google => "google",
                bookmoor_core::dynacat::IconSource::Site => "site",
            },
        }
    });
    Json(DynacatInfo {
        dir: st.dynacat_dir.as_ref().map(|d| d.display().to_string()),
        page,
        outputs,
        profiles,
    })
}

#[derive(Deserialize)]
struct FolderQuery {
    profile: String,
}

/// The folders of a profile that an output can be limited to.
async fn dynacat_folders(
    State(s): State<Shared>,
    _: Admin,
    axum::extract::Query(q): axum::extract::Query<FolderQuery>,
) -> Result<Json<Vec<String>>, ApiError> {
    let st = s.lock().unwrap();
    let engine = st
        .engines
        .get(&q.profile)
        .ok_or_else(|| ApiError::Unprocessable("no such profile".into()))?;
    Ok(Json(
        bookmoor_core::dynacat::folder_paths(&engine.tree().nodes)
            .into_iter()
            .map(|p| p.join("/"))
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NewDynacat {
    file: String,
    profile: String,
    /// `A/B/C`, empty for the whole tree.
    #[serde(default)]
    folder: String,
    #[serde(default)]
    style: bookmoor_core::dynacat::Style,
}

async fn add_dynacat(
    State(s): State<Shared>,
    Admin(a): Admin,
    Json(b): Json<NewDynacat>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    let folder = b
        .folder
        .split('/')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    st.add_dynacat_output(&b.file, &b.profile, folder, b.style)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    // The collecting page gets the new widget at once.
    if let Err(e) = st.publish_dynacat(None) {
        eprintln!("warning: cannot write the Dynacat file: {e}");
    }
    Ok(StatusCode::CREATED)
}

async fn change_dynacat(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
    Json(b): Json<NewDynacat>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    let folder = b
        .folder
        .split('/')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    st.update_dynacat_output(&id, &b.file, &b.profile, folder, b.style)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    if let Err(e) = st.publish_dynacat(None) {
        eprintln!("warning: cannot write the Dynacat file: {e}");
        // Saved, but the collecting page could not follow: say so.
        if let Some(page) = st.effective_page() {
            if let Some(m) = st.dynacat_errors.get(&page.file.display().to_string()) {
                return Err(ApiError::Unprocessable(format!(
                    "Saved, but the collecting page could not be written: {m}"
                )));
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn set_dynacat_page(
    State(s): State<Shared>,
    Admin(a): Admin,
    Json(b): Json<crate::dynacat::UiPage>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    st.set_dynacat_page_ui(b).map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    if let Err(e) = st.publish_dynacat(None) {
        eprintln!("warning: cannot write the Dynacat file: {e}");
        if let Some(page) = st.effective_page() {
            if let Some(m) = st.dynacat_errors.get(&page.file.display().to_string()) {
                return Err(ApiError::Unprocessable(format!(
                    "Saved, but the page could not be written: {m}"
                )));
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_dynacat_page(
    State(s): State<Shared>,
    Admin(a): Admin,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    if st.dynacat_page.is_some() {
        return Err(ApiError::Unprocessable(
            "the page is set on the command line; change it there".into(),
        ));
    }
    if let (Some(p), Some(d)) = (st.dynacat_page_ui.take(), st.dynacat_dir.clone()) {
        st.dynacat_errors
            .remove(&d.join(&p.file).display().to_string());
    }
    // The file stays: Dynacat may still include it.
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_dynacat(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    let Some(i) = st.dynacat_ui.iter().position(|o| o.id == id) else {
        return Err(ApiError::Unprocessable("no such output".into()));
    };
    let gone = st.dynacat_ui.remove(i);
    let key = st.dynacat_key(&gone.file);
    st.dynacat_errors.remove(&key);
    // The file stays: Dynacat may still include it.
    save(&mut st)?;
    // The collecting page drops the widget at once.
    if let Err(e) = st.publish_dynacat(None) {
        eprintln!("warning: cannot write the Dynacat file: {e}");
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct LinkwardenRow {
    id: String,
    profile_id: String,
    profile: String,
    folder: String,
    url: String,
    collection: String,
    mode: &'static str,
    interval: u64,
    /// Items both sides know about.
    paired: usize,
    running: bool,
    last_run: Option<u64>,
    last_ok: Option<u64>,
    message: String,
    failed: bool,
}

#[derive(Serialize)]
struct LinkwardenInfo {
    profiles: Vec<DynacatProfile>,
    connections: Vec<LinkwardenRow>,
}

fn mode_name(m: bookmoor_core::linkwarden::Mode) -> &'static str {
    use bookmoor_core::linkwarden::Mode;
    match m {
        Mode::Both => "both",
        Mode::Push => "push",
        Mode::Pull => "pull",
    }
}

/// Linkwarden connections; admins only, because the server makes the
/// requests (to an address the admin gives) with a stored access token.
async fn linkwarden_list(State(s): State<Shared>, _: Admin) -> Json<LinkwardenInfo> {
    let st = s.lock().unwrap();
    let connections = st
        .linkwarden
        .iter()
        .map(|l| {
            let status = st.linkwarden_status.get(&l.id).cloned().unwrap_or_default();
            LinkwardenRow {
                id: l.id.clone(),
                profile_id: l.profile_id.clone(),
                profile: profile_label(&st, &l.profile_id),
                folder: l.folder.join("/"),
                url: l.url.clone(),
                collection: l.collection.clone(),
                mode: mode_name(l.mode),
                interval: l.interval,
                paired: l.pairs.len(),
                running: status.running,
                last_run: status.last_run,
                last_ok: status.last_ok,
                message: status.message,
                failed: status.failed,
            }
        })
        .collect();
    let profiles = st
        .profiles
        .list()
        .map(|p| DynacatProfile {
            id: p.id.clone(),
            label: profile_label(&st, &p.id),
            name: p.name.clone(),
        })
        .collect();
    Json(LinkwardenInfo {
        profiles,
        connections,
    })
}

#[derive(Deserialize)]
struct LinkwardenBody {
    profile: String,
    /// `A/B/C`, empty for the whole profile.
    #[serde(default)]
    folder: String,
    url: String,
    /// Empty when changing a connection keeps the stored token.
    #[serde(default)]
    token: String,
    collection: String,
    #[serde(default)]
    mode: bookmoor_core::linkwarden::Mode,
    #[serde(default = "crate::linkwarden::default_interval")]
    interval: u64,
}

fn split_folder(f: &str) -> Vec<String> {
    f.split('/')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

/// Whether Linkwarden answers to this token; nothing is changed there.
async fn linkwarden_reachable(l: &crate::linkwarden::Linkwarden) -> Result<(), ApiError> {
    let client =
        crate::linkwarden::Client::new(&l.url, &l.token).map_err(ApiError::Unprocessable)?;
    client
        .collections()
        .await
        .map(drop)
        .map_err(|e| ApiError::Unprocessable(e.to_string()))
}

async fn add_linkwarden(
    State(s): State<Shared>,
    Admin(a): Admin,
    Json(b): Json<LinkwardenBody>,
) -> Result<StatusCode, ApiError> {
    let mut l = crate::linkwarden::Linkwarden {
        id: random_id("lw_"),
        profile_id: b.profile,
        folder: split_folder(&b.folder),
        url: b.url,
        token: b.token,
        collection: b.collection,
        collection_id: None,
        mode: b.mode,
        interval: b.interval,
        pairs: Vec::new(),
    };
    l.check().map_err(ApiError::Unprocessable)?;
    {
        let st = s.lock().unwrap();
        still_admin(&st, &a)?;
        st.check_linkwarden(&l).map_err(ApiError::Unprocessable)?;
    }
    // A wrong address or token is refused here, not found out later.
    linkwarden_reachable(&l).await?;
    let id = l.id.clone();
    {
        let mut st = s.lock().unwrap();
        still_admin(&st, &a)?;
        st.check_linkwarden(&l).map_err(ApiError::Unprocessable)?;
        st.linkwarden.push(l);
        save(&mut st)?;
    }
    // The first run starts at once; its outcome shows on the row.
    let state = s.clone();
    tokio::spawn(async move {
        let _ = crate::linkwarden::sync_now(&state, &id, false).await;
    });
    Ok(StatusCode::CREATED)
}

async fn change_linkwarden(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
    Json(b): Json<LinkwardenBody>,
) -> Result<StatusCode, ApiError> {
    let mut l = {
        let st = s.lock().unwrap();
        still_admin(&st, &a)?;
        st.linkwarden
            .iter()
            .find(|l| l.id == id)
            .cloned()
            .ok_or_else(|| ApiError::Unprocessable("no such connection".into()))?
    };
    let old = l.clone();
    let new_token = !b.token.trim().is_empty();
    if new_token {
        l.token = b.token;
    }
    l.profile_id = b.profile;
    l.folder = split_folder(&b.folder);
    l.url = b.url;
    l.collection = b.collection;
    l.mode = b.mode;
    l.interval = b.interval;
    l.check().map_err(ApiError::Unprocessable)?;
    // The stored token only ever goes to the address it was made for.
    if old.url != l.url && !new_token {
        return Err(ApiError::Unprocessable(
            "access token: enter it again when the address changes".into(),
        ));
    }
    // Another place or collection is a fresh start: what was paired is not
    // meaningful there (the first run matches by name and address).
    let moved = old.profile_id != l.profile_id
        || old.folder != l.folder
        || old.collection != l.collection
        || old.url != l.url;
    if moved {
        l.pairs.clear();
        l.collection_id = None;
    }
    if moved || old.token != l.token {
        linkwarden_reachable(&l).await?;
    }
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    st.check_linkwarden(&l).map_err(ApiError::Unprocessable)?;
    let Some(slot) = st.linkwarden.iter_mut().find(|x| x.id == id) else {
        return Err(ApiError::Unprocessable("no such connection".into()));
    };
    // Pairs made by a run that finished meanwhile are kept unless it moved.
    if !moved {
        l.pairs = slot.pairs.clone();
        l.collection_id = slot.collection_id;
    }
    *slot = l;
    // A run in progress keeps its flag; it notices the change when it ends.
    if st.linkwarden_status.get(&id).is_some_and(|s| !s.running) {
        st.linkwarden_status.remove(&id);
    }
    save(&mut st)?;
    st.linkwarden_wake.notify_one();
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_linkwarden(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    let Some(i) = st.linkwarden.iter().position(|l| l.id == id) else {
        return Err(ApiError::Unprocessable("no such connection".into()));
    };
    // What was synced stays on both sides.
    st.linkwarden.remove(i);
    st.linkwarden_status.remove(&id);
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
struct SyncQuery {
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
struct SyncResult {
    message: String,
}

async fn sync_linkwarden(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<SyncQuery>,
) -> Result<Json<SyncResult>, ApiError> {
    still_admin(&s.lock().unwrap(), &a)?;
    crate::linkwarden::sync_now(&s, &id, q.force)
        .await
        .map(|message| Json(SyncResult { message }))
        .map_err(ApiError::Unprocessable)
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

/// The profile's bookmark tree, for the Bookmarks page.
async fn profile_tree(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
) -> Result<Json<bookmoor_core::wire::TreeResponse>, ApiError> {
    let st = s.lock().unwrap();
    if !st.profiles.get(&id).is_some_and(|p| p.user_id == a.user.id) {
        return Err(ApiError::Forbidden);
    }
    let e = st.engines.get(&id).ok_or(ApiError::Forbidden)?;
    Ok(Json(e.tree()))
}

/// One edit from the Bookmarks page: add, rename, change the URL, move or
/// delete. It becomes a normal change for every device of the profile.
async fn edit_profile(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
    Json(e): Json<crate::engine::UiEdit>,
) -> Result<StatusCode, ApiError> {
    change_tree(&s, &a, &id, |st, id| st.edit_profile(id, e))?;
    Ok(StatusCode::NO_CONTENT)
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

/// Bookmarks saved more than once, grouped, for the review before removal.
async fn duplicates(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::tidy::DupGroup>>, ApiError> {
    let st = s.lock().unwrap();
    if !st.profiles.get(&id).is_some_and(|p| p.user_id == a.user.id) {
        return Err(ApiError::Forbidden);
    }
    st.duplicates(&id)
        .map(Json)
        .map_err(ApiError::Unprocessable)
}

#[derive(Deserialize)]
struct RemoveDuplicates {
    ids: Vec<String>,
}

#[derive(Serialize)]
struct Removed {
    removed: usize,
}

/// Removes the bookmarks picked in the review; the old tree stays restorable.
async fn remove_duplicates(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
    Json(b): Json<RemoveDuplicates>,
) -> Result<Json<Removed>, ApiError> {
    let removed = change_tree(&s, &a, &id, |st, id| st.remove_duplicates(id, &b.ids))?;
    Ok(Json(Removed { removed }))
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
    disabled: bool,
    /// Has an authenticator app.
    mfa: bool,
    /// Number of passkeys.
    passkeys: usize,
    mfa_required: bool,
    /// Linked to a single sign-on account.
    oidc: bool,
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
                disabled: u.disabled,
                mfa: u.totp.is_some(),
                passkeys: u.passkeys.len(),
                mfa_required: u.mfa_required,
                oidc: u.oidc.is_some(),
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

    #[test]
    fn assets_carry_the_build_stamp() {
        let page = crate::web::versioned_index();
        for file in ["app.js", "app.css", "bookmarks.js", "bookmarks.css"] {
            assert!(page.contains(&format!("ui/{file}?v=")), "{file}");
            assert!(!page.contains(&format!("ui/{file}\"")), "{file}");
        }
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
            // Remote images (site icons) are allowed on the page only.
            assert_eq!(csp.contains("img-src 'self' data: https:"), path == "/");
        }
    }

    #[tokio::test]
    async fn admins_manage_linkwarden_connections() {
        let u = ui();
        let anna = u.setup("anna").await;
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        let profile = v[0]["id"].as_str().unwrap().to_string();
        let body = |url: &str, token: &str, coll: &str| serde_json::json!({"profile": profile, "url": url, "token": token, "collection": coll});
        // Nothing is stored for a wrong address, token, name or unreachable server.
        for (b, why) in [
            (body("links.example.com", "t", "Bookmoor"), "address"),
            (body("https://l.example", "", "Bookmoor"), "token"),
            (body("https://l.example", "t", " "), "collection"),
            (body("http://127.0.0.1:1", "t", "Bookmoor"), "cannot reach"),
        ] {
            let (st, v, _) = u.post("/ui/api/linkwarden", Some(&anna), b).await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{why}");
            assert!(v["error"].as_str().unwrap().contains(why), "{v}");
        }
        assert!(u.state.lock().unwrap().linkwarden.is_empty());
        let mut bad = body("https://l.example", "t", "Bookmoor");
        bad["interval"] = 5.into();
        assert_eq!(
            u.post("/ui/api/linkwarden", Some(&anna), bad).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        // A connection in the state shows up without its token.
        u.state
            .lock()
            .unwrap()
            .linkwarden
            .push(crate::linkwarden::Linkwarden {
                id: "lw_1".into(),
                profile_id: profile.clone(),
                folder: vec![],
                url: "https://l.example".into(),
                token: "secret-token".into(),
                collection: "Bookmoor".into(),
                collection_id: Some(3),
                mode: bookmoor_core::linkwarden::Mode::Both,
                interval: 300,
                pairs: vec![],
            });
        let (st, v, _) = u
            .call("GET", "/ui/api/linkwarden", Some(&anna), None, false)
            .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["connections"][0]["collection"], "Bookmoor");
        assert!(!v.to_string().contains("secret-token"));

        // Changing mode and interval keeps the token and what is paired.
        let mut change = body("https://l.example", "", "Bookmoor");
        change["mode"] = "push".into();
        change["interval"] = 600.into();
        let (st, _, _) = u
            .call(
                "PUT",
                "/ui/api/linkwarden/lw_1",
                Some(&anna),
                Some(change),
                true,
            )
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        {
            let st = u.state.lock().unwrap();
            let l = &st.linkwarden[0];
            assert_eq!(
                (l.token.as_str(), l.interval, l.collection_id),
                ("secret-token", 600, Some(3))
            );
            assert_eq!(l.mode, bookmoor_core::linkwarden::Mode::Push);
        }
        // A new address never takes the stored token along.
        let (st, v, _) = u
            .call(
                "PUT",
                "/ui/api/linkwarden/lw_1",
                Some(&anna),
                Some(body("http://127.0.0.1:1", "", "Bookmoor")),
                true,
            )
            .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            v["error"].as_str().unwrap().contains("enter it again"),
            "{v}"
        );
        // Another collection is a fresh start and needs a reachable server.
        let (st, _, _) = u
            .call(
                "PUT",
                "/ui/api/linkwarden/lw_1",
                Some(&anna),
                Some(body("https://l.example", "", "Other")),
                true,
            )
            .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(u.state.lock().unwrap().linkwarden[0].collection, "Bookmoor");
        let (st, _, _) = u
            .call("DELETE", "/ui/api/linkwarden/lw_1", Some(&anna), None, true)
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        assert!(u.state.lock().unwrap().linkwarden.is_empty());
    }

    #[tokio::test]
    async fn admins_manage_dynacat_outputs_inside_the_directory() {
        let dir = std::env::temp_dir().join(format!("bookmoor-ui-dynacat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("glance.yml"), "pages: []\n").unwrap();
        let u = ui();
        let anna = u.setup("anna").await;
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        let profile = v[0]["id"].as_str().unwrap().to_string();

        // Off until the server has a directory.
        let new = |file: &str, folder: &str| serde_json::json!({"file": file, "profile": profile, "folder": folder});
        let (st, v, _) = u
            .call("GET", "/ui/api/dynacat", Some(&anna), None, false)
            .await;
        assert_eq!((st, v["dir"].is_null()), (StatusCode::OK, true));
        assert_eq!(
            u.post("/ui/api/dynacat", Some(&anna), new("a.yml", ""))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        u.state.lock().unwrap().dynacat_dir = Some(dir.clone());
        let (st, _, _) = u
            .post("/ui/api/dynacat", Some(&anna), new("a.yml", ""))
            .await;
        assert_eq!(st, StatusCode::CREATED);
        let text = std::fs::read_to_string(dir.join("a.yml")).unwrap();
        assert!(text.starts_with("# Generated by bookmoor"), "{text}");

        // Bad names, a missing folder, a repeated name and a foreign file.
        for bad in ["../x.yml", "x.txt", "sub/x.yml", ".h.yml"] {
            assert_eq!(
                u.post("/ui/api/dynacat", Some(&anna), new(bad, "")).await.0,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad}"
            );
        }
        let missing = new("b.yml", "Nope");
        assert_eq!(
            u.post("/ui/api/dynacat", Some(&anna), missing).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            u.post("/ui/api/dynacat", Some(&anna), new("a.yml", ""))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        // A foreign file is refused up front and nothing is stored.
        let (st, _, _) = u
            .post("/ui/api/dynacat", Some(&anna), new("glance.yml", ""))
            .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        // Names differing only in case are the same file.
        assert_eq!(
            u.post("/ui/api/dynacat", Some(&anna), new("A.yml", ""))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("glance.yml")).unwrap(),
            "pages: []\n"
        );

        let (_, v, _) = u
            .call("GET", "/ui/api/dynacat", Some(&anna), None, false)
            .await;
        let rows = v["outputs"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0]["error"].is_null());
        assert_eq!(v["profiles"][0]["label"], "anna / Default");
        let id = rows[0]["id"].as_str().unwrap().to_string();
        let (_, f, _) = u
            .call(
                "GET",
                &format!("/ui/api/dynacat/folders?profile={profile}"),
                Some(&anna),
                None,
                false,
            )
            .await;
        assert!(f.as_array().unwrap().iter().any(|p| p == "toolbar"));

        // Other users cannot see or change any of it.
        let bob_pw = serde_json::json!({"name":"bob","password":"another good password"});
        u.post("/ui/api/users", Some(&anna), bob_pw.clone()).await;
        let bob = cookie_of(&u.post("/ui/api/login", None, bob_pw).await.2.unwrap());
        assert_eq!(
            u.call("GET", "/ui/api/dynacat", Some(&bob), None, false)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            u.call(
                "DELETE",
                &format!("/ui/api/dynacat/{id}"),
                Some(&bob),
                None,
                true
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );

        let del = u
            .call(
                "DELETE",
                &format!("/ui/api/dynacat/{id}"),
                Some(&anna),
                None,
                true,
            )
            .await;
        assert_eq!(del.0, StatusCode::NO_CONTENT);
        assert!(dir.join("a.yml").exists(), "the file stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_collecting_page_follows_the_outputs() {
        let dir = std::env::temp_dir().join(format!("bookmoor-ui-page-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let u = ui();
        let anna = u.setup("anna").await;
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        let profile = v[0]["id"].as_str().unwrap().to_string();
        let page = dir.join("page.yml");
        {
            let mut st = u.state.lock().unwrap();
            st.dynacat_dir = Some(dir.clone());
            st.dynacat_page = Some(crate::dynacat::PageSpec {
                file: page.clone(),
                name: "Bookmoor".into(),
                columns: 1,
                source: crate::dynacat::PageSource::Outputs,
                icons: Default::default(),
            });
            st.publish_dynacat(None).unwrap();
        }
        let read = || std::fs::read_to_string(&page).unwrap();
        // No outputs: a valid page with an empty column.
        assert!(read().starts_with("# Generated by bookmoor"));
        assert!(read().contains("widgets: []"), "{}", read());

        let new = |file: &str, title: &str| {
            let style = if title.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::json!({"title": title})
            };
            serde_json::json!({"file": file, "profile": profile, "folder": "", "style": style})
        };
        // Added out of order; the page lists them by file name.
        for (f, t) in [("b.yml", "Beta"), ("a.yml", ""), ("c.yml", "Gamma")] {
            assert_eq!(
                u.post("/ui/api/dynacat", Some(&anna), new(f, t)).await.0,
                StatusCode::CREATED
            );
        }
        let text = read();
        let at = |t: &str| {
            text.find(&format!("title: \"{t}\""))
                .unwrap_or_else(|| panic!("{t} in {text}"))
        };
        // Without a title, the file name stands in.
        assert!(at("a") < at("Beta") && at("Beta") < at("Gamma"), "{text}");
        assert!(!text.contains("page.yml") && !text.contains("$include"));

        // The page cannot be taken by an output.
        assert_eq!(
            u.post("/ui/api/dynacat", Some(&anna), new("page.yml", "x"))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        // Unchanged content is not rewritten.
        let before = std::fs::metadata(&page).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        u.state.lock().unwrap().publish_dynacat(None).unwrap();
        assert_eq!(
            std::fs::metadata(&page).unwrap().modified().unwrap(),
            before
        );

        // A broken output (folder gone) is left out; the rest stays, and
        // the output's row carries the error.
        {
            let mut st = u.state.lock().unwrap();
            st.dynacat_ui[0].folder = vec!["Nope".into()];
            assert!(st.publish_dynacat(None).is_err());
            assert!(st.dynacat_errors.contains_key(&st.dynacat_key("b.yml")));
        }
        let text = read();
        assert!(
            !text.contains("\"Beta\"")
                && text.contains("\"Gamma\"")
                && text.contains("title: \"a\""),
            "{text}"
        );

        // Removing an output removes its widget.
        let id = u.state.lock().unwrap().dynacat_ui[2].id.clone();
        assert_eq!(
            u.call(
                "DELETE",
                &format!("/ui/api/dynacat/{id}"),
                Some(&anna),
                None,
                true
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert!(!read().contains("\"Gamma\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_collecting_file_can_hold_a_page_per_profile() {
        let dir = std::env::temp_dir().join(format!("bookmoor-ui-pp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let u = ui();
        let anna = u.setup("anna").await;
        let page = dir.join("pages.yml");
        {
            let mut st = u.state.lock().unwrap();
            st.dynacat_page = Some(crate::dynacat::PageSpec {
                file: page.clone(),
                name: "ignored".into(),
                columns: 2,
                source: crate::dynacat::PageSource::Profiles,
                icons: Default::default(),
            });
            st.publish_dynacat(None).unwrap();
        }
        let text = std::fs::read_to_string(&page).unwrap();
        assert!(text.starts_with("# Generated by bookmoor"), "{text}");
        assert!(
            text.contains("- name: \"Default\"") && !text.contains("ignored"),
            "{text}"
        );
        // A second profile is a second page.
        let (s, _, _) = u
            .post(
                "/ui/api/profiles",
                Some(&anna),
                serde_json::json!({"name": "Work"}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED);
        u.state.lock().unwrap().publish_dynacat(None).unwrap();
        let text = std::fs::read_to_string(&page).unwrap();
        assert_eq!(text.matches("\n- name: ").count(), 2, "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn admins_edit_outputs_and_the_collecting_page() {
        let dir = std::env::temp_dir().join(format!("bookmoor-ui-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let u = ui();
        let anna = u.setup("anna").await;
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        let profile = v[0]["id"].as_str().unwrap().to_string();
        let put = |path: String, body: serde_json::Value| {
            let (u, anna) = (&u, &anna);
            async move { u.call("PUT", &path, Some(anna), Some(body), true).await.0 }
        };
        let page_body = |file: &str| serde_json::json!({"file": file, "name": "Links", "columns": 2, "source": "outputs", "icons": "known"});

        // Needs the directory.
        assert_eq!(
            put("/ui/api/dynacat/page".into(), page_body("p.yml")).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        u.state.lock().unwrap().dynacat_dir = Some(dir.clone());
        let new = serde_json::json!({"file": "a.yml", "profile": profile, "folder": ""});
        assert_eq!(
            u.post("/ui/api/dynacat", Some(&anna), new).await.0,
            StatusCode::CREATED
        );

        // Page: not an output's file, bad values refused, then written.
        assert_eq!(
            put("/ui/api/dynacat/page".into(), page_body("a.yml")).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        for bad in [
            serde_json::json!({"file": "p.yml", "name": "", "columns": 1}),
            serde_json::json!({"file": "p.yml", "name": "x", "columns": 5}),
            serde_json::json!({"file": "../p.yml", "name": "x", "columns": 1}),
        ] {
            assert_eq!(
                put("/ui/api/dynacat/page".into(), bad).await,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        assert_eq!(
            put("/ui/api/dynacat/page".into(), page_body("p.yml")).await,
            StatusCode::NO_CONTENT
        );
        let text = std::fs::read_to_string(dir.join("p.yml")).unwrap();
        assert!(
            text.contains("- name: \"Links\"") && text.contains("title: \"a\""),
            "{text}"
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/dynacat", Some(&anna), None, false)
            .await;
        assert_eq!(
            (v["page"]["editable"].clone(), v["page"]["file"].clone()),
            (true.into(), "p.yml".into())
        );

        // The page name cannot be taken by a new output, and a changed page
        // is rewritten (profiles source: one page per profile).
        let clash = serde_json::json!({"file": "p.yml", "profile": profile, "folder": ""});
        assert_eq!(
            u.post("/ui/api/dynacat", Some(&anna), clash).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let mut body = page_body("p.yml");
        body["source"] = "profiles".into();
        assert_eq!(
            put("/ui/api/dynacat/page".into(), body).await,
            StatusCode::NO_CONTENT
        );
        assert!(std::fs::read_to_string(dir.join("p.yml"))
            .unwrap()
            .contains("- name: \"Default\""));

        // Edit the output: a title, then a rename (the old file stays).
        let id = v["outputs"][0]["id"].as_str().unwrap().to_string();
        let edit = |file: &str, title: &str| serde_json::json!({"file": file, "profile": profile, "folder": "", "style": {"title": title}});
        assert_eq!(
            put(format!("/ui/api/dynacat/{id}"), edit("a.yml", "Mine")).await,
            StatusCode::NO_CONTENT
        );
        assert!(std::fs::read_to_string(dir.join("a.yml"))
            .unwrap()
            .contains("title: \"Mine\""));
        assert_eq!(
            put(format!("/ui/api/dynacat/{id}"), edit("b.yml", "Mine")).await,
            StatusCode::NO_CONTENT
        );
        assert!(dir.join("a.yml").exists() && dir.join("b.yml").exists());
        assert_eq!(
            put(format!("/ui/api/dynacat/{id}"), edit("p.yml", "x")).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            put("/ui/api/dynacat/do_nope".into(), edit("c.yml", "x")).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/dynacat", Some(&anna), None, false)
            .await;
        assert_eq!(v["outputs"].as_array().unwrap().len(), 1);
        assert_eq!(v["outputs"][0]["file"], "b.yml");

        // A page from the command line is read only here.
        u.state.lock().unwrap().dynacat_page = Some(crate::dynacat::PageSpec {
            file: dir.join("cli.yml"),
            name: "Cli".into(),
            columns: 1,
            source: crate::dynacat::PageSource::Outputs,
            icons: Default::default(),
        });
        assert_eq!(
            put("/ui/api/dynacat/page".into(), page_body("q.yml")).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/dynacat", Some(&anna), None, false)
            .await;
        assert_eq!(v["page"]["editable"], false);
        u.state.lock().unwrap().dynacat_page = None;

        // Removing the page keeps its file.
        assert_eq!(
            u.call("DELETE", "/ui/api/dynacat/page", Some(&anna), None, true)
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let (_, v, _) = u
            .call("GET", "/ui/api/dynacat", Some(&anna), None, false)
            .await;
        assert!(v["page"].is_null());
        assert!(dir.join("p.yml").exists());
        let _ = std::fs::remove_dir_all(&dir);
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
    async fn bookmarks_are_edited_in_the_ui_and_devices_follow() {
        let u = ui();
        let anna = u.setup("anna").await;
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        let pid = v[0]["id"].as_str().unwrap().to_string();
        let dev = serde_json::json!({"name":"laptop","scope":"sync"});
        let (_, v, _) = u.post("/ui/api/devices", Some(&anna), dev).await;
        let token = v["token"].as_str().unwrap().to_string();
        let send = |m: &str, uri: &str, body: serde_json::Value| {
            let req = Request::builder()
                .method(m)
                .uri(uri.to_string())
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            let app = u.app.clone();
            async move {
                let res = app.oneshot(req).await.unwrap();
                let b = axum::body::to_bytes(res.into_body(), usize::MAX)
                    .await
                    .unwrap();
                serde_json::from_slice::<serde_json::Value>(&b).unwrap_or_default()
            }
        };
        let ep = send("POST", "/v1/endpoints", serde_json::json!({})).await;
        let ep = ep["endpoint_id"].as_str().unwrap().to_string();
        let edit_uri = format!("/ui/api/profiles/{pid}/edit");
        let edit = |body: serde_json::Value| u.post(&edit_uri, Some(&anna), body);
        let tree = || async {
            u.call(
                "GET",
                &format!("/ui/api/profiles/{pid}/tree"),
                Some(&anna),
                None,
                false,
            )
            .await
            .1
        };

        let (st, _, _) = edit(
            serde_json::json!({"type":"add","parent":"toolbar","kind":"folder","title":"News"}),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let t = tree().await;
        let folder = t["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["title"] == "News")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        for (title, url) in [
            ("HN", "https://news.ycombinator.com"),
            ("Zeit", "https://zeit.de"),
        ] {
            let (st, _, _) = edit(serde_json::json!({"type":"add","parent":folder,"kind":"bookmark","title":title,"url":url})).await;
            assert_eq!(st, StatusCode::NO_CONTENT);
        }
        let t = tree().await;
        let kids: Vec<&serde_json::Value> = t["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["parent"] == folder.as_str())
            .collect();
        assert_eq!(kids[0]["title"], "HN");
        assert_eq!(kids[1]["title"], "Zeit");
        let (hn, zeit) = (
            kids[0]["id"].as_str().unwrap().to_string(),
            kids[1]["id"].as_str().unwrap().to_string(),
        );
        // A device that has seen nothing gets all of it as normal changes.
        let r = send(
            "POST",
            "/v1/sync",
            serde_json::json!({"endpoint_id": ep, "since_rev": 0, "ops": []}),
        )
        .await;
        assert_eq!(r["resync"], false);
        assert_eq!(r["ops"].as_array().unwrap().len(), 3);
        let seen = r["rev"].as_u64().unwrap();

        // Rename, change the URL, put Zeit first, then reject bad input.
        edit(serde_json::json!({"type":"update","id":hn,"title":"Hacker News","url":"https://news.ycombinator.com/news"})).await;
        edit(serde_json::json!({"type":"move","id":zeit,"parent":folder,"before":hn})).await;
        let t = tree().await;
        let order: Vec<&str> = t["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["parent"] == folder.as_str())
            .map(|n| n["title"].as_str().unwrap())
            .collect();
        assert_eq!(order, ["Zeit", "Hacker News"]);
        let (st, _, _) =
            edit(serde_json::json!({"type":"update","id":zeit,"url":"no scheme"})).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let (st, _, _) = edit(serde_json::json!({"type":"move","id":folder,"parent":folder})).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let (st, _, _) = edit(serde_json::json!({"type":"remove","id":"toolbar"})).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let r = send(
            "POST",
            "/v1/sync",
            serde_json::json!({"endpoint_id": ep, "since_rev": seen, "ops": []}),
        )
        .await;
        assert_eq!(r["resync"], false);
        let kinds: Vec<&str> = r["ops"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["update", "move"]);

        // Deleting a folder with content keeps a copy.
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        assert!(v[0]["backup"].is_null());
        let (st, _, _) = edit(serde_json::json!({"type":"remove","id":folder})).await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        assert_eq!(v[0]["backup"]["bookmarks"], 2);
        assert_eq!(v[0]["bookmarks"], 0);
        // Single bookmarks are deleted without touching the backup.
        let (st, _, _) = u
            .post(
                &format!("/ui/api/profiles/{pid}/restore"),
                Some(&anna),
                serde_json::json!({}),
            )
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (st, _, _) = edit(serde_json::json!({"type":"remove","id":zeit})).await;
        assert_eq!(st, StatusCode::NO_CONTENT);

        // Somebody else cannot read or edit it.
        let new = serde_json::json!({"name":"bob","password":"another good password"});
        u.post("/ui/api/users", Some(&anna), new).await;
        let login = serde_json::json!({"name":"bob","password":"another good password"});
        let (_, _, set) = u.post("/ui/api/login", None, login).await;
        let bob = cookie_of(&set.unwrap());
        let (st, _, _) = u
            .call(
                "GET",
                &format!("/ui/api/profiles/{pid}/tree"),
                Some(&bob),
                None,
                false,
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _, _) = u
            .post(
                &format!("/ui/api/profiles/{pid}/edit"),
                Some(&bob),
                serde_json::json!({"type":"remove","id":hn}),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn duplicates_are_reviewed_before_they_are_removed() {
        let u = ui();
        let anna = u.setup("anna").await;
        let (_, v, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        let pid = v[0]["id"].as_str().unwrap().to_string();
        for (parent, title) in [("toolbar", "One"), ("other", "Two")] {
            let body = serde_json::json!({"type":"add","parent":parent,"kind":"bookmark",
                "title":title,"url":"https://dup.example/"});
            let r = u
                .post(&format!("/ui/api/profiles/{pid}/edit"), Some(&anna), body)
                .await;
            assert_eq!(r.0, StatusCode::NO_CONTENT);
        }
        let (st, g, _) = u
            .call(
                "GET",
                &format!("/ui/api/profiles/{pid}/duplicates"),
                Some(&anna),
                None,
                false,
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        let items = g[0]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        let keep = g[0]["keep"].as_str().unwrap().to_string();
        let drop = items
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .find(|i| *i != keep)
            .unwrap();
        // Removing every copy is refused.
        let all = serde_json::json!({"ids":[keep, drop]});
        let r = u
            .post(
                &format!("/ui/api/profiles/{pid}/duplicates/remove"),
                Some(&anna),
                all,
            )
            .await;
        assert_eq!(r.0, StatusCode::UNPROCESSABLE_ENTITY);
        let one = serde_json::json!({"ids":[drop]});
        let (st, v, _) = u
            .post(
                &format!("/ui/api/profiles/{pid}/duplicates/remove"),
                Some(&anna),
                one,
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["removed"], 1);
        let (_, p, _) = u
            .call("GET", "/ui/api/profiles", Some(&anna), None, false)
            .await;
        assert_eq!(p[0]["bookmarks"], 1);
        assert_eq!(p[0]["backup"]["bookmarks"], 2);
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
