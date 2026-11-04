//! `bookmoor-server token create`: mint a token without a running server or
//! a working admin token (for example after losing it).
//!
//! Guarded against accidents: it needs an existing state file named
//! explicitly, refuses while a server (or another token command) holds the
//! file, asks to retype the token name unless `--yes` is given, and makes
//! admin tokens require `--allow-admin`.

use crate::auth::Scope;
use crate::store::{Lock, Store};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

pub struct TokenRequest<'a> {
    pub state: &'a Path,
    pub name: &'a str,
    pub scope: Scope,
    /// Owner of a read or sync token; may be left out while there is only
    /// one user.
    pub user: Option<&'a str>,
    pub allow_admin: bool,
    /// Skip the confirmation prompt (required without a terminal).
    pub yes: bool,
}

/// Create the token and return its text, shown only once.
pub fn create_token(
    req: &TokenRequest,
    confirm: impl FnOnce(&str) -> Result<bool, String>,
) -> Result<String, String> {
    let name = req.name.trim();
    if req.scope == Scope::Admin && !req.allow_admin {
        return Err("an admin token can do everything; pass --allow-admin if you mean it".into());
    }
    if !req.state.exists() {
        return Err(format!(
            "{} does not exist; start the server once with --state to create it",
            req.state.display()
        ));
    }
    let _lock = Lock::acquire(req.state)?;
    let mut state = crate::api::AppState::open(Store::new(req.state))?;
    if !req.yes {
        let what = format!(
            "Create a token with scope {} named {name:?} in {}",
            req.scope,
            req.state.display()
        );
        if !confirm(&what)? {
            return Err("not confirmed, nothing was changed".into());
        }
    }
    let user_id = if req.scope == Scope::Admin {
        None
    } else {
        Some(pick_user(&state, req.user)?)
    };
    let (token, _) = state.issue_token(user_id.as_deref(), name, req.scope)?;
    state
        .persist()
        .map_err(|e| format!("cannot save the state: {e}"))?;
    Ok(token)
}

/// Ask on the terminal to retype the name; without a terminal, refuse.
pub fn confirm_on_terminal(name: &str) -> impl FnOnce(&str) -> Result<bool, String> + '_ {
    move |what| {
        if !std::io::stdin().is_terminal() {
            return Err("no terminal to ask on; pass --yes to confirm".into());
        }
        eprintln!("{what}.");
        eprint!("Type the token name ({name}) to confirm: ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        Ok(line.trim() == name.trim())
    }
}

/// The named user, or the only one.
fn pick_user(state: &crate::api::AppState, name: Option<&str>) -> Result<String, String> {
    match name {
        Some(n) => state
            .users
            .by_name(n)
            .map(|u| u.id.clone())
            .ok_or_else(|| format!("no user called {n}")),
        None => {
            let mut all = state.users.list();
            match (all.next(), all.next()) {
                (Some(u), None) => Ok(u.id.clone()),
                (None, _) => Err("there is no user yet: create one with `user create`".into()),
                _ => Err("several users exist: pass --user NAME".into()),
            }
        }
    }
}

/// `user create`: add a user (and optionally a web UI password).
pub fn create_user(
    state: &Path,
    name: &str,
    admin: bool,
    password: Option<&str>,
) -> Result<(), String> {
    if !state.exists() {
        return Err(format!(
            "{} does not exist; start the server once with --state to create it",
            state.display()
        ));
    }
    let _lock = Lock::acquire(state)?;
    let mut st = crate::api::AppState::open(Store::new(state))?;
    st.add_user(name, admin, password)?;
    st.persist()
        .map_err(|e| format!("cannot save the state: {e}"))
}

/// `user passwd`: set the web UI password of an existing user.
pub fn set_password(state: &Path, name: &str, password: &str) -> Result<(), String> {
    let _lock = Lock::acquire(state)?;
    let mut st = crate::api::AppState::open(Store::new(state))?;
    let id = pick_user(&st, Some(name))?;
    st.users.set_password(&id, password)?;
    st.persist()
        .map_err(|e| format!("cannot save the state: {e}"))
}
