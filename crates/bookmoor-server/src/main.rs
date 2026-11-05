//! bookmoor server entry point.
//!
//! Usage: `bookmoor-server [--bind ADDR] [--insecure-http] [--state FILE] [--dynacat-output SPEC]...`
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
    const USAGE: &str = "usage: bookmoor-server token create --name NAME --scope read|sync|admin [--user USER] [--profile PROFILE] --state FILE [--yes] [--allow-admin]\n\nCreates a token without a running server; refuses while the state is in use.";
    match args.next().as_deref() {
        Some("create") => {}
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return;
        }
        _ => fail(USAGE),
    }
    let mut state = std::env::var_os("BOOKMOOR_STATE").map(std::path::PathBuf::from);
    let (mut name, mut scope, mut user, mut profile) = (None, None, None, None);
    let (mut yes, mut allow_admin) = (false, false);
    while let Some(a) = args.next() {
        let mut value = |what: &str| {
            args.next()
                .unwrap_or_else(|| fail(&format!("{what} needs a value")))
        };
        match a.as_str() {
            "--name" => name = Some(value("--name")),
            "--user" => user = Some(value("--user")),
            "--profile" => profile = Some(value("--profile")),
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
        profile: profile.as_deref(),
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
    let mut dynacat_profile = std::env::var("BOOKMOOR_DYNACAT_PROFILE").ok();
    let mut client_cert = std::env::var("BOOKMOOR_CLIENT_CERT_HEADER")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let env_flag =
        |name: &str| std::env::var(name).is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
    let mut oidc_issuer = std::env::var("BOOKMOOR_OIDC_ISSUER")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let mut oidc_client_id = std::env::var("BOOKMOOR_OIDC_CLIENT_ID").ok();
    // The secret is only read from the environment: a flag would show in a process list.
    let oidc_client_secret = std::env::var("BOOKMOOR_OIDC_CLIENT_SECRET")
        .ok()
        .filter(|v| !v.is_empty());
    let mut public_url = std::env::var("BOOKMOOR_PUBLIC_URL").ok();
    let mut oidc_label = std::env::var("BOOKMOOR_OIDC_LABEL").ok();
    let mut oidc_create_users = env_flag("BOOKMOOR_OIDC_CREATE_USERS");
    // `BOOKMOOR_DYNACAT_FILE` is a plain path; `BOOKMOOR_DYNACAT_OUTPUTS` holds
    // one spec per line (see `Output::parse`).
    let mut dynacat_outputs: Vec<bookmoor_server::dynacat::Output> = Vec::new();
    if let Some(f) = std::env::var_os("BOOKMOOR_DYNACAT_FILE").filter(|f| !f.is_empty()) {
        dynacat_outputs.push(bookmoor_server::dynacat::Output {
            file: f.into(),
            ..Default::default()
        });
    }
    for spec in std::env::var("BOOKMOOR_DYNACAT_OUTPUTS")
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        dynacat_outputs
            .push(bookmoor_server::dynacat::Output::parse(spec).unwrap_or_else(|e| fail(&e)));
    }
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
            "--oidc-issuer" => {
                oidc_issuer = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--oidc-issuer needs a URL")),
                );
            }
            "--oidc-client-id" => {
                oidc_client_id = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--oidc-client-id needs a value")),
                );
            }
            "--public-url" => {
                public_url = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--public-url needs a URL")),
                );
            }
            "--oidc-label" => {
                oidc_label = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--oidc-label needs a text")),
                );
            }
            "--oidc-create-users" => oidc_create_users = true,
            "--client-cert-header" => {
                client_cert = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--client-cert-header needs NAME[=VALUE]")),
                );
            }
            "--dynacat-profile" => {
                dynacat_profile = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--dynacat-profile needs a profile name")),
                );
            }
            "--dynacat-user" => {
                dynacat_user = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--dynacat-user needs a user name")),
                );
            }
            "--dynacat-file" => {
                let file = args
                    .next()
                    .unwrap_or_else(|| fail("--dynacat-file needs a file path"));
                dynacat_outputs.push(bookmoor_server::dynacat::Output {
                    file: file.into(),
                    ..Default::default()
                });
            }
            "--dynacat-output" => {
                let spec = args
                    .next()
                    .unwrap_or_else(|| fail("--dynacat-output needs a spec"));
                dynacat_outputs.push(
                    bookmoor_server::dynacat::Output::parse(&spec).unwrap_or_else(|e| fail(&e)),
                );
            }
            "--help" | "-h" => {
                println!("usage: bookmoor-server [--bind ADDR] [--insecure-http] [--state FILE] [--dynacat-output SPEC]... [--dynacat-user USER] [--dynacat-profile PROFILE] [--client-cert-header NAME[=VALUE|=*]] [--public-url URL --oidc-issuer URL --oidc-client-id ID [--oidc-label TEXT] [--oidc-create-users]]\n       bookmoor-server token create --help\n       bookmoor-server user create --help");
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
            let (t, _) = state.tokens.create("admin", Scope::Admin, None, None, None);
            println!("admin token (shown once): {t}");
        }
        Err(_) => {}
    }
    if let Err(e) = state.persist() {
        fail(&format!("cannot write the state file: {e}"));
    }

    state.secure_cookies = !insecure;
    for (i, o) in dynacat_outputs.iter().enumerate() {
        if dynacat_outputs[..i].iter().any(|p| p.file == o.file) {
            fail(&format!(
                "two Dynacat outputs write {}; give each its own file",
                o.file.display()
            ));
        }
    }
    state.client_cert = client_cert.map(|spec| {
        bookmoor_server::mtls::ClientCertCheck::parse(&spec)
            .unwrap_or_else(|e| fail(&format!("--client-cert-header: {e}")))
    });
    if let Some(issuer) = oidc_issuer {
        let client_id = oidc_client_id
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| fail("single sign-on needs --oidc-client-id"));
        let public_url = public_url
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| {
                fail("single sign-on needs --public-url (the address users reach the web UI at)")
            });
        let oidc = bookmoor_server::oidc::Oidc::new(bookmoor_server::oidc::OidcConfig {
            issuer,
            client_id,
            client_secret: oidc_client_secret,
            public_url,
            label: oidc_label.unwrap_or_else(|| "single sign-on".into()),
            create_users: oidc_create_users,
        })
        .unwrap_or_else(|e| fail(&e));
        println!(
            "single sign-on: redirect URL {}",
            bookmoor_server::oidc::callback_url(&oidc.cfg.public_url)
        );
        state.oidc = Some(Arc::new(oidc));
        state.oidc_env = true;
    } else {
        // Settings saved in the web UI.
        let saved = state.oidc_config.take();
        state.apply_oidc(saved);
        if let Some(o) = &state.oidc {
            println!(
                "single sign-on (web UI settings): redirect URL {}",
                bookmoor_server::oidc::callback_url(&o.cfg.public_url)
            );
        }
    }
    state.dynacat_outputs = dynacat_outputs;
    state.dynacat_user = dynacat_user;
    state.dynacat_profile = dynacat_profile;
    if let Err(e) = state.publish_dynacat(None) {
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
