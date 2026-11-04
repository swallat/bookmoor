//! On-disk persistence: one JSON snapshot of the engine and the tokens,
//! replaced atomically (write a temp file, fsync, rename), readable by the
//! owner only because it holds token hashes.

use crate::api::AppState;
use crate::auth::{Scope, TokenRecord, Tokens};
use crate::engine::{Engine, Snapshot};
use crate::users::{User, Users};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct SaveFile<'a> {
    users: &'a [User],
    /// Bookmark tree per user id.
    engines: BTreeMap<&'a str, Snapshot>,
    tokens: &'a [TokenRecord],
}

#[derive(Deserialize)]
struct LoadFile {
    #[serde(default)]
    users: Vec<User>,
    #[serde(default)]
    engines: BTreeMap<String, Snapshot>,
    /// Single-user state of earlier versions; becomes the user `owner`.
    engine: Option<Snapshot>,
    tokens: Vec<TokenRecord>,
}

/// Everything the state file holds.
#[derive(Debug, Default)]
pub struct Loaded {
    pub users: Users,
    pub engines: BTreeMap<String, Engine>,
    pub tokens: Tokens,
}

/// An exclusive OS lock on `<state>.lock`, so the server and the token
/// command never write the state at the same time. The kernel drops it when
/// the process ends, however it ends; the empty file may stay behind.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

impl Lock {
    pub fn acquire(state: &Path) -> Result<Lock, String> {
        let mut name = state.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let path = state.with_file_name(name);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Lock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(format!(
                "{} is in use: another bookmoor-server or token command has this state open",
                path.display()
            )),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(format!("cannot lock {}: {e}", path.display()))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Store { path: path.into() }
    }

    /// `Ok(None)` when no state has been saved yet.
    pub fn load(&self) -> Result<Option<Loaded>, String> {
        let text = match fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", self.path.display())),
        };
        let f: LoadFile = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not a valid state file: {e}", self.path.display()))?;
        let mut users = f.users;
        let mut engines = f.engines;
        let mut tokens = f.tokens;
        if let Some(legacy) = f.engine {
            if !users.is_empty() || !engines.is_empty() {
                return Err("state file mixes the single-user and multi-user layout".into());
            }
            // Earlier versions had one tree for everyone: keep it, owned by
            // a user without a password (set one with `user passwd`).
            let owner = User {
                id: "u_owner".into(),
                name: "owner".into(),
                admin: true,
                password_hash: None,
            };
            engines.insert(owner.id.clone(), legacy);
            for t in tokens.iter_mut().filter(|t| t.scope != Scope::Admin) {
                t.user_id = Some(owner.id.clone());
            }
            users.push(owner);
        }
        let users = Users::from_records(users)?;
        let mut restored = BTreeMap::new();
        for (uid, snap) in engines {
            if users.get(&uid).is_none() {
                return Err(format!("tree of unknown user {uid}"));
            }
            restored.insert(uid, Engine::restore(snap)?);
        }
        if let Some(u) = users.list().find(|u| !restored.contains_key(&u.id)) {
            return Err(format!("user {} has no tree", u.name));
        }
        for t in &tokens {
            match (&t.user_id, t.scope) {
                (None, Scope::Admin) => {}
                (Some(uid), s) if s != Scope::Admin && users.get(uid).is_some() => {}
                _ => return Err(format!("token {} does not match any user", t.id)),
            }
        }
        Ok(Some(Loaded {
            users,
            engines: restored,
            tokens: Tokens::from_records(tokens),
        }))
    }

    pub fn save(
        &self,
        users: &Users,
        engines: &BTreeMap<String, Engine>,
        tokens: &Tokens,
    ) -> io::Result<()> {
        let records = tokens.records();
        let user_records = users.records();
        let bytes = serde_json::to_vec(&SaveFile {
            users: &user_records,
            engines: engines
                .iter()
                .map(|(k, e)| (k.as_str(), e.snapshot()))
                .collect(),
            tokens: &records,
        })
        .map_err(io::Error::other)?;
        let tmp = tmp_path(&self.path);
        // A leftover temp file could carry other permissions or owners.
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, &self.path)?;
        // Make the rename itself durable.
        if let Some(dir) = self.path.parent() {
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }
}

fn tmp_path(p: &Path) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    p.with_file_name(name)
}

impl AppState {
    /// Open `store`, loading earlier state if there is any.
    pub fn open(store: Store) -> Result<AppState, String> {
        let l = store.load()?.unwrap_or_default();
        Ok(AppState {
            users: l.users,
            engines: l.engines,
            tokens: l.tokens,
            store: Some(store),
            ..AppState::default()
        })
    }

    /// Write the current state; a no-op without a store.
    pub fn persist(&self) -> io::Result<()> {
        match &self.store {
            Some(s) => s.save(&self.users, &self.engines, &self.tokens),
            None => Ok(()),
        }
    }

    /// Like [`persist`](Self::persist), but if saving fails the in-memory
    /// state is replaced by what is on disk, so no client ever sees a change
    /// the server would forget after a restart. If even that is impossible
    /// the process exits rather than serve state it cannot vouch for.
    pub fn persist_or_revert(&mut self) -> io::Result<()> {
        let err = match self.persist() {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if let Some(store) = &self.store {
            match store.load() {
                Ok(Some(l)) => {
                    self.users = l.users;
                    self.engines = l.engines;
                    self.tokens = l.tokens;
                }
                _ => {
                    eprintln!("fatal: cannot save or reload state ({err}); exiting");
                    std::process::exit(1);
                }
            }
        }
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Scope;
    use bookmoor_core::model::Kind;
    use bookmoor_core::wire::{Op, OpBody, SyncRequest};

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bookmoor-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("state.json")
    }

    fn add(n: u32) -> Op {
        Op {
            op_id: format!("a-{n}"),
            body: OpBody::Add {
                id: format!("n{n}"),
                parent: "toolbar".into(),
                position: "V".into(),
                kind: Kind::Bookmark,
                title: format!("t{n}"),
                url: Some("https://example.org/".into()),
            },
        }
    }

    #[test]
    fn missing_file_is_a_fresh_start() {
        assert!(Store::new(tmp("missing")).load().unwrap().is_none());
    }

    #[test]
    fn state_survives_a_restart_including_idempotency() {
        let path = tmp("roundtrip");
        let store = Store::new(&path);
        let mut st = AppState::open(store.clone()).unwrap();
        let uid = st
            .add_user("alice", false, Some("long enough pw"))
            .unwrap()
            .id;
        let engine = st.engines.get_mut(&uid).unwrap();
        engine.register_endpoint("ep_a");
        engine.register_endpoint("ep_b");
        let (token, _) = st
            .tokens
            .create("a", Scope::Sync, Some(uid.clone()), Some("ep_a".into()));
        let req = SyncRequest {
            endpoint_id: "ep_a".into(),
            since_rev: 0,
            ops: vec![add(1), add(2)],
        };
        let first = st.engines.get_mut(&uid).unwrap().sync(&req).unwrap();
        st.persist().unwrap();

        let mut again = AppState::open(store).unwrap();
        assert_eq!(again.engines[&uid].tree(), st.engines[&uid].tree());
        assert!(again.tokens.authenticate(&token).is_some());
        // A retried request must not apply twice.
        let replay = again.engines.get_mut(&uid).unwrap().sync(&req).unwrap();
        assert_eq!(replay.rev, first.rev);
        assert_eq!(again.engines[&uid].tree(), st.engines[&uid].tree());
        // The other device still receives what it missed.
        let b = again
            .engines
            .get_mut(&uid)
            .unwrap()
            .sync(&SyncRequest {
                endpoint_id: "ep_b".into(),
                since_rev: 0,
                ops: vec![],
            })
            .unwrap();
        assert!(!b.resync);
        assert_eq!(b.ops.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("mode");
        AppState::open(Store::new(&path))
            .unwrap()
            .persist()
            .unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn damaged_state_is_refused_not_replaced() {
        let path = tmp("damaged");
        fs::write(&path, "{ not json").unwrap();
        assert!(Store::new(&path).load().is_err());
        // A well-formed file without the well-known folders is refused too.
        let mut snap = serde_json::to_value(Engine::new().snapshot()).unwrap();
        snap["nodes"] = serde_json::json!([]);
        let file = serde_json::json!({ "engine": snap, "tokens": [] });
        fs::write(&path, file.to_string()).unwrap();
        let err = Store::new(&path).load().err().unwrap();
        assert!(err.contains("well-known"), "{err}");
    }

    #[test]
    fn failed_save_rolls_back_memory() {
        let path = tmp("rollback");
        let mut st = AppState::open(Store::new(&path)).unwrap();
        let uid = st.add_user("alice", false, None).unwrap().id;
        st.engines.get_mut(&uid).unwrap().register_endpoint("ep_a");
        st.persist().unwrap();
        let saved = st.engines[&uid].tree();
        st.engines
            .get_mut(&uid)
            .unwrap()
            .sync(&SyncRequest {
                endpoint_id: "ep_a".into(),
                since_rev: 0,
                ops: vec![add(1)],
            })
            .unwrap();
        assert_ne!(st.engines[&uid].tree(), saved);
        // A directory in the way of the temp file makes the save fail.
        fs::create_dir(tmp_path(&path)).unwrap();
        assert!(st.persist_or_revert().is_err());
        assert_eq!(st.engines[&uid].tree(), saved);
    }

    #[test]
    fn cyclic_or_detached_state_is_refused() {
        let path = tmp("cycle");
        let mut engine = Engine::new();
        engine.register_endpoint("ep_a");
        let ops = vec![add(1), add(2)];
        engine
            .sync(&SyncRequest {
                endpoint_id: "ep_a".into(),
                since_rev: 0,
                ops,
            })
            .unwrap();
        let mut snap = serde_json::to_value(engine.snapshot()).unwrap();
        for n in snap["nodes"].as_array_mut().unwrap() {
            match n["id"].as_str().unwrap() {
                "n1" => n["parent"] = "n2".into(),
                "n2" => n["parent"] = "n1".into(),
                _ => {}
            }
        }
        let file = serde_json::json!({ "engine": snap, "tokens": [] });
        fs::write(&path, file.to_string()).unwrap();
        let err = Store::new(&path).load().err().unwrap();
        assert!(err.contains("not attached"), "{err}");
    }

    #[test]
    fn single_user_state_becomes_the_owner() {
        let path = tmp("legacy");
        let mut engine = Engine::new();
        engine.register_endpoint("ep_a");
        engine
            .sync(&SyncRequest {
                endpoint_id: "ep_a".into(),
                since_rev: 0,
                ops: vec![add(1)],
            })
            .unwrap();
        let mut tokens = Tokens::new();
        let (sync, _) = tokens.create("laptop", Scope::Sync, None, Some("ep_a".into()));
        let (admin, _) = tokens.create("admin", Scope::Admin, None, None);
        let file = serde_json::json!({
            "engine": engine.snapshot(),
            "tokens": tokens.records(),
        });
        fs::write(&path, file.to_string()).unwrap();
        let st = AppState::open(Store::new(&path)).unwrap();
        let owner = st.users.by_name("owner").unwrap();
        assert!(owner.admin && owner.password_hash.is_none());
        assert_eq!(st.engines[&owner.id].tree(), engine.tree());
        let rec = st.tokens.authenticate(&sync).unwrap();
        assert_eq!(rec.user_id.as_deref(), Some(owner.id.as_str()));
        assert!(st.tokens.authenticate(&admin).unwrap().user_id.is_none());
        // Saved again, it is the multi-user layout and loads to the same.
        st.persist().unwrap();
        let again = AppState::open(Store::new(&path)).unwrap();
        assert_eq!(again.users.by_name("owner").unwrap().id, owner.id);
    }

    #[test]
    fn trees_of_users_are_separate() {
        let mut st = AppState::default();
        let a = st.add_user("alice", false, None).unwrap().id;
        let b = st.add_user("bob", false, None).unwrap().id;
        st.engines.get_mut(&a).unwrap().register_endpoint("ep_a");
        st.engines
            .get_mut(&a)
            .unwrap()
            .sync(&SyncRequest {
                endpoint_id: "ep_a".into(),
                since_rev: 0,
                ops: vec![add(1)],
            })
            .unwrap();
        assert_eq!(st.engines[&a].rev(), 1);
        assert_eq!(st.engines[&b].rev(), 0);
        assert!(st.remove_user(&a));
        assert!(!st.engines.contains_key(&a));
    }

    #[test]
    fn tokens_must_belong_to_a_user() {
        let path = tmp("orphan");
        let mut tokens = Tokens::new();
        tokens.create("x", Scope::Read, Some("u_gone".into()), None);
        let file = serde_json::json!({
            "engine": Engine::new().snapshot(),
            "tokens": tokens.records(),
        });
        fs::write(&path, file.to_string()).unwrap();
        // Legacy files re-home non-admin tokens, so build the new layout.
        let st = AppState::open(Store::new(&path)).unwrap();
        st.persist().unwrap();
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        v["tokens"][0]["user_id"] = "u_gone".into();
        fs::write(&path, v.to_string()).unwrap();
        let err = Store::new(&path).load().err().unwrap();
        assert!(err.contains("does not match any user"), "{err}");
    }
}
