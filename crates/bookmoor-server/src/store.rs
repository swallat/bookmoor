//! On-disk persistence: one JSON snapshot of the engine and the tokens,
//! replaced atomically (write a temp file, fsync, rename), readable by the
//! owner only because it holds token hashes.

use crate::api::{AppState, Backup};
use crate::auth::{Scope, TokenRecord, Tokens};
use crate::dynacat::UiOutput;
use crate::engine::{Engine, Snapshot};
use crate::profiles::{default_id_for, Profile, Profiles, DEFAULT_NAME};
use crate::users::{User, Users};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct SaveFile<'a> {
    users: &'a [User],
    profiles: &'a [Profile],
    /// Bookmark tree per profile id.
    engines: BTreeMap<&'a str, Snapshot>,
    tokens: &'a [TokenRecord],
    backups: &'a BTreeMap<String, Backup>,
    dynacat: &'a [UiOutput],
    /// Single sign-on set up in the web UI (holds the client secret).
    #[serde(skip_serializing_if = "Option::is_none")]
    oidc: Option<&'a crate::oidc::OidcConfig>,
}

#[derive(Deserialize)]
struct LoadFile {
    #[serde(default)]
    users: Vec<User>,
    /// Absent in files from before profiles: then `engines` is keyed by user.
    #[serde(default)]
    profiles: Vec<Profile>,
    #[serde(default)]
    engines: BTreeMap<String, Snapshot>,
    /// Single-user state of earlier versions; becomes the user `owner`.
    engine: Option<Snapshot>,
    tokens: Vec<TokenRecord>,
    #[serde(default)]
    backups: BTreeMap<String, Backup>,
    #[serde(default)]
    dynacat: Vec<UiOutput>,
    oidc: Option<crate::oidc::OidcConfig>,
}

/// Everything the state file holds.
#[derive(Debug, Default)]
pub struct Loaded {
    pub users: Users,
    pub profiles: Profiles,
    pub engines: BTreeMap<String, Engine>,
    pub tokens: Tokens,
    pub backups: BTreeMap<String, Backup>,
    pub dynacat: Vec<UiOutput>,
    pub oidc: Option<crate::oidc::OidcConfig>,
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
        let mut profiles = f.profiles;
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
                oidc: None,
                ..User::blank()
            };
            engines.insert(owner.id.clone(), legacy);
            for t in tokens.iter_mut().filter(|t| t.scope != Scope::Admin) {
                t.user_id = Some(owner.id.clone());
            }
            users.push(owner);
        }
        if profiles.is_empty() && !users.is_empty() {
            // Before profiles every user had exactly one tree, keyed by the
            // user id: it becomes that user's profile "Default".
            let mut moved = BTreeMap::new();
            for (uid, snap) in engines {
                let pid = default_id_for(&uid);
                profiles.push(Profile {
                    id: pid.clone(),
                    user_id: uid,
                    name: DEFAULT_NAME.into(),
                });
                moved.insert(pid, snap);
            }
            engines = moved;
            for t in tokens.iter_mut().filter(|t| t.scope != Scope::Admin) {
                if t.profile_id.is_none() {
                    t.profile_id = t.user_id.as_deref().map(default_id_for);
                }
            }
        }
        let users = Users::from_records(users)?;
        let profiles = Profiles::from_records(profiles)?;
        let mut restored = BTreeMap::new();
        for (pid, snap) in engines {
            if profiles.get(&pid).is_none() {
                return Err(format!(
                    "tree of unknown profile {pid} (does the file mix the old and new layout?)"
                ));
            }
            restored.insert(pid, Engine::restore(snap)?);
        }
        for p in profiles.list() {
            if users.get(&p.user_id).is_none() {
                return Err(format!("profile {} belongs to an unknown user", p.name));
            }
            if !restored.contains_key(&p.id) {
                return Err(format!("profile {} has no tree", p.name));
            }
        }
        if let Some(u) = users.list().find(|u| profiles.for_user(&u.id).is_empty()) {
            return Err(format!("user {} has no profile", u.name));
        }
        for t in &tokens {
            let ok = match (&t.user_id, &t.profile_id, t.scope) {
                (None, None, Scope::Admin) => true,
                (Some(u), Some(p), s) if s != Scope::Admin => {
                    users.get(u).is_some() && profiles.get(p).is_some_and(|p| &p.user_id == u)
                }
                _ => false,
            };
            if !ok {
                return Err(format!(
                    "token {} does not match any user and profile",
                    t.id
                ));
            }
        }
        let backups = f
            .backups
            .into_iter()
            .filter(|(p, _)| profiles.get(p).is_some())
            .collect();
        let dynacat: Vec<UiOutput> = f
            .dynacat
            .into_iter()
            .filter(|o| profiles.get(&o.profile_id).is_some())
            .collect();
        Ok(Some(Loaded {
            users,
            profiles,
            engines: restored,
            tokens: Tokens::from_records(tokens),
            backups,
            dynacat,
            oidc: f.oidc,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save(
        &self,
        users: &Users,
        profiles: &Profiles,
        engines: &BTreeMap<String, Engine>,
        tokens: &Tokens,
        backups: &BTreeMap<String, Backup>,
        dynacat: &[UiOutput],
        oidc: Option<&crate::oidc::OidcConfig>,
    ) -> io::Result<()> {
        let records = tokens.records();
        let user_records = users.records();
        let profile_records = profiles.records();
        let bytes = serde_json::to_vec(&SaveFile {
            users: &user_records,
            profiles: &profile_records,
            engines: engines
                .iter()
                .map(|(k, e)| (k.as_str(), e.snapshot()))
                .collect(),
            tokens: &records,
            backups,
            dynacat,
            oidc,
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
            profiles: l.profiles,
            engines: l.engines,
            tokens: l.tokens,
            backups: l.backups,
            dynacat_ui: l.dynacat,
            oidc_config: l.oidc,
            store: Some(store),
            ..AppState::default()
        })
    }

    /// Write the current state; a no-op without a store.
    pub fn persist(&self) -> io::Result<()> {
        match &self.store {
            Some(s) => s.save(
                &self.users,
                &self.profiles,
                &self.engines,
                &self.tokens,
                &self.backups,
                &self.dynacat_ui,
                self.oidc_config.as_ref(),
            ),
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
                    self.profiles = l.profiles;
                    self.engines = l.engines;
                    self.tokens = l.tokens;
                    self.backups = l.backups;
                    self.dynacat_ui = l.dynacat;
                    self.apply_oidc(l.oidc);
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
        let pid = default_id_for(&uid);
        let engine = st.engines.get_mut(&pid).unwrap();
        engine.register_endpoint("ep_a");
        engine.register_endpoint("ep_b");
        let (token, _) = st.tokens.create(
            "a",
            Scope::Sync,
            Some(uid.clone()),
            Some(pid.clone()),
            Some("ep_a".into()),
        );
        let req = SyncRequest {
            endpoint_id: "ep_a".into(),
            since_rev: 0,
            ops: vec![add(1), add(2)],
        };
        let first = st.engines.get_mut(&pid).unwrap().sync(&req).unwrap();
        st.persist().unwrap();

        let mut again = AppState::open(store).unwrap();
        assert_eq!(again.engines[&pid].tree(), st.engines[&pid].tree());
        assert!(again.tokens.authenticate(&token).is_some());
        // A retried request must not apply twice.
        let replay = again.engines.get_mut(&pid).unwrap().sync(&req).unwrap();
        assert_eq!(replay.rev, first.rev);
        assert_eq!(again.engines[&pid].tree(), st.engines[&pid].tree());
        // The other device still receives what it missed.
        let b = again
            .engines
            .get_mut(&pid)
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
        let pid = default_id_for(&uid);
        st.engines.get_mut(&pid).unwrap().register_endpoint("ep_a");
        st.persist().unwrap();
        let saved = st.engines[&pid].tree();
        st.engines
            .get_mut(&pid)
            .unwrap()
            .sync(&SyncRequest {
                endpoint_id: "ep_a".into(),
                since_rev: 0,
                ops: vec![add(1)],
            })
            .unwrap();
        assert_ne!(st.engines[&pid].tree(), saved);
        // A directory in the way of the temp file makes the save fail.
        fs::create_dir(tmp_path(&path)).unwrap();
        assert!(st.persist_or_revert().is_err());
        assert_eq!(st.engines[&pid].tree(), saved);
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
        let (sync, _) = tokens.create("laptop", Scope::Sync, None, None, Some("ep_a".into()));
        let (admin, _) = tokens.create("admin", Scope::Admin, None, None, None);
        let file = serde_json::json!({
            "engine": engine.snapshot(),
            "tokens": tokens.records(),
        });
        fs::write(&path, file.to_string()).unwrap();
        let st = AppState::open(Store::new(&path)).unwrap();
        let owner = st.users.by_name("owner").unwrap();
        assert!(owner.admin && owner.password_hash.is_none());
        assert_eq!(st.engines[&default_id_for(&owner.id)].tree(), engine.tree());
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
        let (pa, pb) = (default_id_for(&a), default_id_for(&b));
        st.engines.get_mut(&pa).unwrap().register_endpoint("ep_a");
        st.engines
            .get_mut(&pa)
            .unwrap()
            .sync(&SyncRequest {
                endpoint_id: "ep_a".into(),
                since_rev: 0,
                ops: vec![add(1)],
            })
            .unwrap();
        assert_eq!(st.engines[&pa].rev(), 1);
        assert_eq!(st.engines[&pb].rev(), 0);
        assert!(st.remove_user(&a));
        assert!(!st.engines.contains_key(&pa));
    }

    #[test]
    fn tokens_must_belong_to_a_user() {
        let path = tmp("orphan");
        let mut tokens = Tokens::new();
        tokens.create("x", Scope::Read, None, None, None);
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

    #[test]
    fn layout_from_before_profiles_gets_a_default_profile() {
        let path = tmp("preprofiles");
        let mut st = AppState::open(Store::new(&path)).unwrap();
        let uid = st.add_user("alice", false, None).unwrap().id;
        let pid = default_id_for(&uid);
        st.engines.get_mut(&pid).unwrap().register_endpoint("ep_a");
        st.engines
            .get_mut(&pid)
            .unwrap()
            .sync(&SyncRequest {
                endpoint_id: "ep_a".into(),
                since_rev: 0,
                ops: vec![add(1)],
            })
            .unwrap();
        let (tok, _) = st.tokens.create(
            "a",
            Scope::Sync,
            Some(uid.clone()),
            Some(pid.clone()),
            Some("ep_a".into()),
        );
        st.persist().unwrap();
        // Rewrite the file as the previous version wrote it.
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("profiles");
        let tree = v["engines"][&pid].take();
        v["engines"] = serde_json::json!({ uid.clone(): tree });
        v["tokens"][0].as_object_mut().unwrap().remove("profile_id");
        fs::write(&path, v.to_string()).unwrap();
        let again = AppState::open(Store::new(&path)).unwrap();
        let p = again.profiles.for_user(&uid);
        assert_eq!(
            (p.len(), p[0].name.as_str(), p[0].id.as_str()),
            (1, "Default", pid.as_str())
        );
        assert_eq!(again.engines[&pid].rev(), 1);
        let rec = again.tokens.authenticate(&tok).unwrap();
        assert_eq!(rec.profile_id.as_deref(), Some(pid.as_str()));
    }

    #[test]
    fn profiles_have_their_own_trees_and_tokens() {
        let mut st = AppState::default();
        let uid = st.add_user("alice", false, None).unwrap().id;
        let work = st.add_profile(&uid, "Work").unwrap().id;
        let home = default_id_for(&uid);
        // Two profiles: a token has to say which.
        assert!(st.issue_token(Some(&uid), None, "d", Scope::Sync).is_err());
        let (_, rec) = st
            .issue_token(Some(&uid), Some("work"), "d", Scope::Sync)
            .unwrap();
        assert_eq!(rec.profile_id.as_deref(), Some(work.as_str()));
        let ep = rec.endpoint_id.unwrap();
        st.engines
            .get_mut(&work)
            .unwrap()
            .sync(&SyncRequest {
                endpoint_id: ep,
                since_rev: 0,
                ops: vec![add(1)],
            })
            .unwrap();
        assert_eq!((st.engines[&work].rev(), st.engines[&home].rev()), (1, 0));
        // Another user's profile cannot be named, a duplicate name is refused.
        let other = st.add_user("bob", false, None).unwrap().id;
        assert!(st
            .issue_token(Some(&other), Some(&work), "d", Scope::Read)
            .is_err());
        assert!(st.add_profile(&uid, "WORK").is_err());
        // Removing a profile drops its tree and its devices; the last one stays.
        assert_eq!(st.remove_profile(&other, &work), Ok(false));
        assert_eq!(st.remove_profile(&uid, &work), Ok(true));
        assert!(!st.engines.contains_key(&work));
        assert!(st.tokens.counts_by_profile().is_empty());
        assert!(st.remove_profile(&uid, &home).is_err());
    }

    #[test]
    fn a_backup_survives_a_restart_and_goes_with_its_profile() {
        let path = tmp("backup");
        let mut st = AppState::default();
        let uid = st.add_user("alice", false, None).unwrap().id;
        let work = st.add_profile(&uid, "Work").unwrap().id;
        st.engines
            .get_mut(&work)
            .unwrap()
            .sync(&SyncRequest {
                endpoint_id: "e".into(),
                since_rev: 0,
                ops: vec![add(1)],
            })
            .unwrap();
        st.engines.get_mut(&work).unwrap().register_endpoint("e");
        st.engines
            .get_mut(&work)
            .unwrap()
            .sync(&SyncRequest {
                endpoint_id: "e".into(),
                since_rev: 0,
                ops: vec![add(2)],
            })
            .unwrap();
        st.clear_profile(&work).unwrap();
        // Clearing an empty profile again keeps the copy of the real tree.
        st.clear_profile(&work).unwrap();
        assert_eq!(st.engines[&work].tree().nodes.len(), 4);
        st.store = Some(Store::new(&path));
        st.persist().unwrap();
        let mut again = AppState::open(Store::new(&path)).unwrap();
        assert_eq!(again.backups[&work].bookmarks(), 1);
        again.restore_profile(&work).unwrap();
        assert_eq!(again.engines[&work].tree().nodes.len(), 5);
        again.remove_profile(&uid, &work).unwrap();
        assert!(again.backups.is_empty());
    }

    #[test]
    fn tokens_must_point_at_a_profile_of_their_user() {
        let path = tmp("badprofile");
        let mut st = AppState::default();
        let a = st.add_user("alice", false, None).unwrap().id;
        let b = st.add_user("bob", false, None).unwrap().id;
        let (pa, pb) = (default_id_for(&a), default_id_for(&b));
        // Alice's token pointing at Bob's profile.
        st.tokens
            .create("x", Scope::Read, Some(a.clone()), Some(pb), None);
        Store::new(&path)
            .save(
                &st.users,
                &st.profiles,
                &st.engines,
                &st.tokens,
                &st.backups,
                &st.dynacat_ui,
                None,
            )
            .unwrap();
        let err = Store::new(&path).load().err().unwrap();
        assert!(err.contains("does not match any user and profile"), "{err}");
        // A token for a profile that does not exist.
        let mut st = AppState::default();
        let a = st.add_user("alice", false, None).unwrap().id;
        st.tokens
            .create("x", Scope::Read, Some(a), Some("p_gone".into()), None);
        Store::new(&path)
            .save(
                &st.users,
                &st.profiles,
                &st.engines,
                &st.tokens,
                &st.backups,
                &st.dynacat_ui,
                None,
            )
            .unwrap();
        assert!(Store::new(&path).load().is_err());
        let _ = pa;
    }
}
