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
    AppState::open(Store::new(&path))
        .unwrap()
        .persist()
        .unwrap();
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
    // The command leaves no lock behind.
    assert!(!path.with_file_name("state.json.lock").exists());
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
fn refuses_while_the_state_is_in_use_and_without_a_state_file() {
    let path = state_file("locked");
    let lock = path.with_file_name("state.json.lock");
    std::fs::write(&lock, "123").unwrap();
    let p = path.to_str().unwrap();
    let out = run(&[
        "create", "--name", "x", "--scope", "read", "--state", p, "--yes",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("state.json.lock"), "{}", stderr(&out));
    assert!(lock.exists(), "someone else's lock must stay");

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
