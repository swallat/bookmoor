//! HTTP API (`docs/sync-architecture.md`, section 3.2).

use crate::auth::{random_id, Scope, TokenRecord, Tokens};
use crate::engine::{Engine, SyncError};
use crate::profiles::Profiles;
use crate::ratelimit::client_key;
use crate::users::User;
use crate::users::Users;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Extension;
use axum::{Json, Router};
use bookmoor_core::wire::{SyncRequest, SyncResponse, TreeResponse, WireNode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Largest accepted request body.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// A profile's tree as it was before a destructive change, so that the change
/// can be undone (one per profile; the next change replaces it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backup {
    /// When it was taken, UTC.
    pub at: String,
    pub nodes: Vec<bookmoor_core::wire::WireNode>,
}

impl Backup {
    /// Bookmarks (not folders) in the saved tree.
    pub fn bookmarks(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.kind == bookmoor_core::model::Kind::Bookmark)
            .count()
    }
}

#[derive(Debug, Default)]
pub struct AppState {
    pub users: Users,
    pub profiles: Profiles,
    /// One bookmark tree per profile id.
    pub engines: BTreeMap<String, Engine>,
    pub tokens: Tokens,
    /// The tree a profile had before its last clear, tidy-up or restore.
    pub backups: BTreeMap<String, Backup>,
    /// Where state is saved after every change; `None` keeps it in memory.
    pub store: Option<crate::store::Store>,
    pub limits: crate::ratelimit::Limits,
    pub sessions: crate::sessions::Sessions,
    /// Mark the session cookie `Secure` (everything but `--insecure-http`).
    pub secure_cookies: bool,
    /// The Dynacat widget files kept up to date (none = off).
    pub dynacat_outputs: Vec<crate::dynacat::Output>,
    /// Whose tree the Dynacat file shows (id or name); the only user if unset.
    pub dynacat_user: Option<String>,
    /// Default profile of an output that names none; the only one if unset.
    pub dynacat_profile: Option<String>,
    /// Require the reverse proxy's client-certificate verdict (see `mtls`).
    pub client_cert: Option<crate::mtls::ClientCertCheck>,
    /// Single sign-on for the web UI (none = off).
    pub oidc: Option<std::sync::Arc<crate::oidc::Oidc>>,
    pub oidc_flows: crate::oidc::Flows,
    /// Single sign-on as an admin set it up in the web UI (saved with the state).
    pub oidc_config: Option<crate::oidc::OidcConfig>,
    /// Single sign-on comes from the environment or flags, which win over `oidc_config`.
    pub oidc_env: bool,
}

impl AppState {
    /// Make `cfg` the web-UI single sign-on settings and, unless the
    /// environment configures it, the live provider. Flows in progress belong
    /// to the old settings and end here. A bad stored config switches
    /// single sign-on off (with a message) rather than stopping the server.
    pub fn apply_oidc(&mut self, cfg: Option<crate::oidc::OidcConfig>) {
        // Unchanged (e.g. a failed save of something else reloads the state):
        // keep the live provider and the sign-ins in progress.
        if self.oidc_config == cfg && (self.oidc_env || self.oidc.is_some() == cfg.is_some()) {
            return;
        }
        self.oidc_config = cfg;
        if self.oidc_env {
            return;
        }
        self.oidc_flows = Default::default();
        self.oidc = None;
        if let Some(c) = self.oidc_config.clone() {
            match crate::oidc::Oidc::new(c) {
                Ok(o) => {
                    // Keep what the provider really uses (trimmed, default label).
                    self.oidc_config = Some(o.cfg.clone());
                    self.oidc = Some(std::sync::Arc::new(o));
                }
                Err(e) => eprintln!("single sign-on settings ignored: {e}"),
            }
        }
    }

    /// The profile whose tree the Dynacat file shows: `dynacat_profile`, else
    /// the user's only profile, else (several) their `Default` one.
    fn dynacat_target(&self, profile: Option<&str>) -> Result<String, String> {
        let profile = profile.or(self.dynacat_profile.as_deref());
        let uid = self.resolve_user(self.dynacat_user.as_deref())?;
        match self.resolve_profile(&uid, profile) {
            Err(_) if profile.is_none() => {
                let id = crate::profiles::default_id_for(&uid);
                if self.profiles.get(&id).is_some_and(|p| p.user_id == uid) {
                    Ok(id)
                } else {
                    Err("several profiles exist: name one with --dynacat-profile".into())
                }
            }
            other => other,
        }
    }

    /// Refresh the Dynacat widget file with the tree of the Dynacat user's
    /// profile. `changed` is the profile a sync just changed: other profiles
    /// leave the file alone. A failure is logged and never fails the sync:
    /// the dashboard is a view, the canonical tree is saved.
    pub fn publish_dynacat(&self, changed: Option<&str>) -> std::io::Result<()> {
        if self.dynacat_outputs.is_empty() || self.users.is_empty() {
            return Ok(());
        }
        // Every output gets its turn; all failures are reported.
        let mut errors = Vec::new();
        for out in &self.dynacat_outputs {
            let res = self
                .dynacat_target(out.profile.as_deref())
                .map_err(std::io::Error::other)
                .and_then(|pid| {
                    if changed.is_some_and(|c| c != pid) {
                        return Ok(false);
                    }
                    crate::dynacat::publish(&out.file, &self.engines[&pid].tree(), &out.folder)
                });
            if let Err(e) = res {
                errors.push(format!("{}: {e}", out.file.display()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(std::io::Error::other(errors.join("; ")))
        }
    }

    /// The user a request names by id or name; with no name, the only user.
    pub fn resolve_user(&self, user: Option<&str>) -> Result<String, String> {
        match user {
            Some(u) => self
                .users
                .get(u)
                .or_else(|| self.users.by_name(u))
                .map(|u| u.id.clone())
                .ok_or_else(|| format!("no user called {u}")),
            None => {
                let mut all = self.users.list();
                match (all.next(), all.next()) {
                    (Some(u), None) => Ok(u.id.clone()),
                    (None, _) => Err("there is no user yet: create one first".into()),
                    _ => Err("several users exist: name the user".into()),
                }
            }
        }
    }

    /// The profile a request names (id or name) among `user_id`'s; with no
    /// name, the only one.
    pub fn resolve_profile(&self, user_id: &str, profile: Option<&str>) -> Result<String, String> {
        match profile {
            Some(p) => self
                .profiles
                .get(p)
                .filter(|x| x.user_id == user_id)
                .or_else(|| self.profiles.by_name(user_id, p))
                .map(|x| x.id.clone())
                .ok_or_else(|| format!("no profile called {p}")),
            None => {
                let all = self.profiles.for_user(user_id);
                match all.as_slice() {
                    [only] => Ok(only.id.clone()),
                    [] => Err("the user has no profile".into()),
                    _ => Err("several profiles exist: name the profile".into()),
                }
            }
        }
    }

    /// A new profile with an empty bookmark tree.
    pub fn add_profile(
        &mut self,
        user_id: &str,
        name: &str,
    ) -> Result<crate::profiles::Profile, String> {
        if self.users.get(user_id).is_none() {
            return Err("no such user".into());
        }
        let p = self.profiles.create(user_id, name)?;
        self.engines.insert(p.id.clone(), Engine::new());
        Ok(p)
    }

    /// Replace a profile's tree by `nodes`, keeping the old one as the
    /// backup. Devices of the profile resync and then follow the new tree.
    fn replace_tree(&mut self, profile: &str, nodes: &[WireNode]) -> Result<(), String> {
        let engine = self.engines.get_mut(profile).ok_or("no such profile")?;
        let had_content = engine.bookmark_count() > 0;
        let before = engine.tree().nodes;
        engine.replace_tree(nodes)?;
        // An empty tree is no backup: a repeated clear must not push out the
        // copy that was taken before the first one.
        if had_content {
            self.backups.insert(
                profile.to_string(),
                Backup {
                    at: utc_now(),
                    nodes: before,
                },
            );
        }
        Ok(())
    }

    /// Empty a profile (the tree stays recoverable with `restore_profile`).
    pub fn clear_profile(&mut self, profile: &str) -> Result<(), String> {
        let fresh = Engine::new().tree().nodes;
        self.replace_tree(profile, &fresh)
    }

    /// Merge folders of the same name and drop repeated bookmarks; returns
    /// what was done. A tree that is already tidy changes nothing.
    pub fn dedupe_profile(&mut self, profile: &str) -> Result<crate::tidy::Tidied, String> {
        let current = self
            .engines
            .get(profile)
            .ok_or("no such profile")?
            .tree()
            .nodes;
        let (nodes, done) = crate::tidy::dedupe(&current);
        if done.total() > 0 {
            self.replace_tree(profile, &nodes)?;
        }
        Ok(done)
    }

    /// Replace a profile's tree by an imported one (the old one becomes the
    /// backup). Refused, with nothing changed, if the tree is not valid.
    pub fn import_profile(&mut self, profile: &str, nodes: &[WireNode]) -> Result<(), String> {
        self.replace_tree(profile, nodes)
    }

    /// Bring back the tree saved before the last clear or tidy-up (which
    /// becomes the new backup, so restoring twice undoes itself).
    pub fn restore_profile(&mut self, profile: &str) -> Result<(), String> {
        let b = self
            .backups
            .get(profile)
            .ok_or("there is no backup of this profile")?
            .clone();
        self.replace_tree(profile, &b.nodes)
    }

    /// Remove a profile with its tree and tokens. A user keeps at least one.
    pub fn remove_profile(&mut self, user_id: &str, id: &str) -> Result<bool, String> {
        let mine = self.profiles.get(id).is_some_and(|p| p.user_id == user_id);
        if !mine {
            return Ok(false);
        }
        if self.profiles.for_user(user_id).len() <= 1 {
            return Err("a user needs at least one profile".into());
        }
        self.tokens.revoke_profile(id);
        self.engines.remove(id);
        self.backups.remove(id);
        Ok(self.profiles.remove(id))
    }

    /// A new user together with a profile and an empty bookmark tree.
    pub fn add_user(
        &mut self,
        name: &str,
        admin: bool,
        password: Option<&str>,
    ) -> Result<User, String> {
        let hash = match password {
            Some(p) => {
                crate::users::check_password(p)?;
                Some(crate::users::hash_password(p)?)
            }
            None => None,
        };
        self.add_user_hashed(name, admin, hash)
    }

    pub fn add_user_hashed(
        &mut self,
        name: &str,
        admin: bool,
        hash: Option<String>,
    ) -> Result<User, String> {
        let user = self.users.create_hashed(name, admin, hash)?;
        let id = crate::profiles::default_id_for(&user.id);
        if let Err(e) =
            self.profiles
                .create_with_id(id.clone(), &user.id, crate::profiles::DEFAULT_NAME)
        {
            // Never leave a user without a profile behind.
            self.users.remove(&user.id);
            return Err(e);
        }
        self.engines.insert(id, Engine::new());
        Ok(user)
    }

    /// Remove a user with their tree and all their tokens.
    pub fn remove_user(&mut self, id: &str) -> bool {
        self.sessions.end_user(id);
        for p in self.profiles.remove_user(id) {
            self.engines.remove(&p);
        }
        self.tokens.revoke_user(id);
        self.users.remove(id)
    }

    /// A new token for `user_id`; a sync token comes with its own endpoint.
    /// Admin tokens belong to no user.
    pub fn issue_token(
        &mut self,
        user_id: Option<&str>,
        profile: Option<&str>,
        name: &str,
        scope: Scope,
    ) -> Result<(String, TokenRecord), String> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err("token name must be 1 to 64 characters without control characters".into());
        }
        let user_id = match (scope, user_id) {
            (Scope::Admin, _) => None,
            (_, Some(u)) if self.users.get(u).is_some() => Some(u.to_string()),
            (_, Some(_)) => return Err("no such user".into()),
            (_, None) => return Err("a read or sync token needs a user".into()),
        };
        let profile_id = match &user_id {
            Some(u) => Some(self.resolve_profile(u, profile)?),
            None if profile.is_some() => return Err("an admin token has no profile".into()),
            None => None,
        };
        let endpoint_id = (scope == Scope::Sync).then(|| random_id("ep_"));
        if let (Some(ep), Some(pid)) = (&endpoint_id, &profile_id) {
            self.engines
                .get_mut(pid)
                .ok_or("profile has no tree")?
                .register_endpoint(ep);
        }
        Ok(self
            .tokens
            .create(name, scope, user_id, profile_id, endpoint_id))
    }
}

pub type Shared = Arc<Mutex<AppState>>;

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    Forbidden,
    /// 403 with a reason the caller may show to the user.
    Denied(&'static str),
    Unprocessable(String),
    Storage,
    /// Seconds until the caller may try again.
    TooManyRequests(u64),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::TooManyRequests(secs) = self {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, secs.to_string())],
                Json(serde_json::json!({ "error": "too many requests" })),
            )
                .into_response();
        }
        let (status, msg) = match self {
            // Uniform body so callers cannot tell why authentication failed.
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".to_string()),
            ApiError::Denied(why) => (StatusCode::FORBIDDEN, why.to_string()),
            ApiError::Unprocessable(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
            ApiError::Storage => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not save state".to_string(),
            ),
            ApiError::TooManyRequests(_) => unreachable!("answered above"),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

/// Authenticates before any body is parsed, so unauthenticated callers get a
/// uniform 401 whatever they send.
async fn require_auth(
    State(s): State<Shared>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // Without a peer address (not served with connect info) there is nothing
    // to key the failed-auth bucket on; skipping beats sharing one for all.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| client_key(c.0.ip()));
    let rec = {
        let mut st = s.lock().unwrap();
        let now = Instant::now();
        let found = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .and_then(|text| st.tokens.authenticate(text))
            .cloned();
        match found {
            Some(rec) => {
                // Only bad credentials count against the address. A throttled
                // address can still use a valid token: tokens carry 256 bits,
                // so guessing is hopeless either way, and refusing before the
                // check would lock out a reverse proxy's real clients.
                st.limits
                    .per_token
                    .take(&rec.id, now)
                    .map_err(retry_after)?;
                rec
            }
            None => {
                if let Some(peer) = peer {
                    st.limits
                        .failed_auth
                        .take(&peer, now)
                        .map_err(retry_after)?;
                }
                return Err(ApiError::Unauthorized);
            }
        }
    };
    let who = TokenName(rec.name.clone());
    req.extensions_mut().insert(rec);
    let mut resp = next.run(req).await;
    // For the request log, which sits outside this layer.
    resp.extensions_mut().insert(who);
    Ok(resp)
}

/// Answers the CORS preflight of browser extensions and lets them read the
/// replies. A browser sends the preflight, without credentials, when an
/// extension has no host permission for the server (Firefox leaves it optional
/// until the user grants it), so it must not need authentication. Only
/// `chrome-extension://` and `moz-extension://` origins get this: web pages
/// stay locked out, and every real request still needs its bearer token.
async fn cors_for_extensions(req: Request, next: Next) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .filter(|o| {
            o.to_str().is_ok_and(|o| {
                o.starts_with("chrome-extension://") || o.starts_with("moz-extension://")
            })
        })
        .cloned();
    let Some(origin) = origin else {
        return next.run(req).await;
    };
    let preflight = req.method() == Method::OPTIONS;
    let wanted = req
        .headers()
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .cloned();
    let mut resp = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = resp.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    h.insert(header::VARY, HeaderValue::from_static("Origin"));
    if preflight {
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, DELETE"),
        );
        // Reflect what the extension wants to send (it may add proxy headers).
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            wanted.unwrap_or_else(|| HeaderValue::from_static("authorization, content-type")),
        );
        h.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
    }
    resp
}

/// Name of the token that made a request, passed outwards to the log line.
#[derive(Clone)]
struct TokenName(String);

/// One line per request on stderr: time, peer, token, method, path, status,
/// duration. The query string is left out on purpose, since proxy setups may
/// carry secrets in it. `/health` is skipped so probes do not drown the rest.
async fn log_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "-".into());
    let started = Instant::now();
    let resp = next.run(req).await;
    if path != "/health" {
        let who = resp
            .extensions()
            .get::<TokenName>()
            .map_or("-", |t| t.0.as_str());
        eprintln!(
            "{} {peer} {who} {method} {path} {} {}ms",
            utc_now(),
            resp.status().as_u16(),
            started.elapsed().as_millis()
        );
    }
    resp
}

/// `2026-11-03T20:35:16Z` without a date library.
pub(crate) fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    utc(secs)
}

fn utc(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil-from-days (Howard Hinnant), days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

pub(crate) fn retry_after(wait: Duration) -> ApiError {
    ApiError::TooManyRequests(wait.as_secs_f64().ceil().max(1.0) as u64)
}

fn need(who: &TokenRecord, required: Scope) -> Result<(), ApiError> {
    if who.scope.allows(required) {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// Scope check that runs before any body is parsed, so a token without the
/// right scope gets 403 whatever it sends.
async fn scope_gate(required: Scope, req: Request, next: Next) -> Result<Response, ApiError> {
    let who = req
        .extensions()
        .get::<TokenRecord>()
        .ok_or(ApiError::Unauthorized)?;
    need(who, required)?;
    Ok(next.run(req).await)
}

pub fn router(state: Shared) -> Router {
    let client_cert = state.lock().unwrap().client_cert.clone();
    let gate = |scope: Scope| {
        middleware::from_fn(move |req: Request, next: Next| scope_gate(scope, req, next))
    };
    let v1 = Router::new()
        .route("/v1/tree", get(tree).route_layer(gate(Scope::Read)))
        .route(
            "/v1/tree/reset",
            post(reset_tree).route_layer(gate(Scope::Sync)),
        )
        .route("/v1/sync", post(sync).route_layer(gate(Scope::Sync)))
        // The handler checks the scope itself to explain a refusal.
        .route("/v1/endpoints", post(register_endpoint))
        .route(
            "/v1/endpoints/{id}",
            delete(delete_endpoint).route_layer(gate(Scope::Sync)),
        )
        .route(
            "/v1/tokens",
            post(create_token).route_layer(gate(Scope::Admin)),
        )
        .route(
            "/v1/users",
            get(list_users)
                .post(create_user)
                .route_layer(gate(Scope::Admin)),
        )
        .route(
            "/v1/users/{id}",
            delete(delete_user).route_layer(gate(Scope::Admin)),
        )
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    let app = Router::new()
        .route("/health", get(health))
        .merge(v1)
        .merge(crate::web::routes())
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));
    // Inside the CORS layer, so a refusal stays readable for the extension.
    let app = match client_cert {
        Some(check) => app.layer(middleware::from_fn_with_state(
            check,
            crate::mtls::require_client_cert,
        )),
        None => app,
    };
    app.layer(middleware::from_fn(cors_for_extensions))
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}

/// A failed save is reported, never ignored: the caller retries and op ids
/// make that safe.
pub(crate) fn save(st: &mut AppState) -> Result<(), ApiError> {
    st.persist_or_revert().map_err(|e| {
        eprintln!("error: saving state failed: {e}");
        ApiError::Storage
    })
}

async fn health() -> &'static str {
    "ok"
}

/// The profile (tree) a token reaches.
fn profile_id(who: &TokenRecord) -> Result<&str, ApiError> {
    who.profile_id.as_deref().ok_or(ApiError::Denied(
        "this token belongs to no user and has no bookmarks: use a read or sync token",
    ))
}

async fn tree(
    State(s): State<Shared>,
    Extension(who): Extension<TokenRecord>,
) -> Result<Json<TreeResponse>, ApiError> {
    need(&who, Scope::Read)?;
    let st = s.lock().unwrap();
    let engine = st
        .engines
        .get(profile_id(&who)?)
        .ok_or(ApiError::Forbidden)?;
    Ok(Json(engine.tree()))
}

/// Empty the token's profile (an old tree stays restorable in the web UI).
/// For a device that wants to start the server's data from its own bookmarks.
async fn reset_tree(
    State(s): State<Shared>,
    Extension(who): Extension<TokenRecord>,
) -> Result<StatusCode, ApiError> {
    need(&who, Scope::Sync)?;
    let mut st = s.lock().unwrap();
    let pid = profile_id(&who)?.to_string();
    st.clear_profile(&pid).map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    if let Err(e) = st.publish_dynacat(Some(&pid)) {
        eprintln!("warning: cannot write the Dynacat file: {e}");
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn sync(
    State(s): State<Shared>,
    Extension(who): Extension<TokenRecord>,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    need(&who, Scope::Sync)?;
    let mut st = s.lock().unwrap();
    // A token may only speak for its own endpoint.
    if who.endpoint_id.as_deref() != Some(req.endpoint_id.as_str()) {
        return Err(ApiError::Forbidden);
    }
    let pid = profile_id(&who)?.to_string();
    let engine = st.engines.get_mut(&pid).ok_or(ApiError::Forbidden)?;
    let before = engine.rev();
    match engine.sync(&req) {
        Ok(resp) => {
            // Rejected or repeated ops leave the canonical state as it was;
            // a retry after a restart is judged the same way again.
            if st.engines[&pid].rev() != before {
                save(&mut st)?;
                if let Err(e) = st.publish_dynacat(Some(&pid)) {
                    eprintln!("warning: cannot write the Dynacat file: {e}");
                }
            }
            Ok(Json(resp))
        }
        Err(SyncError::TooManyOps { max }) => Err(ApiError::Unprocessable(format!(
            "too many ops in one request (max {max})"
        ))),
    }
}

#[derive(Serialize)]
struct EndpointInfo {
    endpoint_id: String,
}

/// The endpoint a sync token is bound to (created together with the token).
async fn register_endpoint(
    Extension(who): Extension<TokenRecord>,
) -> Result<Json<EndpointInfo>, ApiError> {
    if !who.scope.allows(Scope::Sync) {
        return Err(ApiError::Denied(
            "this token cannot sync: create a device token with scope sync",
        ));
    }
    match who.endpoint_id {
        Some(endpoint_id) => Ok(Json(EndpointInfo { endpoint_id })),
        // Admin tokens manage tokens; only sync tokens come with a device.
        None => Err(ApiError::Denied(
            "this token has no device and cannot sync: create a device token with scope sync",
        )),
    }
}

async fn delete_endpoint(
    State(s): State<Shared>,
    Extension(who): Extension<TokenRecord>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    need(&who, Scope::Sync)?;
    let mut st = s.lock().unwrap();
    if who.scope != Scope::Admin && who.endpoint_id.as_deref() != Some(id.as_str()) {
        return Err(ApiError::Forbidden);
    }
    let known = match who.profile_id.as_deref() {
        // A device token only ever reaches its own profile's tree.
        Some(pid) if who.scope != Scope::Admin => st
            .engines
            .get_mut(pid)
            .is_some_and(|e| e.remove_endpoint(&id)),
        _ => st
            .engines
            .values_mut()
            .fold(false, |k, e| e.remove_endpoint(&id) | k),
    };
    let revoked = st.tokens.revoke_endpoint(&id);
    if known || revoked {
        save(&mut st)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
    scope: Scope,
    /// User id or name; required for read and sync tokens unless there is
    /// only one user.
    user: Option<String>,
    /// Profile id or name; may be left out while the user has only one.
    profile: Option<String>,
}

#[derive(Serialize)]
struct CreatedToken {
    /// Shown only once.
    token: String,
    id: String,
    scope: Scope,
    user_id: Option<String>,
    profile_id: Option<String>,
    endpoint_id: Option<String>,
}

async fn create_token(
    State(s): State<Shared>,
    Extension(who): Extension<TokenRecord>,
    Json(body): Json<NewToken>,
) -> Result<(StatusCode, Json<CreatedToken>), ApiError> {
    need(&who, Scope::Admin)?;
    let mut st = s.lock().unwrap();
    let uid = if body.scope == Scope::Admin {
        if body.user.is_some() {
            return Err(ApiError::Unprocessable(
                "an admin token belongs to no user".into(),
            ));
        }
        None
    } else {
        Some(
            st.resolve_user(body.user.as_deref())
                .map_err(ApiError::Unprocessable)?,
        )
    };
    let (token, rec) = st
        .issue_token(
            uid.as_deref(),
            body.profile.as_deref(),
            &body.name,
            body.scope,
        )
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            token,
            id: rec.id,
            scope: rec.scope,
            user_id: rec.user_id,
            profile_id: rec.profile_id,
            endpoint_id: rec.endpoint_id,
        }),
    ))
}

#[derive(Serialize)]
struct UserInfo {
    id: String,
    name: String,
    admin: bool,
    has_password: bool,
}

impl From<&User> for UserInfo {
    fn from(u: &User) -> Self {
        UserInfo {
            id: u.id.clone(),
            name: u.name.clone(),
            admin: u.admin,
            has_password: u.password_hash.is_some(),
        }
    }
}

async fn list_users(State(s): State<Shared>) -> Json<Vec<UserInfo>> {
    let st = s.lock().unwrap();
    Json(st.users.list().map(UserInfo::from).collect())
}

#[derive(Deserialize)]
struct NewUser {
    name: String,
    #[serde(default)]
    admin: bool,
    password: Option<String>,
}

async fn create_user(
    State(s): State<Shared>,
    Json(body): Json<NewUser>,
) -> Result<(StatusCode, Json<UserInfo>), ApiError> {
    // Hashing is slow on purpose: do it off the runtime and outside the lock.
    let hash = match body.password {
        Some(pw) => {
            crate::users::check_password(&pw).map_err(ApiError::Unprocessable)?;
            let hashed = tokio::task::spawn_blocking(move || crate::users::hash_password(&pw))
                .await
                .map_err(|_| ApiError::Storage)?
                .map_err(ApiError::Unprocessable)?;
            Some(hashed)
        }
        None => None,
    };
    let mut st = s.lock().unwrap();
    let user = st
        .add_user_hashed(&body.name, body.admin, hash)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    Ok((StatusCode::CREATED, Json(UserInfo::from(&user))))
}

async fn delete_user(
    State(s): State<Shared>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    if st.remove_user(&id) {
        save(&mut st)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn setup() -> (Shared, String) {
        let mut st = AppState::default();
        st.add_user("tester", false, None).unwrap();
        let (admin, _) = st.tokens.create("admin", Scope::Admin, None, None, None);
        (Arc::new(Mutex::new(st)), admin)
    }

    #[tokio::test]
    async fn client_cert_check_gates_everything_but_health() {
        let (s, admin) = setup();
        s.lock().unwrap().client_cert =
            Some(crate::mtls::ClientCertCheck::parse("X-SSL-Client-Verify").unwrap());
        let app = router(s);
        let get = |uri: &str, hdr: Option<&str>| {
            let mut b = Request::builder().uri(uri.to_string());
            b = b.header("authorization", format!("Bearer {admin}"));
            if let Some(h) = hdr {
                b = b.header("x-ssl-client-verify", h);
            }
            b.body(Body::empty()).unwrap()
        };
        let status = |r: Request<Body>| {
            let app = app.clone();
            async move { app.oneshot(r).await.unwrap().status() }
        };
        assert_eq!(status(get("/health", None)).await, StatusCode::OK);
        assert_eq!(status(get("/v1/users", None)).await, StatusCode::FORBIDDEN);
        assert_eq!(
            status(get("/v1/users", Some("FAILED:certificate has expired"))).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(get("/v1/users", Some("SUCCESS"))).await,
            StatusCode::OK
        );
        // Still needs the token on top of the certificate.
        let no_token = Request::builder()
            .uri("/v1/users")
            .header("x-ssl-client-verify", "SUCCESS")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(no_token).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn tokens_without_a_device_are_told_why() {
        let (s, admin) = setup();
        let app = router(s);
        let (st, v) = call(&app, "POST", "/v1/endpoints", Some(&admin), None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert!(v["error"].as_str().unwrap().contains("scope sync"));
        let (_, v) = call(
            &app,
            "POST",
            "/v1/tokens",
            Some(&admin),
            Some(serde_json::json!({"name":"dash","scope":"read"})),
        )
        .await;
        let read = v["token"].as_str().unwrap().to_string();
        let (st, v) = call(&app, "POST", "/v1/endpoints", Some(&read), None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert!(v["error"].as_str().unwrap().contains("scope sync"));
    }

    #[tokio::test]
    async fn extensions_get_an_unauthenticated_preflight_and_readable_replies() {
        let (s, admin) = setup();
        let app = router(s);
        let preflight = |origin: &str| {
            Request::builder()
                .method("OPTIONS")
                .uri("/v1/endpoints")
                .header("origin", origin)
                .header("access-control-request-method", "POST")
                .header(
                    "access-control-request-headers",
                    "authorization,x-proxy-key",
                )
                .body(Body::empty())
                .unwrap()
        };
        let r = app
            .clone()
            .oneshot(preflight("moz-extension://abc"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let h = r.headers();
        assert_eq!(h["access-control-allow-origin"], "moz-extension://abc");
        assert_eq!(
            h["access-control-allow-headers"],
            "authorization,x-proxy-key"
        );
        // A web page gets nothing: no CORS headers, and the normal 401/405.
        let r = app
            .clone()
            .oneshot(preflight("https://evil.example"))
            .await
            .unwrap();
        assert!(r.headers().get("access-control-allow-origin").is_none());
        assert_ne!(r.status(), StatusCode::NO_CONTENT);
        // The real reply is readable by the extension, and still needs a token.
        let real = |token: Option<&str>| {
            let mut b = Request::builder()
                .method("GET")
                .uri("/v1/tree")
                .header("origin", "chrome-extension://abc");
            if let Some(t) = token {
                b = b.header("authorization", format!("Bearer {t}"));
            }
            b.body(Body::empty()).unwrap()
        };
        let r = app.clone().oneshot(real(None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            r.headers()["access-control-allow-origin"],
            "chrome-extension://abc"
        );
        let (_, v) = call(
            &app,
            "POST",
            "/v1/tokens",
            Some(&admin),
            Some(serde_json::json!({"name":"dash","scope":"read"})),
        )
        .await;
        let read = v["token"].as_str().unwrap().to_string();
        let r = app.oneshot(real(Some(&read))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_793_738_116), "2026-11-03T20:35:16Z");
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn new_device(app: &Router, admin: &str) -> (String, String) {
        let (st, v) = call(
            app,
            "POST",
            "/v1/tokens",
            Some(admin),
            Some(serde_json::json!({"name": "laptop", "scope": "sync"})),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        (
            v["token"].as_str().unwrap().into(),
            v["endpoint_id"].as_str().unwrap().into(),
        )
    }

    #[tokio::test]
    async fn health_needs_no_auth_everything_else_does() {
        let (s, _) = setup();
        let app = router(s);
        assert_eq!(
            call(&app, "GET", "/health", None, None).await.0,
            StatusCode::OK
        );
        for (m, u) in [
            ("GET", "/v1/tree"),
            ("POST", "/v1/sync"),
            ("POST", "/v1/tokens"),
            ("POST", "/v1/endpoints"),
        ] {
            let (st, body) = call(&app, m, u, None, None).await;
            assert_eq!(st, StatusCode::UNAUTHORIZED, "{m} {u}");
            assert_eq!(body["error"], "unauthorized");
        }
        let (st, _) = call(&app, "GET", "/v1/tree", Some("bkm_x_y"), None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn scopes_are_enforced() {
        let (s, admin) = setup();
        let app = router(s);
        let (dev, _) = new_device(&app, &admin).await;
        // sync token cannot create tokens
        let (st, _) = call(
            &app,
            "POST",
            "/v1/tokens",
            Some(&dev),
            Some(serde_json::json!({"name":"x","scope":"read"})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        // read token can read but not sync
        let (_, v) = call(
            &app,
            "POST",
            "/v1/tokens",
            Some(&admin),
            Some(serde_json::json!({"name":"dash","scope":"read"})),
        )
        .await;
        let read = v["token"].as_str().unwrap().to_string();
        assert_eq!(
            call(&app, "GET", "/v1/tree", Some(&read), None).await.0,
            StatusCode::OK
        );
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sync",
            Some(&read),
            Some(serde_json::json!({"endpoint_id":"x","since_rev":0,"ops":[]})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn missing_scope_is_403_even_with_a_bad_body() {
        let (s, admin) = setup();
        let app = router(s);
        let (dev, _) = new_device(&app, &admin).await;
        // sync token + malformed body on an admin route
        let (st, _) = call(
            &app,
            "POST",
            "/v1/tokens",
            Some(&dev),
            Some(serde_json::json!({"nonsense": true})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (_, v) = call(
            &app,
            "POST",
            "/v1/tokens",
            Some(&admin),
            Some(serde_json::json!({"name":"dash","scope":"read"})),
        )
        .await;
        let read = v["token"].as_str().unwrap().to_string();
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sync",
            Some(&read),
            Some(serde_json::json!({"pad": big})),
        )
        .await;
        assert!(
            st == StatusCode::FORBIDDEN || st == StatusCode::PAYLOAD_TOO_LARGE,
            "{st}"
        );
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sync",
            Some(&read),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn token_only_speaks_for_its_own_endpoint() {
        let (s, admin) = setup();
        let app = router(s);
        let (dev, _) = new_device(&app, &admin).await;
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sync",
            Some(&dev),
            Some(serde_json::json!({"endpoint_id":"ep_someone_else","since_rev":0,"ops":[]})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn two_devices_sync_through_http() {
        let (s, admin) = setup();
        let app = router(s);
        let (dev_a, ep_a) = new_device(&app, &admin).await;
        let (dev_b, ep_b) = new_device(&app, &admin).await;
        assert_eq!(
            call(&app, "POST", "/v1/endpoints", Some(&dev_a), None)
                .await
                .1["endpoint_id"],
            ep_a.as_str()
        );
        let (st, r) = call(
            &app,
            "POST",
            "/v1/sync",
            Some(&dev_a),
            Some(serde_json::json!({
                "endpoint_id": ep_a, "since_rev": 0,
                "ops": [{"op_id":"o1","type":"add","id":"bk_1","parent":"toolbar","position":"V","kind":"bookmark","title":"HN","url":"https://news.ycombinator.com"}]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["rev"], 1);
        assert_eq!(r["resync"], false);
        let (_, r) = call(
            &app,
            "POST",
            "/v1/sync",
            Some(&dev_b),
            Some(serde_json::json!({"endpoint_id": ep_b, "since_rev": 0, "ops": []})),
        )
        .await;
        assert_eq!(r["ops"][0]["id"], "bk_1");
        let (_, t) = call(&app, "GET", "/v1/tree", Some(&dev_b), None).await;
        assert_eq!(t["rev"], 1);
        assert!(t["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == "bk_1"));
    }

    #[tokio::test]
    async fn too_many_ops_and_big_bodies_are_refused() {
        let (s, admin) = setup();
        let app = router(s);
        let (dev, ep) = new_device(&app, &admin).await;
        let ops: Vec<_> = (0..501)
            .map(|i| serde_json::json!({"op_id": i.to_string(), "type": "remove", "id": "x"}))
            .collect();
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sync",
            Some(&dev),
            Some(serde_json::json!({"endpoint_id": ep, "since_rev": 0, "ops": ops})),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sync",
            Some(&dev),
            Some(serde_json::json!({"endpoint_id": ep, "since_rev": 0, "ops": [], "pad": big})),
        )
        .await;
        assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn deleting_an_endpoint_revokes_the_device() {
        let (s, admin) = setup();
        let app = router(s);
        let (dev, ep) = new_device(&app, &admin).await;
        let (st, _) = call(
            &app,
            "DELETE",
            &format!("/v1/endpoints/{ep}"),
            Some(&dev),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (st, _) = call(
            &app,
            "POST",
            "/v1/sync",
            Some(&dev),
            Some(serde_json::json!({"endpoint_id": ep, "since_rev": 0, "ops": []})),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_endpoint_after_restore_asks_for_resync() {
        let (s, admin) = setup();
        let app = router(s.clone());
        let (dev, ep) = new_device(&app, &admin).await;
        // Simulate a restored database that forgot the endpoint.
        s.lock().unwrap().engines.values_mut().for_each(|e| {
            e.remove_endpoint(&ep);
        });
        let (_, r) = call(
            &app,
            "POST",
            "/v1/sync",
            Some(&dev),
            Some(serde_json::json!({"endpoint_id": ep, "since_rev": 5, "ops": []})),
        )
        .await;
        assert_eq!(r["resync"], true);
    }

    #[tokio::test]
    async fn bad_tokens_are_throttled_but_good_ones_keep_working() {
        let (state, admin) = setup();
        let app = router(state);
        let from = |peer: &str, token: &str| {
            let mut req = Request::builder()
                .uri("/v1/users")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let addr: SocketAddr = format!("{peer}:4000").parse().unwrap();
            req.extensions_mut().insert(ConnectInfo(addr));
            let app = app.clone();
            async move { app.oneshot(req).await.unwrap().status() }
        };
        let mut last = StatusCode::OK;
        for _ in 0..30 {
            last = from("192.0.2.1", "bkm_x_nope").await;
        }
        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
        // The same address with a valid token is not locked out ...
        assert_eq!(from("192.0.2.1", &admin).await, StatusCode::OK);
        // ... and other addresses have buckets of their own.
        assert_eq!(
            from("192.0.2.2", "bkm_x_nope").await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_busy_token_gets_429_with_retry_after() {
        let (state, admin) = setup();
        state.lock().unwrap().limits.per_token = crate::ratelimit::Limiter::new(3, 0.5, 100);
        let app = router(state);
        for _ in 0..3 {
            assert_eq!(
                call(&app, "GET", "/v1/users", Some(&admin), None).await.0,
                StatusCode::OK
            );
        }
        let req = Request::builder()
            .uri("/v1/users")
            .header("authorization", format!("Bearer {admin}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let wait: u64 = resp.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=3).contains(&wait));
        // /health stays open and unthrottled.
        assert_eq!(
            call(&app, "GET", "/health", None, None).await.0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn users_have_separate_trees_and_only_admins_manage_them() {
        let (s, admin) = setup();
        let app = router(s);
        let (st, v) = call(
            &app,
            "POST",
            "/v1/users",
            Some(&admin),
            Some(serde_json::json!({"name":"bob"})),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(v["has_password"], false);
        // Two users now: a token must say whose it is.
        let mk = |user: Option<&str>| serde_json::json!({"name":"d","scope":"sync","user":user});
        let (st, _) = call(&app, "POST", "/v1/tokens", Some(&admin), Some(mk(None))).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let (st, ta) = call(
            &app,
            "POST",
            "/v1/tokens",
            Some(&admin),
            Some(mk(Some("tester"))),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (_, tb) = call(
            &app,
            "POST",
            "/v1/tokens",
            Some(&admin),
            Some(mk(Some("bob"))),
        )
        .await;
        let (ta_tok, ea) = (
            ta["token"].as_str().unwrap(),
            ta["endpoint_id"].as_str().unwrap(),
        );
        let tb_tok = tb["token"].as_str().unwrap();
        let body = serde_json::json!({"endpoint_id": ea, "since_rev": 0, "ops": [{
            "op_id":"1","type":"add","id":"n1","parent":"toolbar","position":"V",
            "kind":"bookmark","title":"t","url":"https://example.org/"}]});
        let (st, _) = call(&app, "POST", "/v1/sync", Some(ta_tok), Some(body)).await;
        assert_eq!(st, StatusCode::OK);
        let count = |v: &serde_json::Value| v["nodes"].as_array().unwrap().len();
        let (_, a) = call(&app, "GET", "/v1/tree", Some(ta_tok), None).await;
        let (_, b) = call(&app, "GET", "/v1/tree", Some(tb_tok), None).await;
        assert_eq!(count(&a), count(&b) + 1);
        // A device token cannot speak for another user's endpoint.
        let eb = tb["endpoint_id"].as_str().unwrap();
        let body = serde_json::json!({"endpoint_id": eb, "since_rev": 0, "ops": []});
        let (st, _) = call(&app, "POST", "/v1/sync", Some(ta_tok), Some(body)).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        // Admin tokens have no tree; device tokens cannot manage users.
        let (st, _) = call(&app, "GET", "/v1/tree", Some(&admin), None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _) = call(&app, "GET", "/v1/users", Some(ta_tok), None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        // Removing a user revokes their devices and drops their tree.
        let uid = a_user_id(&app, &admin, "bob").await;
        let (st, _) = call(
            &app,
            "DELETE",
            &format!("/v1/users/{uid}"),
            Some(&admin),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (st, _) = call(&app, "GET", "/v1/tree", Some(tb_tok), None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, _) = call(&app, "GET", "/v1/tree", Some(ta_tok), None).await;
        assert_eq!(st, StatusCode::OK);
    }

    async fn a_user_id(app: &Router, admin: &str, name: &str) -> String {
        let (_, v) = call(app, "GET", "/v1/users", Some(admin), None).await;
        v.as_array()
            .unwrap()
            .iter()
            .find(|u| u["name"] == name)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
}
