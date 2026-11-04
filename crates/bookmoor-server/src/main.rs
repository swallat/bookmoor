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
    const USAGE: &str = "usage: bookmoor-server token create --name NAME --scope read|sync|admin --state FILE [--yes] [--allow-admin]\n\nCreates a token without a running server; refuses while the state is in use.";
    match args.next().as_deref() {
        Some("create") => {}
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return;
        }
        _ => fail(USAGE),
    }
    let mut state = std::env::var_os("BOOKMOOR_STATE").map(std::path::PathBuf::from);
    let (mut name, mut scope) = (None, None);
    let (mut yes, mut allow_admin) = (false, false);
    while let Some(a) = args.next() {
        let mut value = |what: &str| {
            args.next()
                .unwrap_or_else(|| fail(&format!("{what} needs a value")))
        };
        match a.as_str() {
            "--name" => name = Some(value("--name")),
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
        allow_admin,
        yes,
    };
    match create_token(&req, confirm_on_terminal(name.trim())) {
        Ok(token) => println!("{token}"),
        Err(e) => fail(&e),
    }
}

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("token") {
        token_command(std::env::args().skip(2));
        return;
    }
    let mut bind: SocketAddr = "127.0.0.1:8787".parse().unwrap();
    let mut insecure = false;
    let mut state_path = std::env::var_os("BOOKMOOR_STATE").map(std::path::PathBuf::from);
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
            "--dynacat-file" => {
                dynacat_file = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--dynacat-file needs a file path"))
                        .into(),
                );
            }
            "--help" | "-h" => {
                println!("usage: bookmoor-server [--bind ADDR] [--insecure-http] [--state FILE] [--dynacat-file FILE]\n       bookmoor-server token create --help");
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
            let (t, _) = state.tokens.create("admin", Scope::Admin, None);
            println!("admin token (shown once): {t}");
        }
        Err(_) => {}
    }
    if let Err(e) = state.persist() {
        fail(&format!("cannot write the state file: {e}"));
    }

    state.dynacat_file = dynacat_file;
    state.publish_dynacat();

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
