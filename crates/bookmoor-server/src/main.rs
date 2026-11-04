//! bookmoor server entry point.
//!
//! Usage: `bookmoor-server [--bind ADDR] [--insecure-http] [--state FILE] [--dynacat-file FILE]`
//! and `bookmoor-server token create ...` (see `--help` of that command).
//! With `--state` (or `BOOKMOOR_STATE`) everything survives restarts.
//! The first admin token is read from `BOOKMOOR_ADMIN_TOKEN`
//! (`bkm_<id>_<secret>`) or generated and printed once.

use bookmoor_server::api::{router, AppState};
use bookmoor_server::auth::Scope;
use bookmoor_server::store::{Lock, Store};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// `bookmoor-server token create --name N --scope S [--state FILE] [--yes] [--allow-admin]`
fn token_command(mut args: impl Iterator<Item = String>) {
    use bookmoor_server::cli::{confirm_on_terminal, create_token, TokenRequest};
    const USAGE: &str = "usage: bookmoor-server token create --name NAME --scope read|sync|admin [--user USER] --state FILE [--yes] [--allow-admin]\n\nCreates a token without a running server; refuses while the state is in use.";
    match args.next().as_deref() {
        Some("create") => {}
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return;
        }
        _ => fail(USAGE),
    }
    let mut state = std::env::var_os("BOOKMOOR_STATE").map(std::path::PathBuf::from);
    let (mut name, mut scope, mut user) = (None, None, None);
    let (mut yes, mut allow_admin) = (false, false);
    while let Some(a) = args.next() {
        let mut value = |what: &str| {
            args.next()
                .unwrap_or_else(|| fail(&format!("{what} needs a value")))
        };
        match a.as_str() {
            "--name" => name = Some(value("--name")),
            "--user" => user = Some(value("--user")),
            "--scope" => {
                scope = Some(
                    value("--scope")
                        .parse::<Scope>()
                        .unwrap_or_else(|e| fail(&e)),
                )
            }
            "--state" => state = Some(value("--state").into()),
            "--help" | "-h" => {
                println!("{USAGE}");
                return;
            }
            "--yes" => yes = true,
            "--allow-admin" => allow_admin = true,
            other => fail(&format!("unknown argument {other}")),
        }
    }
    let name = name.unwrap_or_else(|| fail("--name is required"));
    let scope = scope.unwrap_or_else(|| fail("--scope is required"));
    let state = state.unwrap_or_else(|| fail("--state FILE (or BOOKMOOR_STATE) is required"));
    let req = TokenRequest {
        state: &state,
        name: &name,
        scope,
        user: user.as_deref(),
        allow_admin,
        yes,
    };
    match create_token(&req, confirm_on_terminal(name.trim())) {
        Ok(token) => println!("{token}"),
        Err(e) => fail(&e),
    }
}

/// `bookmoor-server user create|passwd ...`; the password is the first line
/// of stdin (or `BOOKMOOR_PASSWORD`), so it never shows up in a process list.
fn user_command(mut args: impl Iterator<Item = String>) {
    const USAGE: &str =
        "usage: bookmoor-server user create --name NAME [--admin] [--no-password] --state FILE
       bookmoor-server user passwd --name NAME --state FILE

The password is read from the first line of stdin (or BOOKMOOR_PASSWORD).
Refuses while the state is in use: stop the server first.";
    let sub = args.next();
    if matches!(sub.as_deref(), Some("--help" | "-h")) {
        println!("{USAGE}");
        return;
    }
    let mut state = std::env::var_os("BOOKMOOR_STATE").map(std::path::PathBuf::from);
    let (mut name, mut admin, mut no_password) = (None, false, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.next(),
            "--state" => state = args.next().map(Into::into),
            "--admin" => admin = true,
            "--no-password" => no_password = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                return;
            }
            other => fail(&format!("unknown argument {other}\n{USAGE}")),
        }
    }
    if no_password && sub.as_deref() != Some("create") {
        fail("--no-password only makes sense for user create");
    }
    let name = name.unwrap_or_else(|| fail("--name is required"));
    let state = state.unwrap_or_else(|| fail("--state FILE (or BOOKMOOR_STATE) is required"));
    let password = || {
        std::env::var_os("BOOKMOOR_PASSWORD").map_or_else(
            || {
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .unwrap_or_else(|e| fail(&format!("cannot read the password: {e}")));
                line.trim_end_matches(['\r', '\n']).to_string()
            },
            |v| {
                v.into_string()
                    .unwrap_or_else(|_| fail("BOOKMOOR_PASSWORD is not valid UTF-8"))
            },
        )
    };
    let res = match sub.as_deref() {
        Some("create") if no_password => {
            bookmoor_server::cli::create_user(&state, &name, admin, None)
        }
        Some("create") => {
            bookmoor_server::cli::create_user(&state, &name, admin, Some(&password()))
        }
        Some("passwd") => bookmoor_server::cli::set_password(&state, &name, &password()),
        _ => fail(USAGE),
    };
    match res {
        Ok(()) => println!("ok"),
        Err(e) => fail(&e),
    }
}

#[tokio::main]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("token") => {
            token_command(std::env::args().skip(2));
            return;
        }
        Some("user") => {
            user_command(std::env::args().skip(2));
            return;
        }
        _ => {}
    }
    let mut bind: SocketAddr = "127.0.0.1:8787".parse().unwrap();
    let mut insecure = false;
    let mut state_path = std::env::var_os("BOOKMOOR_STATE").map(std::path::PathBuf::from);
    let mut dynacat_user = std::env::var("BOOKMOOR_DYNACAT_USER").ok();
    let mut dynacat_file = std::env::var_os("BOOKMOOR_DYNACAT_FILE").map(std::path::PathBuf::from);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => {
                bind = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| fail("--bind needs an address like 127.0.0.1:8787"));
            }
            "--insecure-http" => insecure = true,
            "--state" => {
                state_path = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--state needs a file path"))
                        .into(),
                );
            }
            "--dynacat-user" => {
                dynacat_user = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--dynacat-user needs a user name")),
                );
            }
            "--dynacat-file" => {
                dynacat_file = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--dynacat-file needs a file path"))
                        .into(),
                );
            }
            "--help" | "-h" => {
                println!("usage: bookmoor-server [--bind ADDR] [--insecure-http] [--state FILE] [--dynacat-file FILE] [--dynacat-user USER]\n       bookmoor-server token create --help\n       bookmoor-server user create --help");
                return;
            }
            other => fail(&format!("unknown argument {other}")),
        }
    }
    // The bearer token would otherwise travel in clear text.
    if !bind.ip().is_loopback() && !insecure {
        fail("refusing to bind a non-loopback address without TLS in front; use a reverse proxy or pass --insecure-http");
    }

    // Held for the life of the process; dropped on a clean shutdown.
    // Held until the process ends; the OS releases it however that happens.
    let _lock = state_path
        .as_deref()
        .map(|p| Lock::acquire(p).unwrap_or_else(|e| fail(&e)));
    let mut state = match state_path {
        Some(p) => AppState::open(Store::new(p)).unwrap_or_else(|e| fail(&e)),
        None => {
            eprintln!("warning: no --state file, everything is lost on restart");
            AppState::default()
        }
    };
    match std::env::var("BOOKMOOR_ADMIN_TOKEN") {
        Ok(t) => {
            // Rotating the variable replaces the old admin token.
            state.tokens.remove_named("admin", Scope::Admin);
            if state
                .tokens
                .insert_known("admin", Scope::Admin, &t)
                .is_none()
            {
                fail("BOOKMOOR_ADMIN_TOKEN must look like bkm_<id>_<secret> (secret at least 32 characters)");
            }
        }
        // A state file that already holds an admin token needs no new one.
        Err(_) if !state.tokens.has_admin() => {
            let (t, _) = state.tokens.create("admin", Scope::Admin, None, None);
            println!("admin token (shown once): {t}");
        }
        Err(_) => {}
    }
    if let Err(e) = state.persist() {
        fail(&format!("cannot write the state file: {e}"));
    }

    state.dynacat_file = dynacat_file;
    state.dynacat_user = dynacat_user;
    if let Err(e) = state.publish_dynacat() {
        fail(&format!("cannot write the Dynacat file: {e}"));
    }

    let app = router(Arc::new(Mutex::new(state)));
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .unwrap_or_else(|e| fail(&format!("cannot bind {bind}: {e}")));
    println!(
        "bookmoor-server {} listening on {bind}",
        env!("CARGO_PKG_VERSION")
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("server error");
}

/// Ctrl-C everywhere, and SIGTERM (systemd, docker stop) on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}
