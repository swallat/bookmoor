//! bookmoor server entry point.
//!
//! Usage: `bookmoor-server [--bind ADDR] [--insecure-http]`
//! The first admin token is read from `BOOKMOOR_ADMIN_TOKEN`
//! (`bkm_<id>_<secret>`) or generated and printed once.

use bookmoor_server::api::{router, AppState};
use bookmoor_server::auth::Scope;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[tokio::main]
async fn main() {
    let mut bind: SocketAddr = "127.0.0.1:8787".parse().unwrap();
    let mut insecure = false;
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
            "--help" | "-h" => {
                println!("usage: bookmoor-server [--bind ADDR] [--insecure-http]");
                return;
            }
            other => fail(&format!("unknown argument {other}")),
        }
    }
    // The bearer token would otherwise travel in clear text.
    if !bind.ip().is_loopback() && !insecure {
        fail("refusing to bind a non-loopback address without TLS in front; use a reverse proxy or pass --insecure-http");
    }

    let mut state = AppState::default();
    match std::env::var("BOOKMOOR_ADMIN_TOKEN") {
        Ok(t) => {
            if state
                .tokens
                .insert_known("admin", Scope::Admin, &t)
                .is_none()
            {
                fail("BOOKMOOR_ADMIN_TOKEN must look like bkm_<id>_<secret> (secret at least 32 characters)");
            }
        }
        Err(_) => {
            let (t, _) = state.tokens.create("admin", Scope::Admin, None);
            println!("admin token (shown once): {t}");
        }
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
