//! On-disk persistence: one JSON snapshot of the engine and the tokens,
//! replaced atomically (write a temp file, fsync, rename), readable by the
//! owner only because it holds token hashes.

use crate::api::AppState;
use crate::auth::{TokenRecord, Tokens};
use crate::engine::{Engine, Snapshot};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct SaveFile<'a> {
    engine: Snapshot,
    tokens: &'a [TokenRecord],
}

#[derive(Deserialize)]
struct LoadFile {
    engine: Snapshot,
    tokens: Vec<TokenRecord>,
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
    pub fn load(&self) -> Result<Option<(Engine, Tokens)>, String> {
        let text = match fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", self.path.display())),
        };
        let f: LoadFile = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not a valid state file: {e}", self.path.display()))?;
        Ok(Some((
            Engine::restore(f.engine)?,
            Tokens::from_records(f.tokens),
        )))
    }

    pub fn save(&self, engine: &Engine, tokens: &Tokens) -> io::Result<()> {
        let records = tokens.records();
        let bytes = serde_json::to_vec(&SaveFile {
            engine: engine.snapshot(),
            tokens: &records,
        })
        .map_err(io::Error::other)?;
        let tmp = tmp_path(&self.path);
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
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
        let (engine, tokens) = store.load()?.unwrap_or_default();
        Ok(AppState {
            engine,
            tokens,
            store: Some(store),
        })
    }

    /// Write the current state; a no-op without a store.
    pub fn persist(&self) -> io::Result<()> {
        match &self.store {
            Some(s) => s.save(&self.engine, &self.tokens),
            None => Ok(()),
        }
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
        st.engine.register_endpoint("ep_a");
        st.engine.register_endpoint("ep_b");
        let (token, _) = st.tokens.create("a", Scope::Sync, Some("ep_a".into()));
        let req = SyncRequest {
            endpoint_id: "ep_a".into(),
            since_rev: 0,
            ops: vec![add(1), add(2)],
        };
        let first = st.engine.sync(&req).unwrap();
        st.persist().unwrap();

        let mut again = AppState::open(store).unwrap();
        assert_eq!(again.engine.tree(), st.engine.tree());
        assert!(again.tokens.authenticate(&token).is_some());
        // A retried request must not apply twice.
        let replay = again.engine.sync(&req).unwrap();
        assert_eq!(replay.rev, first.rev);
        assert_eq!(again.engine.tree(), st.engine.tree());
        // The other device still receives what it missed.
        let b = again
            .engine
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
        let mut st = AppState::default();
        st.tokens.create("x", Scope::Read, None);
        let mut snap = serde_json::to_value(st.engine.snapshot()).unwrap();
        snap["nodes"] = serde_json::json!([]);
        let file = serde_json::json!({ "engine": snap, "tokens": [] });
        fs::write(&path, file.to_string()).unwrap();
        let err = Store::new(&path).load().err().unwrap();
        assert!(err.contains("well-known"), "{err}");
    }
}
