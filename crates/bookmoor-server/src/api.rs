//! HTTP API (`docs/sync-architecture.md`, section 3.2).

use crate::auth::{random_id, Scope, TokenRecord, Tokens};
use crate::engine::{Engine, SyncError};
use crate::ratelimit::client_key;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Extension;
use axum::{Json, Router};
use bookmoor_core::wire::{SyncRequest, SyncResponse, TreeResponse};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Largest accepted request body.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct AppState {
    pub engine: Engine,
    pub tokens: Tokens,
    /// Where state is saved after every change; `None` keeps it in memory.
    pub store: Option<crate::store::Store>,
    pub limits: crate::ratelimit::Limits,
    /// Where the Dynacat widget file is kept up to date; `None` = off.
    pub dynacat_file: Option<std::path::PathBuf>,
}

impl AppState {
    /// Refresh the Dynacat widget file. A failure is logged and never fails
    /// the sync: the dashboard is a view, the canonical tree is saved.
    pub fn publish_dynacat(&self) {
        if let Some(path) = &self.dynacat_file {
            if let Err(e) = crate::dynacat::publish(path, &self.engine.tree()) {
                eprintln!("warning: cannot write {}: {e}", path.display());
            }
        }
    }

    /// A new token; a sync token comes with its own endpoint.
    pub fn issue_token(
        &mut self,
        name: &str,
        scope: Scope,
    ) -> Result<(String, TokenRecord), String> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err("token name must be 1 to 64 characters without control characters".into());
        }
        let endpoint_id = (scope == Scope::Sync).then(|| random_id("ep_"));
        if let Some(ep) = &endpoint_id {
            self.engine.register_endpoint(ep);
        }
        Ok(self.tokens.create(name, scope, endpoint_id))
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
fn utc_now() -> String {
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

fn retry_after(wait: Duration) -> ApiError {
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
    let gate = |scope: Scope| {
        middleware::from_fn(move |req: Request, next: Next| scope_gate(scope, req, next))
    };
    let v1 = Router::new()
        .route("/v1/tree", get(tree).route_layer(gate(Scope::Read)))
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
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/health", get(health))
        .merge(v1)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(cors_for_extensions))
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}

/// A failed save is reported, never ignored: the caller retries and op ids
/// make that safe.
fn save(st: &mut AppState) -> Result<(), ApiError> {
    st.persist_or_revert().map_err(|e| {
        eprintln!("error: saving state failed: {e}");
        ApiError::Storage
    })
}

async fn health() -> &'static str {
    "ok"
}

async fn tree(
    State(s): State<Shared>,
    Extension(who): Extension<TokenRecord>,
) -> Result<Json<TreeResponse>, ApiError> {
    need(&who, Scope::Read)?;
    let st = s.lock().unwrap();
    Ok(Json(st.engine.tree()))
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
    let before = st.engine.rev();
    match st.engine.sync(&req) {
        Ok(resp) => {
            // Rejected or repeated ops leave the canonical state as it was;
            // a retry after a restart is judged the same way again.
            if st.engine.rev() != before {
                save(&mut st)?;
                st.publish_dynacat();
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
    let known = st.engine.remove_endpoint(&id);
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
}

#[derive(Serialize)]
struct CreatedToken {
    /// Shown only once.
    token: String,
    id: String,
    scope: Scope,
    endpoint_id: Option<String>,
}

async fn create_token(
    State(s): State<Shared>,
    Extension(who): Extension<TokenRecord>,
    Json(body): Json<NewToken>,
) -> Result<(StatusCode, Json<CreatedToken>), ApiError> {
    need(&who, Scope::Admin)?;
    let mut st = s.lock().unwrap();
    let (token, rec) = st
        .issue_token(&body.name, body.scope)
        .map_err(ApiError::Unprocessable)?;
    save(&mut st)?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            token,
            id: rec.id,
            scope: rec.scope,
            endpoint_id: rec.endpoint_id,
        }),
    ))
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
        let (admin, _) = st.tokens.create("admin", Scope::Admin, None);
        (Arc::new(Mutex::new(st)), admin)
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
        let r = app.oneshot(real(Some(&admin))).await.unwrap();
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
        s.lock().unwrap().engine.remove_endpoint(&ep);
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
                .uri("/v1/tree")
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
                call(&app, "GET", "/v1/tree", Some(&admin), None).await.0,
                StatusCode::OK
            );
        }
        let req = Request::builder()
            .uri("/v1/tree")
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
}
