//! Two-factor authentication in the web UI: an authenticator app (TOTP) for
//! the password login, recovery codes, and the admin's say over it. Single
//! sign-on skips all of this, the identity provider does its own.

use super::*;
use crate::mfa;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/ui/api/login/mfa", post(login_mfa))
        .route("/ui/api/mfa", get(status))
        .route("/ui/api/mfa/begin", post(begin))
        .route("/ui/api/mfa/enable", post(enable))
        .route("/ui/api/mfa/disable", post(disable))
        .route("/ui/api/mfa/recovery", post(new_recovery))
        .route("/ui/api/users/{id}/edit", post(edit_user))
        .route("/ui/api/users/{id}/mfa", post(require_mfa))
        .route("/ui/api/users/{id}/mfa/reset", post(reset_mfa))
}

/// What a session that must still set up its second factor may call.
pub fn allowed_before_setup(path: &str) -> bool {
    path.starts_with("/ui/api/mfa") || path == "/ui/api/logout"
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A TOTP code (each only once) or a recovery code (used up).
fn second_factor_ok(user: &mut User, code: &str) -> bool {
    if let Some(t) = &mut user.totp {
        if let Some(step) = mfa::verify(&t.secret, code, now_secs(), t.last_step) {
            t.last_step = step;
            return true;
        }
    }
    mfa::take_recovery(&mut user.recovery, code)
}

#[derive(Deserialize)]
struct MfaLogin {
    ticket: String,
    code: String,
}

/// The second step of a password login.
async fn login_mfa(
    State(s): State<Shared>,
    Peer(peer): Peer,
    Json(b): Json<MfaLogin>,
) -> Result<Response, ApiError> {
    let mut st = s.lock().unwrap();
    throttle(&mut st, peer)?;
    let uid = st
        .sessions
        .try_ticket(&b.ticket, Instant::now())
        .ok_or(ApiError::Unauthorized)?;
    let ok = match st.users.get_mut(&uid) {
        Some(u) if !u.disabled && u.totp.is_some() => second_factor_ok(u, &b.code),
        _ => false,
    };
    if !ok {
        return Err(ApiError::Unauthorized);
    }
    st.sessions.end_ticket(&b.ticket);
    save(&mut st)?;
    let user = st.users.get(&uid).cloned().ok_or(ApiError::Unauthorized)?;
    Ok(signed_in(&mut st, &user))
}

async fn status(State(s): State<Shared>, a: Authed) -> Json<serde_json::Value> {
    let st = s.lock().unwrap();
    let u = st.users.get(&a.user.id).unwrap_or(&a.user);
    Json(serde_json::json!({
        "enabled": u.totp.is_some(),
        "required": u.mfa_required,
        "recovery_left": u.recovery.len(),
    }))
}

/// Start setting up an authenticator: a fresh secret to put into the app.
async fn begin(State(s): State<Shared>, a: Authed) -> Result<Json<serde_json::Value>, ApiError> {
    let mut st = s.lock().unwrap();
    if st.users.get(&a.user.id).is_some_and(|u| u.totp.is_some()) {
        return Err(ApiError::Denied("two-factor authentication is already on"));
    }
    let secret = mfa::new_secret();
    st.sessions.set_setup(&a.user.id, secret.clone());
    Ok(Json(serde_json::json!({
        "secret": secret,
        "uri": mfa::otpauth_uri(&a.user.name, &secret),
    })))
}

#[derive(Deserialize)]
struct Code {
    code: String,
}

/// Finish the setup with a code from the app; returns the recovery codes,
/// shown only now.
async fn enable(
    State(s): State<Shared>,
    Peer(peer): Peer,
    a: Authed,
    Json(b): Json<Code>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut st = s.lock().unwrap();
    throttle(&mut st, peer)?;
    let secret = st
        .sessions
        .setup(&a.user.id)
        .map(String::from)
        .ok_or_else(|| ApiError::Unprocessable("start the setup first".into()))?;
    let step = mfa::verify(&secret, &b.code, now_secs(), 0)
        .ok_or_else(|| ApiError::Unprocessable("that code is not right".into()))?;
    let (plain, hashes) = mfa::new_recovery_codes();
    let u = st.users.get_mut(&a.user.id).ok_or(ApiError::Unauthorized)?;
    u.totp = Some(mfa::Totp {
        secret,
        last_step: step,
    });
    u.recovery = hashes;
    st.sessions.end_setup(&a.user.id);
    save(&mut st)?;
    Ok(Json(serde_json::json!({ "recovery_codes": plain })))
}

#[derive(Deserialize)]
struct Disable {
    password: String,
    code: String,
}

/// Turn it off again; needs the password and a code, and an admin's
/// requirement must be lifted first.
async fn disable(
    State(s): State<Shared>,
    Peer(peer): Peer,
    a: Authed,
    Json(b): Json<Disable>,
) -> Result<StatusCode, ApiError> {
    {
        let mut st = s.lock().unwrap();
        throttle(&mut st, peer)?;
        if a.user.mfa_required {
            return Err(ApiError::Denied(
                "an admin requires two-factor authentication",
            ));
        }
    }
    if a.user.password_hash.is_some()
        && !verify_blocking(a.user.password_hash.clone(), b.password).await
    {
        return Err(ApiError::Denied("the password is wrong"));
    }
    let mut st = s.lock().unwrap();
    let u = st.users.get_mut(&a.user.id).ok_or(ApiError::Unauthorized)?;
    if u.mfa_required {
        return Err(ApiError::Denied(
            "an admin requires two-factor authentication",
        ));
    }
    if u.totp.is_none() || !second_factor_ok(u, &b.code) {
        return Err(ApiError::Denied("that code is not right"));
    }
    u.totp = None;
    u.recovery.clear();
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Replace the recovery codes (the old ones stop working).
async fn new_recovery(
    State(s): State<Shared>,
    Peer(peer): Peer,
    a: Authed,
    Json(b): Json<Code>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut st = s.lock().unwrap();
    throttle(&mut st, peer)?;
    let u = st.users.get_mut(&a.user.id).ok_or(ApiError::Unauthorized)?;
    if u.totp.is_none() || !second_factor_ok(u, &b.code) {
        return Err(ApiError::Denied("that code is not right"));
    }
    let (plain, hashes) = mfa::new_recovery_codes();
    u.recovery = hashes;
    save(&mut st)?;
    Ok(Json(serde_json::json!({ "recovery_codes": plain })))
}

#[derive(Deserialize)]
struct Edit {
    name: Option<String>,
    disabled: Option<bool>,
}

/// Admin: rename a user, or disable / enable them. A disabled user cannot
/// sign in and their tokens stop working; nothing is deleted.
async fn edit_user(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
    Json(b): Json<Edit>,
) -> Result<StatusCode, ApiError> {
    if b.disabled == Some(true) && id == a.user.id {
        return Err(ApiError::Denied("you cannot disable yourself"));
    }
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    if st.users.get(&id).is_none() {
        return Err(ApiError::Unprocessable("no such user".into()));
    }
    if let Some(name) = &b.name {
        st.users
            .rename(&id, name)
            .map_err(ApiError::Unprocessable)?;
    }
    if let Some(d) = b.disabled {
        if let Some(u) = st.users.get_mut(&id) {
            u.disabled = d;
        }
        if d {
            st.sessions.end_user(&id);
        }
    }
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Required {
    required: bool,
}

/// Admin: require (or stop requiring) two-factor authentication of a user.
async fn require_mfa(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
    Json(b): Json<Required>,
) -> Result<StatusCode, ApiError> {
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    st.users
        .get_mut(&id)
        .ok_or_else(|| ApiError::Unprocessable("no such user".into()))?
        .mfa_required = b.required;
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Admin: switch a user's authenticator off (lost phone). They sign in with
/// the password alone, or must set it up again if it is required.
async fn reset_mfa(
    State(s): State<Shared>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if id == a.user.id {
        return Err(ApiError::Denied("use the Account tab for your own"));
    }
    let mut st = s.lock().unwrap();
    still_admin(&st, &a)?;
    let u = st
        .users
        .get_mut(&id)
        .ok_or_else(|| ApiError::Unprocessable("no such user".into()))?;
    u.totp = None;
    u.recovery.clear();
    st.sessions.end_user(&id);
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::api::{router, AppState, Shared};
    use crate::auth::Scope;
    use crate::mfa;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    struct Ui {
        app: axum::Router,
        state: Shared,
        admin: String,
    }

    impl Ui {
        fn new() -> Ui {
            let mut st = AppState::default();
            let (admin, _) = st.tokens.create("admin", Scope::Admin, None, None, None);
            let state = Arc::new(Mutex::new(st));
            Ui {
                app: router(state.clone()),
                state,
                admin,
            }
        }

        async fn send(
            &self,
            method: &str,
            uri: &str,
            cookie: Option<&str>,
            body: Option<Value>,
        ) -> (StatusCode, Value, Option<String>) {
            let mut b = Request::builder()
                .method(method)
                .uri(uri)
                .header("x-bookmoor-ui", "1");
            if let Some(c) = cookie {
                b = b.header("cookie", format!("bookmoor_session={c}"));
            }
            let req = match body {
                Some(v) => b
                    .header("content-type", "application/json")
                    .body(Body::from(v.to_string())),
                None => b.body(Body::empty()),
            }
            .unwrap();
            let resp = self.app.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let set = resp
                .headers()
                .get("set-cookie")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(';').next())
                .map(|v| v.trim_start_matches("bookmoor_session=").to_string());
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                set,
            )
        }

        async fn post(
            &self,
            uri: &str,
            c: Option<&str>,
            body: Value,
        ) -> (StatusCode, Value, Option<String>) {
            self.send("POST", uri, c, Some(body)).await
        }

        async fn setup(&self, name: &str) -> String {
            let (st, _, set) = self
                .post(
                    "/ui/api/setup",
                    None,
                    json!({"admin_token": self.admin, "name": name, "password": "correct horse battery"}),
                )
                .await;
            assert_eq!(st, StatusCode::OK);
            set.unwrap()
        }

        /// A user with a password; returns their id.
        async fn add_user(&self, admin: &str, name: &str) -> String {
            let (st, ..) = self
                .post(
                    "/ui/api/users",
                    Some(admin),
                    json!({"name": name, "password": "another good password"}),
                )
                .await;
            assert_eq!(st, StatusCode::CREATED);
            self.state
                .lock()
                .unwrap()
                .users
                .by_name(name)
                .unwrap()
                .id
                .clone()
        }

        /// Password login of `name`, then `code` as the second factor.
        async fn second_step(&self, name: &str, code: &str) -> StatusCode {
            let (_, v, _) = self.login(name).await;
            let t = v["ticket"].as_str().unwrap().to_string();
            self.post(
                "/ui/api/login/mfa",
                None,
                json!({"ticket": t, "code": code}),
            )
            .await
            .0
        }

        async fn login(&self, name: &str) -> (StatusCode, Value, Option<String>) {
            self.post(
                "/ui/api/login",
                None,
                json!({"name": name, "password": "another good password"}),
            )
            .await
        }

        /// Turn the authenticator on for a signed-in user; returns the secret and the recovery codes.
        async fn enroll(&self, cookie: &str) -> (String, Vec<String>) {
            let (st, v, _) = self
                .post("/ui/api/mfa/begin", Some(cookie), json!({}))
                .await;
            assert_eq!(st, StatusCode::OK);
            let secret = v["secret"].as_str().unwrap().to_string();
            assert!(v["uri"].as_str().unwrap().contains(&secret));
            let code = mfa::code_at(&secret, mfa::step_at(super::now_secs())).unwrap();
            let (st, v, _) = self
                .post("/ui/api/mfa/enable", Some(cookie), json!({"code": code}))
                .await;
            assert_eq!(st, StatusCode::OK, "{v}");
            let codes = v["recovery_codes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_str().unwrap().to_string())
                .collect();
            (secret, codes)
        }
    }

    /// A code that was not used yet (the enrolment used the current step).
    fn next_code(secret: &str) -> String {
        mfa::code_at(secret, mfa::step_at(super::now_secs()) + 1).unwrap()
    }

    #[tokio::test]
    async fn a_second_factor_guards_the_password_login() {
        let u = Ui::new();
        let admin = u.setup("anna").await;
        u.add_user(&admin, "bob").await;
        let (_, _, bob) = u.login("bob").await;
        let bob = bob.unwrap();
        let (secret, recovery) = u.enroll(&bob).await;
        assert_eq!(recovery.len(), mfa::RECOVERY_CODES);

        // The password alone no longer opens a session.
        let (st, v, set) = u.login("bob").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["state"], "mfa");
        assert!(set.is_none());
        let ticket = v["ticket"].as_str().unwrap().to_string();

        // Wrong code, then the right one.
        let (st, ..) = u
            .post(
                "/ui/api/login/mfa",
                None,
                json!({"ticket": ticket, "code": "000000"}),
            )
            .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, v, set) = u
            .post(
                "/ui/api/login/mfa",
                None,
                json!({"ticket": ticket, "code": next_code(&secret)}),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["user"]["mfa"], true);
        let cookie = set.unwrap();
        assert_eq!(
            u.send("GET", "/ui/api/devices", Some(&cookie), None)
                .await
                .0,
            StatusCode::OK
        );

        // The ticket was spent.
        let (st, ..) = u
            .post(
                "/ui/api/login/mfa",
                None,
                json!({"ticket": ticket, "code": next_code(&secret)}),
            )
            .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);

        // A recovery code works once.
        assert_eq!(u.second_step("bob", &recovery[0]).await, StatusCode::OK);
        assert_eq!(
            u.second_step("bob", &recovery[0]).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            u.send("GET", "/ui/api/mfa", Some(&cookie), None).await.1["recovery_left"],
            9
        );
    }

    #[tokio::test]
    async fn a_ticket_survives_only_a_few_wrong_codes() {
        let u = Ui::new();
        let admin = u.setup("anna").await;
        u.add_user(&admin, "bob").await;
        let (_, _, bob) = u.login("bob").await;
        let (secret, _) = u.enroll(&bob.unwrap()).await;
        let (_, v, _) = u.login("bob").await;
        let t = v["ticket"].as_str().unwrap().to_string();
        for _ in 0..5 {
            let (st, ..) = u
                .post(
                    "/ui/api/login/mfa",
                    None,
                    json!({"ticket": t, "code": "000000"}),
                )
                .await;
            assert_eq!(st, StatusCode::UNAUTHORIZED);
        }
        let (st, ..) = u
            .post(
                "/ui/api/login/mfa",
                None,
                json!({"ticket": t, "code": next_code(&secret)}),
            )
            .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "the ticket is burnt");
    }

    #[tokio::test]
    async fn setup_needs_a_matching_code_and_turning_off_needs_proof() {
        let u = Ui::new();
        let admin = u.setup("anna").await;
        // No setup started, wrong code.
        let (st, ..) = u
            .post(
                "/ui/api/mfa/enable",
                Some(&admin),
                json!({"code": "123456"}),
            )
            .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        u.post("/ui/api/mfa/begin", Some(&admin), json!({})).await;
        let (st, ..) = u
            .post(
                "/ui/api/mfa/enable",
                Some(&admin),
                json!({"code": "000000"}),
            )
            .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(u
            .state
            .lock()
            .unwrap()
            .users
            .by_name("anna")
            .unwrap()
            .totp
            .is_none());

        let (secret, _) = u.enroll(&admin).await;
        let (st, ..) = u.post("/ui/api/mfa/begin", Some(&admin), json!({})).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "already on");

        let off = |pw: &str, code: String| json!({"password": pw, "code": code});
        let (st, ..) = u
            .post(
                "/ui/api/mfa/disable",
                Some(&admin),
                off("wrong password!!", next_code(&secret)),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, ..) = u
            .post(
                "/ui/api/mfa/disable",
                Some(&admin),
                off("correct horse battery", "000000".into()),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, ..) = u
            .post(
                "/ui/api/mfa/disable",
                Some(&admin),
                off("correct horse battery", next_code(&secret)),
            )
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let me = u.send("GET", "/ui/api/mfa", Some(&admin), None).await.1;
        assert_eq!(me["enabled"], false);
    }

    #[tokio::test]
    async fn new_recovery_codes_replace_the_old_ones() {
        let u = Ui::new();
        let admin = u.setup("anna").await;
        let (secret, old) = u.enroll(&admin).await;
        let (st, v, _) = u
            .post(
                "/ui/api/mfa/recovery",
                Some(&admin),
                json!({"code": next_code(&secret)}),
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        let fresh = v["recovery_codes"][0].as_str().unwrap().to_string();
        assert_ne!(fresh, old[0]);
        let mut st = u.state.lock().unwrap();
        let id = st.users.by_name("anna").unwrap().id.clone();
        let user = st.users.get_mut(&id).unwrap();
        assert!(!mfa::take_recovery(&mut user.recovery, &old[0]));
        assert!(mfa::take_recovery(&mut user.recovery, &fresh));
    }

    #[tokio::test]
    async fn an_admin_can_require_it_and_reset_it() {
        let u = Ui::new();
        let admin = u.setup("anna").await;
        let bob_id = u.add_user(&admin, "bob").await;
        let (_, _, bob) = u.login("bob").await;
        let bob = bob.unwrap();

        // Required: bob can do nothing but set it up.
        let (st, ..) = u
            .post(
                &format!("/ui/api/users/{bob_id}/mfa"),
                Some(&admin),
                json!({"required": true}),
            )
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        assert_eq!(
            u.send("GET", "/ui/api/devices", Some(&bob), None).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            u.send("GET", "/ui/api/mfa", Some(&bob), None).await.0,
            StatusCode::OK
        );
        let (secret, _) = u.enroll(&bob).await;
        assert_eq!(
            u.send("GET", "/ui/api/devices", Some(&bob), None).await.0,
            StatusCode::OK
        );

        // Required: bob cannot switch it off.
        let (st, ..) = u
            .post(
                "/ui/api/mfa/disable",
                Some(&bob),
                json!({"password": "another good password", "code": next_code(&secret)}),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        // Lost phone: the admin resets it, bob's sessions end, he must enrol again.
        let (st, ..) = u
            .post(
                &format!("/ui/api/users/{bob_id}/mfa/reset"),
                Some(&admin),
                json!({}),
            )
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        assert_eq!(
            u.send("GET", "/ui/api/mfa", Some(&bob), None).await.0,
            StatusCode::UNAUTHORIZED
        );
        let (_, v, set) = u.login("bob").await;
        assert_eq!(v["state"], "user", "no second factor to ask for");
        assert_eq!(
            u.send("GET", "/ui/api/devices", Some(&set.unwrap()), None)
                .await
                .0,
            StatusCode::FORBIDDEN
        );

        // Not for plain users, and not on oneself.
        let anna_id = u
            .state
            .lock()
            .unwrap()
            .users
            .by_name("anna")
            .unwrap()
            .id
            .clone();
        assert_eq!(
            u.post(
                &format!("/ui/api/users/{anna_id}/mfa/reset"),
                Some(&admin),
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let (_, _, bob) = u.login("bob").await;
        assert_eq!(
            u.post(
                &format!("/ui/api/users/{bob_id}/mfa"),
                bob.as_deref(),
                json!({"required": false})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn an_admin_can_rename_and_disable_users() {
        let u = Ui::new();
        let admin = u.setup("anna").await;
        let bob_id = u.add_user(&admin, "bob").await;
        u.add_user(&admin, "carl").await;
        let (_, _, bob) = u.login("bob").await;
        let bob = bob.unwrap();
        let edit = |id: &str| format!("/ui/api/users/{id}/edit");

        // Names stay unique and valid.
        assert_eq!(
            u.post(&edit(&bob_id), Some(&admin), json!({"name": "CARL"}))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            u.post(&edit(&bob_id), Some(&admin), json!({"name": "a b"}))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            u.post(&edit(&bob_id), Some(&admin), json!({"name": "robert"}))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(u.login("bob").await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(u.login("robert").await.0, StatusCode::OK);

        // A disabled user is out everywhere, and comes back unharmed.
        let sync = {
            let mut st = u.state.lock().unwrap();
            let p = st.profiles.for_user(&bob_id)[0].id.clone();
            st.tokens
                .create("laptop", Scope::Read, Some(bob_id.clone()), Some(p), None)
                .0
        };
        let tree = || async {
            let mut b = Request::builder()
                .uri("/v1/tree")
                .header("authorization", format!("Bearer {sync}"));
            b = b.method("GET");
            u.app
                .clone()
                .oneshot(b.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        };
        assert_eq!(tree().await, StatusCode::OK);
        assert_eq!(
            u.post(&edit(&bob_id), Some(&admin), json!({"disabled": true}))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            u.send("GET", "/ui/api/devices", Some(&bob), None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(u.login("robert").await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(tree().await, StatusCode::UNAUTHORIZED);
        let listed = u.send("GET", "/ui/api/users", Some(&admin), None).await.1;
        assert!(listed
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "robert" && r["disabled"] == true));
        assert_eq!(
            u.post(&edit(&bob_id), Some(&admin), json!({"disabled": false}))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(u.login("robert").await.0, StatusCode::OK);
        assert_eq!(tree().await, StatusCode::OK);

        // Not yourself, and not as a plain user.
        let anna_id = u
            .state
            .lock()
            .unwrap()
            .users
            .by_name("anna")
            .unwrap()
            .id
            .clone();
        assert_eq!(
            u.post(&edit(&anna_id), Some(&admin), json!({"disabled": true}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (_, _, rob) = u.login("robert").await;
        assert_eq!(
            u.post(&edit(&anna_id), rob.as_deref(), json!({"name": "x"}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
}
