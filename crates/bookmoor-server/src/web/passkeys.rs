//! Passkeys (WebAuthn) in the web UI: registering and managing them in the
//! Account tab, signing in with one instead of a password, using one as the
//! second factor after a password, and the admin's reset.
//!
//! The relying party id is the host name the browser is on (taken from the
//! `Origin` header, which a browser sets and a page cannot forge), so this
//! works behind any reverse proxy without configuration. A passkey only
//! ever works on the host it was made for.

use super::mfa::{now_secs, Begin};
use super::*;
use crate::webauthn::{self as wa, Passkey};

/// Adding a passkey without a password needs a session this young.
const FRESH_SESSION: std::time::Duration = std::time::Duration::from_secs(15 * 60);

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/ui/api/passkeys", get(list))
        .route("/ui/api/passkeys/begin", post(begin))
        .route("/ui/api/passkeys/finish", post(finish))
        .route("/ui/api/passkeys/{id}", axum::routing::delete(remove))
        .route("/ui/api/passkeys/{id}/rename", post(rename))
        .route("/ui/api/login/passkey/begin", post(login_begin))
        .route("/ui/api/login/passkey/finish", post(login_finish))
        .route("/ui/api/login/mfa/passkey/begin", post(mfa_begin))
        .route("/ui/api/login/mfa/passkey/finish", post(mfa_finish))
        .route("/ui/api/users/{id}/passkeys/reset", post(reset))
}

/// (relying party id, origin) of the request.
fn rp(headers: &HeaderMap) -> Result<(String, String), ApiError> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::Unprocessable("the browser sent no Origin".into()))?;
    wa::rp_from_origin(origin).map_err(ApiError::Unprocessable)
}

fn credential_ids(user: &User) -> Vec<serde_json::Value> {
    user.passkeys
        .iter()
        .map(|p| serde_json::json!({ "type": "public-key", "id": p.id }))
        .collect()
}

fn rows(user: &User) -> Vec<serde_json::Value> {
    user.passkeys
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "name": p.name,
                "created": p.created,
                "last_used": p.last_used,
            })
        })
        .collect()
}

async fn list(State(s): State<Shared>, a: Authed) -> Json<Vec<serde_json::Value>> {
    let st = s.lock().unwrap();
    Json(rows(st.users.get(&a.user.id).unwrap_or(&a.user)))
}

/// Start adding a passkey: the options for `navigator.credentials.create`.
async fn begin(
    State(s): State<Shared>,
    Peer(peer): Peer,
    headers: HeaderMap,
    a: Authed,
    Json(b): Json<Begin>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (rp_id, origin) = rp(&headers)?;
    {
        let mut st = s.lock().unwrap();
        throttle(&mut st, peer)?;
    }
    // A stolen session cookie alone must not be able to enrol the thief's
    // key. Without a password to ask for (single sign-on, passkey-only), the
    // session has to be fresh instead.
    if a.user.password_hash.is_some() {
        if !verify_blocking(a.user.password_hash.clone(), b.password).await {
            return Err(ApiError::Denied("the password is wrong"));
        }
    } else if s
        .lock()
        .unwrap()
        .sessions
        .age(&a.session, Instant::now())
        .is_none_or(|age| age > FRESH_SESSION)
    {
        return Err(ApiError::Denied("sign in again to add a passkey"));
    }
    let mut st = s.lock().unwrap();
    let user = st
        .users
        .get(&a.user.id)
        .cloned()
        .ok_or(ApiError::Unauthorized)?;
    if user.passkeys.len() >= wa::MAX_PASSKEYS {
        return Err(ApiError::Unprocessable(format!(
            "at most {} passkeys",
            wa::MAX_PASSKEYS
        )));
    }
    let challenge = wa::new_challenge();
    let ceremony = st
        .sessions
        .start_ceremony(Some(&user.id), &challenge, &origin, Instant::now());
    Ok(Json(serde_json::json!({
        "ceremony": ceremony,
        "options": {
            "rp": { "id": rp_id, "name": "bookmoor" },
            "user": {
                "id": wa::b64_encode(user.id.as_bytes()),
                "name": user.name,
                "displayName": user.name,
            },
            "challenge": challenge,
            "pubKeyCredParams": [{ "type": "public-key", "alg": wa::ALG_ES256 }],
            "timeout": 120_000,
            "attestation": "none",
            "authenticatorSelection": { "residentKey": "required", "userVerification": "required" },
            "excludeCredentials": credential_ids(&user),
        }
    })))
}

#[derive(Deserialize)]
struct Finish {
    ceremony: String,
    #[serde(default)]
    name: String,
    client_data_json: String,
    attestation_object: String,
}

/// Finish adding a passkey with the browser's answer.
async fn finish(
    State(s): State<Shared>,
    headers: HeaderMap,
    a: Authed,
    Json(b): Json<Finish>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let (rp_id, origin) = rp(&headers)?;
    let name = wa::check_name(if b.name.trim().is_empty() {
        "Passkey"
    } else {
        &b.name
    })
    .map_err(ApiError::Unprocessable)?;
    let mut st = s.lock().unwrap();
    let (challenge, started_origin, owner) = st
        .sessions
        .take_ceremony(&b.ceremony, Instant::now())
        .ok_or_else(|| ApiError::Unprocessable("that took too long, start again".into()))?;
    if owner.as_deref() != Some(a.user.id.as_str()) || started_origin != origin {
        return Err(ApiError::Unprocessable("start again from this page".into()));
    }
    let decode = |t: &str| wa::b64_decode(t).map_err(ApiError::Unprocessable);
    let reg = wa::verify_registration(
        &decode(&b.client_data_json)?,
        &decode(&b.attestation_object)?,
        &challenge,
        &origin,
        &rp_id,
    )
    .map_err(ApiError::Unprocessable)?;
    let id = wa::b64_encode(&reg.id);
    if st.users.by_passkey(&id).is_some() {
        return Err(ApiError::Unprocessable(
            "that passkey is already registered".into(),
        ));
    }
    let u = st.users.get_mut(&a.user.id).ok_or(ApiError::Unauthorized)?;
    if u.passkeys.len() >= wa::MAX_PASSKEYS {
        return Err(ApiError::Unprocessable(format!(
            "at most {} passkeys",
            wa::MAX_PASSKEYS
        )));
    }
    u.passkeys.push(Passkey {
        id,
        name,
        created: now_secs(),
        last_used: None,
        rp_id,
        public_key: wa::b64_encode(&reg.public_key),
        sign_count: reg.sign_count,
    });
    let out = rows(u);
    save(&mut st)?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct Rename {
    name: String,
}

async fn rename(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
    Json(b): Json<Rename>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let name = wa::check_name(&b.name).map_err(ApiError::Unprocessable)?;
    let mut st = s.lock().unwrap();
    let u = st.users.get_mut(&a.user.id).ok_or(ApiError::Unauthorized)?;
    u.passkeys
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::Unprocessable("no such passkey".into()))?
        .name = name;
    let out = rows(u);
    save(&mut st)?;
    Ok(Json(out))
}

/// Delete a passkey. It may not leave the user without a way in, or without
/// the second factor an admin requires.
async fn remove(
    State(s): State<Shared>,
    a: Authed,
    Path(id): Path<String>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let mut st = s.lock().unwrap();
    let u = st.users.get_mut(&a.user.id).ok_or(ApiError::Unauthorized)?;
    let Some(at) = u.passkeys.iter().position(|p| p.id == id) else {
        return Err(ApiError::Unprocessable("no such passkey".into()));
    };
    if u.passkeys.len() == 1 {
        if u.password_hash.is_none() && u.oidc.is_none() {
            return Err(ApiError::Denied(
                "this is your only way to sign in, set a password first",
            ));
        }
        if u.mfa_required && u.totp.is_none() {
            return Err(ApiError::Denied(
                "an admin requires two-factor authentication, add another passkey or an authenticator app first",
            ));
        }
    }
    u.passkeys.remove(at);
    let out = rows(u);
    save(&mut st)?;
    Ok(Json(out))
}

/// The options for `navigator.credentials.get`.
fn get_options(
    rp_id: &str,
    challenge: &str,
    allow: Option<Vec<serde_json::Value>>,
) -> serde_json::Value {
    let mut o = serde_json::json!({
        "challenge": challenge,
        "rpId": rp_id,
        "timeout": 120_000,
        "userVerification": "required",
    });
    if let Some(a) = allow {
        o["allowCredentials"] = a.into();
    }
    o
}

/// Passwordless sign-in, step one: a challenge for any passkey of this site.
async fn login_begin(
    State(s): State<Shared>,
    Peer(peer): Peer,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (rp_id, origin) = rp(&headers)?;
    let mut st = s.lock().unwrap();
    throttle(&mut st, peer)?;
    let challenge = wa::new_challenge();
    let ceremony = st
        .sessions
        .start_ceremony(None, &challenge, &origin, Instant::now());
    Ok(Json(serde_json::json!({
        "ceremony": ceremony,
        "options": get_options(&rp_id, &challenge, None),
    })))
}

#[derive(Deserialize)]
struct Assertion {
    ceremony: String,
    id: String,
    client_data_json: String,
    authenticator_data: String,
    signature: String,
}

/// Check the browser's answer against the passkey it names; `expect_user`
/// pins the user when a password already identified them. Counts the
/// signature counter and the time of use.
fn check_assertion(
    st: &mut AppState,
    headers: &HeaderMap,
    b: &Assertion,
    expect_user: Option<&str>,
) -> Result<User, ApiError> {
    let (_, origin) = rp(headers)?;
    let (challenge, started_origin, started_user) = st
        .sessions
        .take_ceremony(&b.ceremony, Instant::now())
        .ok_or(ApiError::Unauthorized)?;
    if started_origin != origin || started_user.as_deref() != expect_user {
        return Err(ApiError::Unauthorized);
    }
    let user = st
        .users
        .by_passkey(&b.id)
        .filter(|u| !u.disabled && expect_user.is_none_or(|e| e == u.id))
        .cloned()
        .ok_or(ApiError::Unauthorized)?;
    let key = user
        .passkeys
        .iter()
        .find(|p| p.id == b.id)
        .ok_or(ApiError::Unauthorized)?;
    let decode = |t: &str| wa::b64_decode(t).map_err(|_| ApiError::Unauthorized);
    let count = wa::verify_assertion(
        &decode(&b.client_data_json)?,
        &decode(&b.authenticator_data)?,
        &decode(&b.signature)?,
        &challenge,
        &origin,
        key,
    )
    .map_err(|_| ApiError::Unauthorized)?;
    let u = st.users.get_mut(&user.id).ok_or(ApiError::Unauthorized)?;
    if let Some(p) = u.passkeys.iter_mut().find(|p| p.id == b.id) {
        p.sign_count = count;
        p.last_used = Some(now_secs());
    }
    Ok(u.clone())
}

/// Passwordless sign-in, step two. A passkey checks the user's presence and
/// identity (PIN, fingerprint, face) itself, so no second factor follows.
async fn login_finish(
    State(s): State<Shared>,
    Peer(peer): Peer,
    headers: HeaderMap,
    Json(b): Json<Assertion>,
) -> Result<Response, ApiError> {
    let mut st = s.lock().unwrap();
    throttle(&mut st, peer)?;
    let user = check_assertion(&mut st, &headers, &b, None)?;
    save(&mut st)?;
    Ok(signed_in(&mut st, &user))
}

#[derive(Deserialize)]
struct Ticket {
    ticket: String,
}

/// Second factor after the password, step one: a challenge for this user's
/// passkeys.
async fn mfa_begin(
    State(s): State<Shared>,
    Peer(peer): Peer,
    headers: HeaderMap,
    Json(b): Json<Ticket>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (rp_id, origin) = rp(&headers)?;
    let mut st = s.lock().unwrap();
    throttle(&mut st, peer)?;
    let uid = st
        .sessions
        .ticket_user(&b.ticket, Instant::now())
        .ok_or(ApiError::Unauthorized)?;
    let user = st
        .users
        .get(&uid)
        .filter(|u| !u.disabled && !u.passkeys.is_empty())
        .cloned()
        .ok_or(ApiError::Unauthorized)?;
    let challenge = wa::new_challenge();
    let ceremony = st
        .sessions
        .start_ceremony(Some(&uid), &challenge, &origin, Instant::now());
    Ok(Json(serde_json::json!({
        "ceremony": ceremony,
        "options": get_options(&rp_id, &challenge, Some(credential_ids(&user))),
    })))
}

#[derive(Deserialize)]
struct MfaAssertion {
    ticket: String,
    #[serde(flatten)]
    assertion: Assertion,
}

async fn mfa_finish(
    State(s): State<Shared>,
    headers: HeaderMap,
    Json(b): Json<MfaAssertion>,
) -> Result<Response, ApiError> {
    let mut st = s.lock().unwrap();
    let uid = st
        .sessions
        .try_ticket(&b.ticket, Instant::now())
        .ok_or(ApiError::Unauthorized)?;
    let user = check_assertion(&mut st, &headers, &b.assertion, Some(&uid))?;
    st.sessions.end_ticket(&b.ticket);
    save(&mut st)?;
    Ok(signed_in(&mut st, &user))
}

/// Admin: delete all of a user's passkeys (lost device). They sign in with
/// the password, or must set up a second factor again if it is required.
async fn reset(
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
    u.passkeys.clear();
    st.sessions.end_user(&id);
    st.sessions.end_setup(&id);
    save(&mut st)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::super::mfa::tests::{Ui, ANNA, BOB};
    use crate::webauthn::b64_encode as b64;
    use crate::webauthn::tests::{Authenticator, OK, ORIGIN, RP};
    use axum::http::StatusCode;
    use serde_json::{json, Value};

    /// Register a passkey for the signed-in `cookie`; returns its id.
    async fn register(u: &Ui, cookie: &str, pw: &str, a: &Authenticator, name: &str) -> String {
        let (st, v, _) = u
            .post(
                "/ui/api/passkeys/begin",
                Some(cookie),
                json!({"password": pw}),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let chal = v["options"]["challenge"].as_str().unwrap();
        assert_eq!(v["options"]["rp"]["id"], RP);
        let (cd, att) = a.create(chal, ORIGIN, RP, OK);
        let (st, v, _) = u
            .post(
                "/ui/api/passkeys/finish",
                Some(cookie),
                json!({"ceremony": v["ceremony"], "name": name,
                       "client_data_json": b64(&cd), "attestation_object": b64(&att)}),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        b64(&a.id)
    }

    fn assertion(ceremony: &Value, chal: &str, a: &mut Authenticator) -> Value {
        let (cd, ad, sig) = a.get(chal, ORIGIN, RP, OK);
        json!({"ceremony": ceremony, "id": b64(&a.id), "client_data_json": b64(&cd),
               "authenticator_data": b64(&ad), "signature": b64(&sig)})
    }

    #[tokio::test]
    async fn a_passkey_signs_in_without_a_password() {
        let u = Ui::new();
        let anna = u.setup("anna").await;
        let mut key = Authenticator::new();
        register(&u, &anna, ANNA, &key, "Laptop").await;
        // Registering asks for the password.
        let (st, ..) = u
            .post(
                "/ui/api/passkeys/begin",
                Some(&anna),
                json!({"password": "wrong wrong wrong"}),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        let (st, v, _) = u.post("/ui/api/login/passkey/begin", None, json!({})).await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["options"].get("allowCredentials").is_none());
        let chal = v["options"]["challenge"].as_str().unwrap().to_string();
        let body = assertion(&v["ceremony"], &chal, &mut key);
        let (st, v2, set) = u
            .post("/ui/api/login/passkey/finish", None, body.clone())
            .await;
        assert_eq!(st, StatusCode::OK, "{v2}");
        assert_eq!(v2["user"]["name"], "anna");
        assert!(set.is_some());
        // The answer works once.
        let (st, ..) = u.post("/ui/api/login/passkey/finish", None, body).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        // last_used is recorded.
        let used = u
            .state
            .lock()
            .unwrap()
            .users
            .by_name("anna")
            .unwrap()
            .passkeys[0]
            .last_used;
        assert!(used.is_some());
    }

    #[tokio::test]
    async fn unknown_keys_other_origins_and_disabled_users_are_refused() {
        let u = Ui::new();
        let anna = u.setup("anna").await;
        let mut key = Authenticator::new();
        register(&u, &anna, ANNA, &key, "Laptop").await;
        let mut stranger = Authenticator::new();

        let try_with =
            |who: &mut Authenticator, origin: &'static str| {
                let u = &u;
                let (cd_origin, who_id) = (origin, b64(&who.id));
                let mut who_key = Authenticator {
                    key: who.key.clone(),
                    id: who.id.clone(),
                    counter: who.counter,
                };
                async move {
                    let (_, v, _) = u.post("/ui/api/login/passkey/begin", None, json!({})).await;
                    let chal = v["options"]["challenge"].as_str().unwrap().to_string();
                    let (cd, ad, sig) = who_key.get(&chal, cd_origin, RP, OK);
                    u.post("/ui/api/login/passkey/finish", None,
                    json!({"ceremony": v["ceremony"], "id": who_id, "client_data_json": b64(&cd),
                           "authenticator_data": b64(&ad), "signature": b64(&sig)})).await.0
                }
            };
        assert_eq!(
            try_with(&mut stranger, ORIGIN).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            try_with(&mut key, "https://evil.example").await,
            StatusCode::UNAUTHORIZED
        );

        // Disabled users cannot sign in with a passkey either.
        let id = u
            .state
            .lock()
            .unwrap()
            .users
            .by_name("anna")
            .unwrap()
            .id
            .clone();
        u.state.lock().unwrap().users.get_mut(&id).unwrap().disabled = true;
        assert_eq!(try_with(&mut key, ORIGIN).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_passkey_can_be_the_second_factor_after_the_password() {
        let u = Ui::new();
        let admin = u.setup("anna").await;
        u.add_user(&admin, "bob").await;
        let (_, _, bob) = u.login("bob").await;
        let bob = bob.unwrap();
        let mut key = Authenticator::new();
        register(&u, &bob, BOB, &key, "Phone").await;

        let (_, v, set) = u.login("bob").await;
        assert_eq!(v["state"], "mfa");
        assert_eq!(v["passkey"], true);
        assert_eq!(v["totp"], false);
        assert!(set.is_none());
        let ticket = v["ticket"].as_str().unwrap().to_string();

        let (st, v, _) = u
            .post(
                "/ui/api/login/mfa/passkey/begin",
                None,
                json!({"ticket": ticket}),
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            v["options"]["allowCredentials"].as_array().unwrap().len(),
            1
        );
        let chal = v["options"]["challenge"].as_str().unwrap().to_string();
        let mut body = assertion(&v["ceremony"], &chal, &mut key);
        body["ticket"] = json!(ticket);

        // A ticket of someone else does not accept bob's passkey.
        let (_, other, _) = u
            .post(
                "/ui/api/login",
                None,
                json!({"name": "anna", "password": ANNA}),
            )
            .await;
        assert_eq!(other["state"], "user");

        let (st, v, set) = u.post("/ui/api/login/mfa/passkey/finish", None, body).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["user"]["name"], "bob");
        assert!(set.is_some());
    }

    #[tokio::test]
    async fn a_passkey_does_not_answer_for_another_ticket() {
        let u = Ui::new();
        let admin = u.setup("anna").await;
        u.add_user(&admin, "bob").await;
        let mut key = Authenticator::new();
        register(&u, &admin, ANNA, &key, "Key").await;
        // Bob has some passkey too, so he gets a ticket.
        let (_, _, bob) = u.login("bob").await;
        register(&u, &bob.unwrap(), BOB, &Authenticator::new(), "Other").await;
        let (_, v, _) = u.login("bob").await;
        let ticket = v["ticket"].as_str().unwrap().to_string();
        let (_, v, _) = u
            .post(
                "/ui/api/login/mfa/passkey/begin",
                None,
                json!({"ticket": ticket}),
            )
            .await;
        let chal = v["options"]["challenge"].as_str().unwrap().to_string();
        let mut body = assertion(&v["ceremony"], &chal, &mut key); // anna's key
        body["ticket"] = json!(ticket);
        let (st, ..) = u.post("/ui/api/login/mfa/passkey/finish", None, body).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn passkeys_are_renamed_deleted_and_reset_by_an_admin() {
        let u = Ui::new();
        let admin = u.setup("anna").await;
        u.add_user(&admin, "bob").await;
        let (_, _, bob) = u.login("bob").await;
        let bob = bob.unwrap();
        let id = register(&u, &bob, BOB, &Authenticator::new(), "Phone").await;
        register(&u, &bob, BOB, &Authenticator::new(), "Key").await;

        let (st, v, _) = u
            .post(
                &format!("/ui/api/passkeys/{id}/rename"),
                Some(&bob),
                json!({"name": "Pixel"}),
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v[0]["name"], "Pixel");
        let (st, ..) = u
            .post(
                &format!("/ui/api/passkeys/{id}/rename"),
                Some(&bob),
                json!({"name": "  "}),
            )
            .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

        let (st, v, _) = u
            .send(
                "DELETE",
                &format!("/ui/api/passkeys/{id}"),
                Some(&bob),
                None,
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v.as_array().unwrap().len(), 1);

        // An admin clears the rest and bob falls back to the password.
        let bob_id = u
            .state
            .lock()
            .unwrap()
            .users
            .by_name("bob")
            .unwrap()
            .id
            .clone();
        let (st, ..) = u
            .post(
                &format!("/ui/api/users/{bob_id}/passkeys/reset"),
                Some(&bob),
                json!({}),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "not an admin");
        let (st, ..) = u
            .post(
                &format!("/ui/api/users/{bob_id}/passkeys/reset"),
                Some(&admin),
                json!({}),
            )
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (_, v, set) = u.login("bob").await;
        assert_eq!(v["state"], "user");
        assert!(set.is_some());
    }

    #[tokio::test]
    async fn a_required_second_factor_can_be_a_passkey_and_the_last_one_stays() {
        let u = Ui::new();
        let admin = u.setup("anna").await;
        let bob_id = u.add_user(&admin, "bob").await;
        u.post(
            &format!("/ui/api/users/{bob_id}/mfa"),
            Some(&admin),
            json!({"required": true}),
        )
        .await;
        let (_, v, bob) = u.login("bob").await;
        assert_eq!(v["user"]["mfa_setup_needed"], true);
        let bob = bob.unwrap();
        // Until a factor exists, other calls are refused.
        let (st, ..) = u.send("GET", "/ui/api/devices", Some(&bob), None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let id = register(&u, &bob, BOB, &Authenticator::new(), "Key").await;
        let (st, ..) = u.send("GET", "/ui/api/devices", Some(&bob), None).await;
        assert_eq!(st, StatusCode::OK);
        // The only factor cannot go while it is required.
        let (st, ..) = u
            .send(
                "DELETE",
                &format!("/ui/api/passkeys/{id}"),
                Some(&bob),
                Some(json!({"password": BOB})),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn only_https_hosts_can_register() {
        let u = Ui::new();
        let anna = u.setup("anna").await;
        // No Origin header: the harness always sets one, so go around it.
        let st = u.state.clone();
        let app = crate::api::router(st);
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let r = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ui/api/passkeys/begin")
                    .header("x-bookmoor-ui", "1")
                    .header("origin", "http://bookmoor.example")
                    .header("cookie", format!("bookmoor_session={anna}"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"password": ANNA}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
