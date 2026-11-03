//! bookmoor server entry point.
//!
//! Usage: `bookmoor-server [--bind ADDR] [--insecure-http] [--state FILE]`
//! With `--state` (or `BOOKMOOR_STATE`) everything survives restarts.
//! The first admin token is read from `BOOKMOOR_ADMIN_TOKEN`
//! (`bkm_<id>_<secret>`) or generated and printed once.

use bookmoor_server::api::{router, AppState};
use bookmoor_server::auth::Scope;
use bookmoor_server::store::Store;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[tokio::main]
async fn main() {
    let mut bind: SocketAddr = "127.0.0.1:8787".parse().unwrap();
    let mut insecure = false;
    let mut state_path = std::env::var_os("BOOKMOOR_STATE").map(std::path::PathBuf::from);
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
            "--help" | "-h" => {
                println!("usage: bookmoor-server [--bind ADDR] [--insecure-http] [--state FILE]");
                return;
            }
            other => fail(&format!("unknown argument {other}")),
        }
    }
    // The bearer token would otherwise travel in clear text.
    if !bind.ip().is_loopback() && !insecure {
        fail("refusing to bind a non-loopback address without TLS in front; use a reverse proxy or pass --insecure-http");
    }

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

    let app = router(Arc::new(Mutex::new(state)));
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .unwrap_or_else(|e| fail(&format!("cannot bind {bind}: {e}")));
    println!(
        "bookmoor-server {} listening on {bind}",
        env!("CARGO_PKG_VERSION")
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server error");
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}
