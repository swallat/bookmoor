use bookmoor_server::api::AppState;
use bookmoor_server::auth::Scope;
use bookmoor_server::store::Store;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn state_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bookmoor-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.json");
    let mut st = AppState::open(Store::new(&path)).unwrap();
    st.add_user("tester", false, None).unwrap();
    st.persist().unwrap();
    path
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bookmoor-server"))
        .arg("token")
        .args(args)
        .env_remove("BOOKMOOR_STATE")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn creates_a_token_that_the_state_then_accepts() {
    let path = state_file("ok");
    let p = path.to_str().unwrap();
    let out = run(&[
        "create", "--name", "laptop", "--scope", "sync", "--state", p, "--yes",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let state = AppState::open(Store::new(&path)).unwrap();
    let rec = state.tokens.authenticate(&token).expect("token is valid");
    assert_eq!(rec.scope, Scope::Sync);
    assert!(rec.endpoint_id.is_some());
}

#[test]
fn refuses_without_a_terminal_or_yes() {
    let path = state_file("noyes");
    let out = run(&[
        "create",
        "--name",
        "x",
        "--scope",
        "read",
        "--state",
        path.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    let state = AppState::open(Store::new(&path)).unwrap();
    assert!(!state.tokens.has_admin());
}

#[test]
fn admin_needs_an_extra_flag() {
    let path = state_file("admin");
    let p = path.to_str().unwrap();
    let out = run(&[
        "create", "--name", "a", "--scope", "admin", "--state", p, "--yes",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--allow-admin"));
    let out = run(&[
        "create",
        "--name",
        "a",
        "--scope",
        "admin",
        "--state",
        p,
        "--yes",
        "--allow-admin",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn refuses_while_a_server_runs_and_works_after_it_was_killed() {
    let path = state_file("locked");
    let p = path.to_str().unwrap();
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_bookmoor-server"))
            .args(["--bind", "127.0.0.1:0", "--state", p])
            .env_remove("BOOKMOOR_ADMIN_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut server = spawn();
    // The server holds the lock once it has started, so ask until refused.
    // If the command's own brief lock made the server's start fail, the
    // server exits; start it again.
    let mut refused = false;
    for _ in 0..200 {
        if server.try_wait().unwrap().is_some() {
            server = spawn();
        }
        let out = run(&[
            "create", "--name", "x", "--scope", "read", "--state", p, "--yes",
        ]);
        if !out.status.success() && stderr(&out).contains("in use") {
            refused = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(refused, "the running server must block the command");
    // A hard kill leaves the lock file behind but not the lock.
    server.kill().unwrap();
    server.wait().unwrap();
    let out = run(&[
        "create", "--name", "x", "--scope", "read", "--state", p, "--yes",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn needs_an_existing_state_file() {
    let path = state_file("missing");
    let missing = path.with_file_name("nope.json");
    let out = run(&[
        "create",
        "--name",
        "x",
        "--scope",
        "read",
        "--state",
        missing.to_str().unwrap(),
        "--yes",
    ]);
    assert!(!out.status.success());
    assert!(!missing.exists(), "must not create a state file");
}

#[test]
fn rejects_bad_scope_and_names() {
    let path = state_file("bad");
    let p = path.to_str().unwrap();
    let out = run(&[
        "create", "--name", "x", "--scope", "root", "--state", p, "--yes",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("unknown scope"));
    let long = "é".repeat(65);
    for name in ["", "   ", long.as_str()] {
        let out = run(&[
            "create", "--name", name, "--scope", "read", "--state", p, "--yes",
        ]);
        assert!(!out.status.success(), "name {name:?}");
        assert!(stderr(&out).contains("1 to 64"), "{}", stderr(&out));
    }
    let out = run(&["create", "--help"]);
    assert!(out.status.success());
}

#[test]
fn creates_users_and_tokens_for_them() {
    let path = state_file("users");
    let p = path.to_str().unwrap();
    let user = |args: &[&str], pw: &str| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_bookmoor-server"));
        c.arg("user").args(args).env_remove("BOOKMOOR_STATE");
        c.env("BOOKMOOR_PASSWORD", pw).stdin(Stdio::null());
        c.output().unwrap()
    };
    let out = user(
        &["create", "--name", "anna", "--admin", "--state", p],
        "long enough pw",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    // Too short a password is refused.
    assert!(!user(&["create", "--name", "bob", "--state", p], "short")
        .status
        .success());
    // With two users a token must name its owner.
    assert!(
        !run(&["create", "--name", "x", "--scope", "read", "--state", p, "--yes"])
            .status
            .success()
    );
    let out = run(&[
        "create", "--name", "x", "--scope", "read", "--user", "ANNA", "--state", p, "--yes",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let st = AppState::open(Store::new(&path)).unwrap();
    let anna = st.users.by_name("anna").unwrap();
    assert!(anna.admin && anna.password_hash.is_some());
    let token = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        st.tokens
            .authenticate(token.trim())
            .unwrap()
            .user_id
            .as_deref(),
        Some(anna.id.as_str())
    );
    assert!(user(
        &["passwd", "--name", "anna", "--state", p],
        "another long pw"
    )
    .status
    .success());
}
