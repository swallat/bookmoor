//! In-memory sync engine (`docs/sync-architecture.md`, sections 3 and 4).
//!
//! The server is the only serialisation point: every request is applied
//! against the canonical tree in order. Instead of keeping a base snapshot
//! per endpoint, each node remembers which endpoint made its last change
//! per field group and at which revision (`Stamp`). An incoming op conflicts
//! with another endpoint's change when that change is newer than the op's
//! `since_rev`.
//!
//! Rules (section 4): fields are last-write-wins by arrival order (`newest`);
//! edit beats delete; a move into a deleted folder is re-parented to the
//! nearest surviving ancestor; a new node inside a deleted folder revives
//! the folder; a move that would create a cycle is rejected as
//! `conflict_lost`. Whenever the canonical result differs from what the
//! client asked for, the engine emits a *server-origin* change, which is
//! delivered to every endpoint including the sender. `add` of an id the
//! client already knows is an upsert of the full node when it comes from the
//! server.
//!
//! Known limitation: a node keeps only the latest stamp per field group, so
//! if endpoint B edits a node after endpoint A, A's earlier edit no longer
//! counts as a concurrent change for an edit-vs-delete decision.

use bookmoor_core::model::Kind;
use bookmoor_core::position;
use bookmoor_core::wire::{
    Op, OpBody, RejectReason, RejectedOp, SyncRequest, SyncResponse, TreeResponse, WireNode,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const ROOT: &str = "root";
/// Well-known children of the root; the extension maps browser roots to these.
pub const ROLES: [&str; 3] = ["toolbar", "other", "mobile"];
pub const MAX_OPS_PER_REQUEST: usize = 500;
/// Nodes (alive and tombstoned) the engine accepts; further adds are invalid.
pub const MAX_NODES: usize = 100_000;
/// Longest accepted id, op_id, title, url or position.
pub const MAX_FIELD_BYTES: usize = 4096;
/// Remembered op ids for idempotency; the oldest are forgotten first.
const MAX_SEEN_OPS: usize = 200_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    TooManyOps { max: usize },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Stamp {
    rev: u64,
    by: String,
}

impl Stamp {
    fn newer_than(&self, since: u64, ep: &str) -> bool {
        self.rev > since && self.by != ep
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Rec {
    id: String,
    parent: Option<String>,
    position: String,
    kind: Kind,
    title: String,
    url: Option<String>,
    created: Stamp,
    title_st: Stamp,
    url_st: Stamp,
    place_st: Stamp,
    deleted: Option<Stamp>,
}

impl Rec {
    fn alive(&self) -> bool {
        self.deleted.is_none()
    }
    fn touched_by_others(&self, since: u64, ep: &str) -> bool {
        self.created.newer_than(since, ep)
            || self.title_st.newer_than(since, ep)
            || self.url_st.newer_than(since, ep)
            || self.place_st.newer_than(since, ep)
    }
    fn add_body(&self) -> OpBody {
        OpBody::Add {
            id: self.id.clone(),
            parent: self.parent.clone().unwrap_or_default(),
            position: self.position.clone(),
            kind: self.kind,
            title: self.title.clone(),
            url: self.url.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogEntry {
    rev: u64,
    /// `None` = made by the server itself (delivered to everyone).
    origin: Option<String>,
    op_id: String,
    body: OpBody,
}

type Res<T> = Result<T, RejectReason>;

/// Version of the persisted engine state.
const SNAPSHOT_VERSION: u32 = 1;

/// The persisted form of an [`Engine`] (JSON map keys must be strings, so the
/// idempotency table is stored as a list in eviction order).
#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    version: u32,
    nodes: Vec<Rec>,
    rev: u64,
    horizon: u64,
    log: Vec<LogEntry>,
    endpoints: Vec<String>,
    seen: Vec<(String, String, Option<RejectReason>)>,
}

/// A change requested in the web UI.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UiEdit {
    Add {
        parent: String,
        kind: Kind,
        title: String,
        #[serde(default)]
        url: Option<String>,
        /// Sibling to insert before; `None` appends.
        #[serde(default)]
        before: Option<String>,
    },
    Update {
        id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
    Move {
        id: String,
        parent: String,
        #[serde(default)]
        before: Option<String>,
    },
    Remove {
        id: String,
    },
}

#[derive(Debug)]
pub struct Engine {
    nodes: BTreeMap<String, Rec>,
    rev: u64,
    /// Requests with `since_rev` below this need a full resync.
    horizon: u64,
    log: Vec<LogEntry>,
    endpoints: HashSet<String>,
    seen: HashMap<(String, String), Option<RejectReason>>,
    seen_order: std::collections::VecDeque<(String, String)>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

fn op_too_long(b: &OpBody) -> bool {
    let long = |s: &str| s.len() > MAX_FIELD_BYTES;
    match b {
        OpBody::Add {
            id,
            parent,
            position,
            title,
            url,
            ..
        } => {
            long(id)
                || long(parent)
                || long(position)
                || long(title)
                || url.as_deref().is_some_and(long)
        }
        OpBody::Update { id, title, url } => {
            long(id) || title.as_deref().is_some_and(long) || url.as_deref().is_some_and(long)
        }
        OpBody::Move {
            id,
            parent,
            position,
        } => long(id) || long(parent) || long(position),
        OpBody::Remove { id } => long(id),
    }
}

fn valid_url(u: &str) -> bool {
    match u.split_once(':') {
        Some((scheme, rest)) => {
            let mut c = scheme.chars();
            !rest.is_empty()
                && c.next().is_some_and(|f| f.is_ascii_alphabetic())
                && c.all(|ch| ch.is_ascii_alphanumeric() || "+.-".contains(ch))
        }
        None => false,
    }
}

impl Engine {
    pub fn new() -> Self {
        let mut e = Engine {
            nodes: BTreeMap::new(),
            rev: 0,
            horizon: 0,
            log: Vec::new(),
            endpoints: HashSet::new(),
            seen: HashMap::new(),
            seen_order: std::collections::VecDeque::new(),
        };
        let st = Stamp {
            rev: 0,
            by: "server".into(),
        };
        let mk = |id: &str, parent: Option<&str>, position: &str, title: &str| Rec {
            id: id.into(),
            parent: parent.map(String::from),
            position: position.into(),
            kind: Kind::Folder,
            title: title.into(),
            url: None,
            created: st.clone(),
            title_st: st.clone(),
            url_st: st.clone(),
            place_st: st.clone(),
            deleted: None,
        };
        e.nodes.insert(ROOT.into(), mk(ROOT, None, "", "Bookmarks"));
        for (role, pos) in ROLES.iter().zip(["V", "W", "X"]) {
            e.nodes
                .insert((*role).into(), mk(role, Some(ROOT), pos, role));
        }
        e
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut endpoints: Vec<String> = self.endpoints.iter().cloned().collect();
        endpoints.sort();
        Snapshot {
            version: SNAPSHOT_VERSION,
            nodes: self.nodes.values().cloned().collect(),
            rev: self.rev,
            horizon: self.horizon,
            log: self.log.clone(),
            endpoints,
            seen: self
                .seen_order
                .iter()
                .filter_map(|k| self.seen.get(k).map(|v| (k.0.clone(), k.1.clone(), *v)))
                .collect(),
        }
    }

    /// Rebuild an engine from persisted state, refusing anything that would
    /// break the invariants the sync code relies on.
    pub fn restore(s: Snapshot) -> Result<Engine, String> {
        if s.version != SNAPSHOT_VERSION {
            return Err(format!("unsupported snapshot version {}", s.version));
        }
        let nodes: BTreeMap<String, Rec> = s.nodes.into_iter().map(|r| (r.id.clone(), r)).collect();
        for id in std::iter::once(&ROOT).chain(ROLES.iter()) {
            if !nodes.get(*id).is_some_and(Rec::alive) {
                return Err(format!("snapshot lacks the well-known node {id}"));
            }
        }
        // Every live node must reach the root through live parents; this
        // also rules out cycles, which the sync code would loop on.
        for r in nodes.values().filter(|r| r.alive()) {
            let mut cur = r;
            let mut steps = 0;
            while cur.id != ROOT {
                steps += 1;
                match cur.parent.as_ref().and_then(|p| nodes.get(p)) {
                    Some(p) if p.alive() && steps <= nodes.len() => cur = p,
                    _ => return Err(format!("node {} is not attached to the root", r.id)),
                }
            }
        }
        for id in ROLES {
            if nodes[id].parent.as_deref() != Some(ROOT) {
                return Err(format!("well-known node {id} is not below the root"));
            }
        }
        let stamp_ok = |st: &Stamp| st.rev <= s.rev;
        let stamps_ok = nodes.values().all(|r| {
            stamp_ok(&r.created)
                && stamp_ok(&r.title_st)
                && stamp_ok(&r.url_st)
                && stamp_ok(&r.place_st)
                && r.deleted.as_ref().is_none_or(stamp_ok)
        });
        if !stamps_ok
            || s.log.iter().any(|e| e.rev > s.rev)
            || s.horizon > s.rev
            || s.seen.len() > MAX_SEEN_OPS
        {
            return Err("snapshot revisions are inconsistent".into());
        }
        let mut seen = HashMap::new();
        let mut seen_order = std::collections::VecDeque::new();
        for (ep, op, v) in s.seen {
            if seen.insert((ep.clone(), op.clone()), v).is_none() {
                seen_order.push_back((ep, op));
            }
        }
        Ok(Engine {
            nodes,
            rev: s.rev,
            horizon: s.horizon,
            log: s.log,
            endpoints: s.endpoints.into_iter().collect(),
            seen,
            seen_order,
        })
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn register_endpoint(&mut self, id: &str) {
        self.endpoints.insert(id.to_string());
    }

    /// Whether the endpoint was registered.
    pub fn remove_endpoint(&mut self, id: &str) -> bool {
        let known = self.endpoints.remove(id);
        self.seen.retain(|(ep, _), _| ep != id);
        self.seen_order.retain(|(ep, _)| ep != id);
        known
    }

    /// The canonical tree, alive nodes only, parent-first.
    pub fn tree(&self) -> TreeResponse {
        let mut nodes = Vec::new();
        let mut kids: BTreeMap<&str, Vec<&Rec>> = BTreeMap::new();
        for r in self.nodes.values().filter(|r| r.alive()) {
            if let Some(p) = &r.parent {
                kids.entry(p.as_str()).or_default().push(r);
            }
        }
        let mut stack: Vec<&Rec> = vec![&self.nodes[ROOT]];
        while let Some(r) = stack.pop() {
            nodes.push(WireNode {
                id: r.id.clone(),
                parent: r.parent.clone(),
                position: r.position.clone(),
                kind: r.kind,
                title: r.title.clone(),
                url: r.url.clone(),
            });
            let mut c = kids.get(r.id.as_str()).cloned().unwrap_or_default();
            c.sort_by(|a, b| (&b.position, &b.id).cmp(&(&a.position, &a.id)));
            stack.extend(c);
        }
        TreeResponse {
            rev: self.rev,
            nodes,
        }
    }

    /// How many bookmarks (not folders) are alive.
    pub fn bookmark_count(&self) -> usize {
        self.nodes
            .values()
            .filter(|r| r.alive() && r.kind == Kind::Bookmark)
            .count()
    }

    /// An edit made by the user in the web UI. It runs through the same
    /// operations as a device's sync request, so every device receives it as
    /// a normal change. `new_id` names the node of an add.
    pub fn ui_edit(&mut self, edit: UiEdit, new_id: &str) -> Result<(), String> {
        let rev = self.rev + 1;
        let op_id = format!("ui_{rev}");
        let body = match edit {
            UiEdit::Add {
                parent,
                kind,
                title,
                url,
                before,
            } => {
                let position = self.position_in(&parent, before.as_deref(), None)?;
                OpBody::Add {
                    id: new_id.to_string(),
                    parent,
                    position,
                    kind,
                    title,
                    url,
                }
            }
            UiEdit::Update { id, title, url } => OpBody::Update { id, title, url },
            UiEdit::Move { id, parent, before } => {
                if self.is_within(&parent, &id) {
                    return Err("a folder cannot be moved into itself".into());
                }
                let position = self.position_in(&parent, before.as_deref(), Some(&id))?;
                OpBody::Move {
                    id,
                    parent,
                    position,
                }
            }
            UiEdit::Remove { id } => OpBody::Remove { id },
        };
        if op_too_long(&body) {
            return Err("a field is too long".into());
        }
        // Whatever is in the tree now is the newest state: nothing the
        // server holds can be concurrent with this edit.
        let since = self.rev;
        let id = match &body {
            OpBody::Add { id, .. }
            | OpBody::Update { id, .. }
            | OpBody::Move { id, .. }
            | OpBody::Remove { id } => id.clone(),
        };
        if let OpBody::Update { .. } | OpBody::Move { .. } | OpBody::Remove { .. } = &body {
            match self.nodes.get(&id) {
                Some(r) if r.alive() => {}
                _ => return Err("that bookmark no longer exists".into()),
            }
        }
        if let OpBody::Add { parent, .. } | OpBody::Move { parent, .. } = &body {
            match self.nodes.get(parent) {
                Some(r) if r.alive() && r.kind == Kind::Folder => {}
                _ => return Err("the target folder no longer exists".into()),
            }
        }
        let op = Op { op_id, body };
        self.apply("server", since, &op).map_err(|r| {
            match r {
                RejectReason::Invalid => {
                    "that change is not allowed (is the URL complete, like https://example.com?)"
                }
                RejectReason::ConflictLost => "a folder cannot be moved into itself",
                RejectReason::Gone => "that bookmark no longer exists",
            }
            .to_string()
        })
    }

    /// A position in `parent` right before the sibling `before` (at the end
    /// if `None`), ignoring the node `skip` that is being moved.
    fn position_in(
        &self,
        parent: &str,
        before: Option<&str>,
        skip: Option<&str>,
    ) -> Result<String, String> {
        let mut kids: Vec<&Rec> = self
            .nodes
            .values()
            .filter(|r| {
                r.alive() && r.parent.as_deref() == Some(parent) && Some(r.id.as_str()) != skip
            })
            .collect();
        kids.sort_by(|a, b| position::cmp_siblings((&a.position, &a.id), (&b.position, &b.id)));
        let (prev, next) = match before {
            None => (kids.last().map(|r| r.position.as_str()), None),
            Some(b) => {
                let i = kids
                    .iter()
                    .position(|r| r.id == b)
                    .ok_or("the bookmark to place it before is not in that folder")?;
                (
                    i.checked_sub(1).map(|j| kids[j].position.as_str()),
                    Some(kids[i].position.as_str()),
                )
            }
        };
        position::between(prev, next).map_err(|_| "cannot place it there".to_string())
    }

    /// Replace the whole tree by `nodes` (parent-first or not, as
    /// [`tree`](Self::tree) returns them). Node ids are kept, so devices that
    /// resync keep their pairings. The change log is dropped and the horizon
    /// raised, so every device must resync and then removes what is gone and
    /// creates what is new, as for a remote delete. Registered devices stay
    /// registered.
    pub fn replace_tree(&mut self, nodes: &[WireNode]) -> Result<(), String> {
        let rev = self.rev + 1;
        let st = Stamp {
            rev,
            by: "server".into(),
        };
        let mut recs = Vec::with_capacity(nodes.len());
        for n in nodes {
            let long = |s: &str| s.len() > MAX_FIELD_BYTES;
            if long(&n.id)
                || long(&n.title)
                || n.parent.as_deref().is_some_and(long)
                || n.url.as_deref().is_some_and(long)
            {
                return Err(format!("node {} has a field that is too long", n.id));
            }
            if n.id != ROOT && !position::is_valid(&n.position) {
                return Err(format!("node {} has an invalid position", n.id));
            }
            match (n.kind, n.url.as_deref()) {
                (Kind::Bookmark, Some(u)) if valid_url(u) => {}
                (Kind::Folder, None) => {}
                _ => return Err(format!("node {} has an invalid kind or URL", n.id)),
            }
            recs.push(Rec {
                id: n.id.clone(),
                parent: n.parent.clone(),
                position: n.position.clone(),
                kind: n.kind,
                title: n.title.clone(),
                url: n.url.clone(),
                created: st.clone(),
                title_st: st.clone(),
                url_st: st.clone(),
                place_st: st.clone(),
                deleted: None,
            });
        }
        if recs.len() > MAX_NODES {
            return Err("the tree is too big".into());
        }
        let ids: HashSet<&str> = recs.iter().map(|r| r.id.as_str()).collect();
        if ids.len() != recs.len() {
            return Err("duplicate node ids".into());
        }
        if recs
            .iter()
            .any(|r| r.parent.as_deref().is_some_and(|p| !ids.contains(p)))
        {
            return Err("a node has an unknown parent".into());
        }
        let folders: HashSet<&str> = recs
            .iter()
            .filter(|r| r.kind == Kind::Folder)
            .map(|r| r.id.as_str())
            .collect();
        if recs
            .iter()
            .any(|r| r.parent.as_deref().is_some_and(|p| !folders.contains(p)))
        {
            return Err("a bookmark has children".into());
        }
        let mut endpoints: Vec<String> = self.endpoints.iter().cloned().collect();
        endpoints.sort();
        *self = Engine::restore(Snapshot {
            version: SNAPSHOT_VERSION,
            nodes: recs,
            rev,
            horizon: rev,
            log: Vec::new(),
            endpoints,
            seen: Vec::new(),
        })?;
        Ok(())
    }

    /// Drop log entries and tombstones up to `rev`; older clients must resync.
    pub fn prune(&mut self, rev: u64) {
        let rev = rev.min(self.rev);
        self.log.retain(|e| e.rev > rev);
        self.nodes.retain(|_, r| match &r.deleted {
            None => true,
            Some(d) => d.rev > rev,
        });
        self.horizon = self.horizon.max(rev);
    }

    pub fn sync(&mut self, req: &SyncRequest) -> Result<SyncResponse, SyncError> {
        if req.ops.len() > MAX_OPS_PER_REQUEST {
            return Err(SyncError::TooManyOps {
                max: MAX_OPS_PER_REQUEST,
            });
        }
        let ep = req.endpoint_id.as_str();
        if !self.endpoints.contains(ep) || req.since_rev > self.rev || req.since_rev < self.horizon
        {
            return Ok(SyncResponse {
                rev: self.rev,
                ops: vec![],
                rejected: vec![],
                resync: true,
            });
        }
        let mut rejected = Vec::new();
        for op in &req.ops {
            let key = (ep.to_string(), op.op_id.clone());
            if let Some(prev) = self.seen.get(&key) {
                if let Some(reason) = prev {
                    rejected.push(RejectedOp {
                        op_id: op.op_id.clone(),
                        reason: *reason,
                    });
                }
                continue;
            }
            let res = if op.op_id.len() > MAX_FIELD_BYTES || op_too_long(&op.body) {
                Err(RejectReason::Invalid)
            } else {
                self.apply(ep, req.since_rev, op)
            };
            self.seen.insert(key.clone(), res.err());
            self.seen_order.push_back(key);
            if self.seen_order.len() > MAX_SEEN_OPS {
                if let Some(old) = self.seen_order.pop_front() {
                    self.seen.remove(&old);
                }
            }
            if let Err(reason) = res {
                rejected.push(RejectedOp {
                    op_id: op.op_id.clone(),
                    reason,
                });
            }
        }
        let had_remove = req
            .ops
            .iter()
            .any(|o| matches!(o.body, OpBody::Remove { .. }));
        let touched: HashSet<&str> = req
            .ops
            .iter()
            .map(|o| match &o.body {
                OpBody::Add { id, .. }
                | OpBody::Update { id, .. }
                | OpBody::Move { id, .. }
                | OpBody::Remove { id } => id.as_str(),
            })
            .collect();
        let ops = self.delta(ep, req.since_rev, had_remove, &touched);
        Ok(SyncResponse {
            rev: self.rev,
            ops,
            rejected,
            resync: false,
        })
    }

    /// Canonical changes after `since`, minus the endpoint's own. Where the
    /// endpoint's own later change overwrote someone else's earlier one, the
    /// stale entry is replaced by the node's final canonical state, so the
    /// client cannot regress by applying the older change after its own.
    fn delta(&self, ep: &str, since: u64, had_remove: bool, touched: &HashSet<&str>) -> Vec<Op> {
        fn id_of(b: &OpBody) -> &str {
            match b {
                OpBody::Add { id, .. }
                | OpBody::Update { id, .. }
                | OpBody::Move { id, .. }
                | OpBody::Remove { id } => id,
            }
        }
        let entries: Vec<&LogEntry> = self.log.iter().filter(|e| e.rev > since).collect();
        let mut last_own: HashMap<&str, u64> = HashMap::new();
        for e in entries.iter().filter(|e| e.origin.as_deref() == Some(ep)) {
            last_own.insert(id_of(&e.body), e.rev);
        }
        let mut replaced: HashMap<&str, u64> = HashMap::new();
        for e in entries.iter().filter(|e| e.origin.as_deref() != Some(ep)) {
            let id = id_of(&e.body);
            if last_own.get(id).is_some_and(|own| e.rev < *own) {
                replaced.insert(id, e.rev);
            }
        }
        // When the endpoint deleted a subtree, it dropped every node it held
        // in there. Nodes that others moved out concurrently survive on the
        // server, so changes to them are sent as full upserts.
        let own_removed = had_remove;
        let mut out = Vec::new();
        for e in entries.iter().filter(|e| e.origin.as_deref() != Some(ep)) {
            let id = id_of(&e.body);
            let body = match replaced.get(id) {
                None if own_removed && !matches!(e.body, OpBody::Remove { .. }) => {
                    match self.nodes.get(id) {
                        Some(r) if r.alive() => r.add_body(),
                        _ => e.body.clone(),
                    }
                }
                None => e.body.clone(),
                Some(last) if *last == e.rev => match self.nodes.get(id) {
                    Some(r) if r.alive() => r.add_body(),
                    _ => OpBody::Remove { id: id.to_string() },
                },
                Some(_) => continue,
            };
            out.push(Op {
                op_id: e.op_id.clone(),
                body,
            });
        }
        // A delivered `remove` deletes a whole subtree on the client, which
        // may contain nodes the client itself just added or moved there.
        // Re-assert the final state of everything it touched, parent first.
        if had_remove || out.iter().any(|o| matches!(o.body, OpBody::Remove { .. })) {
            // Includes everything below a touched folder: the client may
            // have dropped those descendants together with the folder. The
            // same goes for nodes others changed in the meantime, since they
            // may have been moved out of a subtree the client deleted.
            let changed: Vec<String> = out.iter().map(|o| id_of(&o.body).to_string()).collect();
            let index = self.children_index();
            let mut ids: HashSet<String> = HashSet::new();
            for id in last_own
                .keys()
                .copied()
                .chain(touched.iter().copied())
                .chain(changed.iter().map(String::as_str))
            {
                if !ids.contains(id) && self.nodes.get(id).is_some_and(|r| r.alive()) {
                    ids.extend(Self::subtree_in(&index, id));
                }
            }
            let mut own: Vec<(usize, &Rec)> = ids
                .iter()
                .filter_map(|id| self.nodes.get(id))
                .map(|r| (self.depth(&r.id), r))
                .collect();
            own.sort_by(|a, b| (a.0, &a.1.id).cmp(&(b.0, &b.1.id)));
            for (_, r) in own {
                out.push(Op {
                    op_id: format!("srv_{}", self.rev),
                    body: r.add_body(),
                });
            }
        }
        out
    }

    /// Alive children per parent, built once per operation (O(nodes)).
    fn children_index(&self) -> HashMap<&str, Vec<&str>> {
        let mut idx: HashMap<&str, Vec<&str>> = HashMap::new();
        for r in self.nodes.values().filter(|r| r.alive()) {
            if let Some(p) = r.parent.as_deref() {
                idx.entry(p).or_default().push(r.id.as_str());
            }
        }
        idx
    }

    /// `id` and its alive descendants, parent first.
    fn subtree_in(index: &HashMap<&str, Vec<&str>>, id: &str) -> Vec<String> {
        let mut subtree = vec![id.to_string()];
        let mut i = 0;
        while i < subtree.len() {
            if let Some(kids) = index.get(subtree[i].as_str()) {
                let kids: Vec<String> = kids.iter().map(|k| (*k).to_string()).collect();
                subtree.extend(kids);
            }
            i += 1;
        }
        subtree
    }

    fn live_subtree(&self, id: &str) -> Vec<String> {
        Self::subtree_in(&self.children_index(), id)
    }

    fn depth(&self, id: &str) -> usize {
        let mut d = 0;
        let mut cur = self.nodes.get(id).and_then(|r| r.parent.clone());
        while let Some(p) = cur {
            d += 1;
            cur = self.nodes.get(&p).and_then(|r| r.parent.clone());
        }
        d
    }

    // ---- helpers -------------------------------------------------------

    fn push(&mut self, origin: Option<&str>, op_id: Option<&str>, body: OpBody) -> u64 {
        self.rev += 1;
        let op_id = op_id.map_or_else(|| format!("srv_{}", self.rev), String::from);
        self.log.push(LogEntry {
            rev: self.rev,
            origin: origin.map(String::from),
            op_id,
            body,
        });
        self.rev
    }

    fn stamp(&self, ep: &str) -> Stamp {
        Stamp {
            rev: self.rev + 1,
            by: ep.to_string(),
        }
    }

    fn is_protected(id: &str) -> bool {
        id == ROOT || ROLES.contains(&id)
    }

    /// Nearest alive ancestor (exclusive) of `id`; the root always survives.
    fn alive_ancestor(&self, id: &str) -> String {
        let mut cur = self.nodes.get(id).and_then(|r| r.parent.clone());
        while let Some(p) = cur {
            match self.nodes.get(&p) {
                Some(r) if r.alive() => return p,
                Some(r) => cur = r.parent.clone(),
                None => break,
            }
        }
        ROOT.to_string()
    }

    fn is_within(&self, node: &str, ancestor: &str) -> bool {
        let mut cur = Some(node.to_string());
        while let Some(id) = cur {
            if id == ancestor {
                return true;
            }
            cur = self.nodes.get(&id).and_then(|r| r.parent.clone());
        }
        false
    }

    /// Revive a tombstoned node because of a concurrent edit (edit beats
    /// delete). With `with_ancestors` dead ancestors are revived too (a new
    /// child needs its folder); otherwise the node is attached to the
    /// nearest surviving ancestor. Returns the revived ids, parent first.
    fn revive(&mut self, id: &str, ep: &str, with_ancestors: bool) -> Vec<String> {
        let mut out = Vec::new();
        let Some(rec) = self.nodes.get(id) else {
            return out;
        };
        if rec.alive() {
            return out;
        }
        let parent = rec.parent.clone().unwrap_or_else(|| ROOT.to_string());
        if self.nodes.get(&parent).is_some_and(|p| !p.alive()) {
            if with_ancestors {
                out = self.revive(&parent, ep, true);
            } else {
                let np = self.alive_ancestor(id);
                self.nodes.get_mut(id).unwrap().parent = Some(np);
            }
        }
        let st = self.stamp(ep);
        let r = self.nodes.get_mut(id).unwrap();
        r.deleted = None;
        r.created = st.clone();
        r.place_st = st;
        out.push(id.to_string());
        out
    }

    /// Server-origin upsert of a node's current state.
    fn push_upsert(&mut self, id: &str) {
        let body = self.nodes[id].add_body();
        self.push(None, None, body);
    }

    /// For update/move: a tombstoned target may be revived only if someone
    /// else deleted it after the client's `since_rev`.
    fn revivable(&self, id: &str, since: u64, ep: &str) -> Res<()> {
        match self.nodes.get(id) {
            None => Err(RejectReason::Gone),
            Some(r) => match &r.deleted {
                None => Ok(()),
                Some(d) if d.newer_than(since, ep) => Ok(()),
                Some(_) => Err(RejectReason::Gone),
            },
        }
    }

    // ---- ops -----------------------------------------------------------

    fn apply(&mut self, ep: &str, since: u64, op: &Op) -> Res<()> {
        match &op.body {
            OpBody::Add {
                id,
                parent,
                position: pos,
                kind,
                title,
                url,
            } => self.add(ep, since, op, id, parent, pos, *kind, title, url.as_deref()),
            OpBody::Update { id, title, url } => {
                self.update(ep, since, op, id, title.as_deref(), url.as_deref())
            }
            OpBody::Move {
                id,
                parent,
                position: pos,
            } => self.mv(ep, since, op, id, parent, pos),
            OpBody::Remove { id } => self.remove(ep, since, op, id),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        ep: &str,
        since: u64,
        op: &Op,
        id: &str,
        parent: &str,
        pos: &str,
        kind: Kind,
        title: &str,
        url: Option<&str>,
    ) -> Res<()> {
        if !position::is_valid(pos) {
            return Err(RejectReason::Invalid);
        }
        match (kind, url) {
            (Kind::Bookmark, Some(u)) if valid_url(u) => {}
            (Kind::Folder, None) => {}
            _ => return Err(RejectReason::Invalid),
        }
        if self.nodes.len() >= MAX_NODES && !self.nodes.contains_key(id) {
            return Err(RejectReason::Invalid);
        }
        if let Some(existing) = self.nodes.get(id) {
            // Retried add that already took effect is a no-op.
            let same = existing.alive()
                && existing.parent.as_deref() == Some(parent)
                && existing.position == pos
                && existing.kind == kind
                && existing.title == title
                && existing.url.as_deref() == url;
            return if same {
                Ok(())
            } else {
                Err(RejectReason::Invalid)
            };
        }
        let Some(p) = self.nodes.get(parent) else {
            return Err(RejectReason::Invalid);
        };
        if p.kind != Kind::Folder {
            return Err(RejectReason::Invalid);
        }
        if !p.alive() {
            self.revivable(parent, since, ep)?;
            for id in self.revive(parent, ep, true) {
                self.push_upsert(&id);
                // The stamp must carry the revision of the entry just pushed.
                let st = Stamp {
                    rev: self.rev,
                    by: ep.to_string(),
                };
                let r = self.nodes.get_mut(&id).unwrap();
                r.created = st.clone();
                r.place_st = st;
            }
        }
        let st = self.stamp(ep);
        let rec = Rec {
            id: id.into(),
            parent: Some(parent.into()),
            position: pos.into(),
            kind,
            title: title.into(),
            url: url.map(String::from),
            created: st.clone(),
            title_st: st.clone(),
            url_st: st.clone(),
            place_st: st,
            deleted: None,
        };
        let body = rec.add_body();
        self.nodes.insert(id.into(), rec);
        self.push(Some(ep), Some(&op.op_id), body);
        Ok(())
    }

    fn update(
        &mut self,
        ep: &str,
        since: u64,
        op: &Op,
        id: &str,
        title: Option<&str>,
        url: Option<&str>,
    ) -> Res<()> {
        self.revivable(id, since, ep)?;
        if let Some(u) = url {
            let r = &self.nodes[id];
            if r.kind != Kind::Bookmark || !valid_url(u) {
                return Err(RejectReason::Invalid);
            }
        }
        if Self::is_protected(id) && title.is_some() {
            return Err(RejectReason::Invalid);
        }
        let revived = !self.revive(id, ep, false).is_empty();
        let st = self.stamp(ep);
        let r = self.nodes.get_mut(id).unwrap();
        let mut changed_title = None;
        let mut changed_url = None;
        if let Some(t) = title.filter(|t| *t != r.title) {
            r.title = t.into();
            r.title_st = st.clone();
            changed_title = Some(t.to_string());
        }
        if let Some(u) = url.filter(|u| r.url.as_deref() != Some(*u)) {
            r.url = Some(u.into());
            r.url_st = st;
            changed_url = Some(u.to_string());
        }
        if revived {
            // Everyone (the sender included, whose response still carries the
            // delete) gets the final state of the revived node.
            self.push_upsert(id);
        } else if changed_title.is_some() || changed_url.is_some() {
            self.push(
                Some(ep),
                Some(&op.op_id),
                OpBody::Update {
                    id: id.into(),
                    title: changed_title,
                    url: changed_url,
                },
            );
        }
        Ok(())
    }

    fn mv(&mut self, ep: &str, since: u64, op: &Op, id: &str, parent: &str, pos: &str) -> Res<()> {
        if !position::is_valid(pos) || Self::is_protected(id) {
            return Err(RejectReason::Invalid);
        }
        self.revivable(id, since, ep)?;
        match self.nodes.get(parent) {
            Some(p) if p.kind == Kind::Folder => {}
            _ => return Err(RejectReason::Invalid),
        }
        // Target folder deleted: nearest surviving ancestor instead.
        let mut target = parent.to_string();
        let mut adjusted = false;
        if !self.nodes[parent].alive() {
            target = self.alive_ancestor(parent);
            adjusted = true;
        }
        if self.is_within(&target, id) {
            // Cycle: tell the sender the canonical placement.
            let r = &self.nodes[id];
            let body = OpBody::Move {
                id: id.into(),
                parent: r.parent.clone().unwrap_or_default(),
                position: r.position.clone(),
            };
            self.push(None, None, body);
            return Err(RejectReason::ConflictLost);
        }
        let revived = !self.revive(id, ep, false).is_empty();
        let r = &self.nodes[id];
        let unchanged = r.parent.as_deref() == Some(target.as_str()) && r.position == pos;
        if unchanged && !revived {
            return Ok(());
        }
        let st = self.stamp(ep);
        let r = self.nodes.get_mut(id).unwrap();
        r.parent = Some(target.clone());
        r.position = pos.into();
        r.place_st = st;
        if revived {
            self.push_upsert(id);
            return Ok(());
        }
        let body = OpBody::Move {
            id: id.into(),
            parent: target,
            position: pos.into(),
        };
        self.push(Some(ep), Some(&op.op_id), body.clone());
        if adjusted {
            self.push(None, None, body);
        }
        Ok(())
    }

    fn remove(&mut self, ep: &str, since: u64, op: &Op, id: &str) -> Res<()> {
        let Some(rec) = self.nodes.get(id) else {
            return Err(RejectReason::Gone);
        };
        if !rec.alive() {
            return Ok(()); // delete vs delete
        }
        if Self::is_protected(id) {
            return Err(RejectReason::Invalid);
        }
        if rec.touched_by_others(since, ep) {
            // Edit wins: restore the node and everything below it on the
            // sender, which already dropped the whole subtree locally.
            for n in self.live_subtree(id) {
                self.push_upsert(&n);
            }
            return Err(RejectReason::ConflictLost);
        }
        let subtree = self.live_subtree(id);
        let is_new = |s: &Self, n: &str| s.nodes[n].created.newer_than(since, ep);
        let is_edited = |s: &Self, n: &str| s.nodes[n].touched_by_others(since, ep);
        // New nodes keep their folder chain alive (up to and including `id`).
        let mut keep: HashSet<String> = HashSet::new();
        for n in subtree.iter().filter(|n| is_new(self, n)) {
            let mut cur = Some(n.clone());
            while let Some(c) = cur {
                keep.insert(c.clone());
                if c == id {
                    break;
                }
                cur = self.nodes[&c].parent.clone();
            }
        }
        let edited: Vec<String> = subtree
            .iter()
            .filter(|n| !keep.contains(*n) && is_edited(self, n))
            .cloned()
            .collect();
        // Whatever hangs below a surviving node stays with it.
        let retained: HashSet<String> = subtree
            .iter()
            .filter(|n| {
                if keep.contains(*n) || edited.contains(*n) {
                    return false;
                }
                let mut cur = self.nodes[*n].parent.clone();
                while let Some(p) = cur {
                    if keep.contains(&p) || edited.contains(&p) {
                        return true;
                    }
                    if p == id {
                        break;
                    }
                    cur = self.nodes[&p].parent.clone();
                }
                false
            })
            .cloned()
            .collect();
        let tombstoned: Vec<String> = subtree
            .iter()
            .filter(|n| !keep.contains(*n) && !edited.contains(n) && !retained.contains(*n))
            .cloned()
            .collect();
        let dead: HashSet<&String> = tombstoned.iter().collect();
        // Re-parent edited survivors to the nearest ancestor that stays.
        let mut upserts: Vec<String> = Vec::new();
        for n in &edited {
            let mut p = self.nodes[n].parent.clone();
            while let Some(c) = &p {
                if dead.contains(c) {
                    p = self.nodes[c].parent.clone();
                } else {
                    break;
                }
            }
            self.nodes.get_mut(n).unwrap().parent = p;
        }
        for n in subtree
            .iter()
            .filter(|n| keep.contains(*n) || edited.contains(n) || retained.contains(*n))
        {
            upserts.push(n.clone());
        }
        for n in upserts {
            self.push_upsert(&n);
            if edited.contains(&n) {
                // Stamp with the revision of the entry just pushed.
                let st = Stamp {
                    rev: self.rev,
                    by: ep.to_string(),
                };
                self.nodes.get_mut(&n).unwrap().place_st = st;
            }
        }
        // Tombstone deepest first so clients delete leaves before folders.
        for n in tombstoned.iter().rev() {
            let st = self.stamp(ep);
            self.nodes.get_mut(n).unwrap().deleted = Some(st);
            let op_id = if n == id {
                Some(op.op_id.as_str())
            } else {
                None
            };
            self.push(Some(ep), op_id, OpBody::Remove { id: n.clone() });
        }
        if keep.contains(id) {
            Err(RejectReason::ConflictLost)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal client mirror: applies server ops the way the extension
    /// will (add = upsert) and talks to the engine like a device.
    struct Client {
        ep: String,
        rev: u64,
        n: u32,
        nodes: BTreeMap<String, WireNode>,
    }

    impl Client {
        fn new(e: &mut Engine, ep: &str) -> Client {
            e.register_endpoint(ep);
            let mut c = Client {
                ep: ep.into(),
                rev: 0,
                n: 0,
                nodes: BTreeMap::new(),
            };
            c.load(e);
            c
        }
        fn load(&mut self, e: &Engine) {
            let t = e.tree();
            self.rev = t.rev;
            self.nodes = t.nodes.into_iter().map(|n| (n.id.clone(), n)).collect();
        }
        fn op(&mut self, body: OpBody) -> Op {
            self.n += 1;
            Op {
                op_id: format!("{}-{}", self.ep, self.n),
                body,
            }
        }
        fn sync(&mut self, e: &mut Engine, bodies: Vec<OpBody>) -> SyncResponse {
            let ops = bodies.into_iter().map(|b| self.op(b)).collect();
            self.send(e, ops)
        }
        fn send(&mut self, e: &mut Engine, ops: Vec<Op>) -> SyncResponse {
            let resp = e
                .sync(&SyncRequest {
                    endpoint_id: self.ep.clone(),
                    since_rev: self.rev,
                    ops: ops.clone(),
                })
                .unwrap();
            if resp.resync {
                self.load(e);
                return resp;
            }
            // Apply own ops locally first (the device already did), then
            // the server's changes.
            for op in &ops {
                self.apply(&op.body);
            }
            for op in &resp.ops {
                self.apply(&op.body);
            }
            self.rev = resp.rev;
            resp
        }
        fn apply(&mut self, b: &OpBody) {
            match b {
                OpBody::Add {
                    id,
                    parent,
                    position,
                    kind,
                    title,
                    url,
                } => {
                    self.nodes.insert(
                        id.clone(),
                        WireNode {
                            id: id.clone(),
                            parent: Some(parent.clone()),
                            position: position.clone(),
                            kind: *kind,
                            title: title.clone(),
                            url: url.clone(),
                        },
                    );
                }
                OpBody::Update { id, title, url } => {
                    if let Some(n) = self.nodes.get_mut(id) {
                        if let Some(t) = title {
                            n.title = t.clone();
                        }
                        if let Some(u) = url {
                            n.url = Some(u.clone());
                        }
                    }
                }
                OpBody::Move {
                    id,
                    parent,
                    position,
                } => {
                    if let Some(n) = self.nodes.get_mut(id) {
                        n.parent = Some(parent.clone());
                        n.position = position.clone();
                    }
                }
                OpBody::Remove { id } => {
                    // Like chrome.bookmarks.removeTree.
                    let mut stack = vec![id.clone()];
                    while let Some(cur) = stack.pop() {
                        self.nodes.remove(&cur);
                        stack.extend(
                            self.nodes
                                .values()
                                .filter(|n| n.parent.as_deref() == Some(cur.as_str()))
                                .map(|n| n.id.clone()),
                        );
                    }
                }
            }
        }
        /// Whether `node` is `anc` or inside it (local view).
        fn within(&self, node: &str, anc: &str) -> bool {
            let mut cur = Some(node.to_string());
            while let Some(c) = cur {
                if c == anc {
                    return true;
                }
                cur = self.nodes.get(&c).and_then(|n| n.parent.clone());
            }
            false
        }
        fn get(&self, id: &str) -> Option<&WireNode> {
            self.nodes.get(id)
        }
    }

    fn add_bm(id: &str, parent: &str, pos: &str, title: &str) -> OpBody {
        OpBody::Add {
            id: id.into(),
            parent: parent.into(),
            position: pos.into(),
            kind: Kind::Bookmark,
            title: title.into(),
            url: Some(format!("https://{id}.example")),
        }
    }
    fn add_folder(id: &str, parent: &str, pos: &str, title: &str) -> OpBody {
        OpBody::Add {
            id: id.into(),
            parent: parent.into(),
            position: pos.into(),
            kind: Kind::Folder,
            title: title.into(),
            url: None,
        }
    }
    fn rename(id: &str, title: &str) -> OpBody {
        OpBody::Update {
            id: id.into(),
            title: Some(title.into()),
            url: None,
        }
    }
    fn mv(id: &str, parent: &str, pos: &str) -> OpBody {
        OpBody::Move {
            id: id.into(),
            parent: parent.into(),
            position: pos.into(),
        }
    }
    fn rm(id: &str) -> OpBody {
        OpBody::Remove { id: id.into() }
    }

    /// Both devices pull until quiet and then must equal the canonical tree.
    fn assert_converged(e: &mut Engine, clients: &mut [&mut Client]) {
        let canon: BTreeMap<String, WireNode> = e
            .tree()
            .nodes
            .into_iter()
            .map(|n| (n.id.clone(), n))
            .collect();
        for c in clients.iter_mut() {
            c.sync(e, vec![]);
            let diff: Vec<String> = canon
                .keys()
                .chain(c.nodes.keys())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .filter(|k| canon.get(*k) != c.nodes.get(*k))
                .map(|k| format!("{k}: server={:?} client={:?}", canon.get(k), c.nodes.get(k)))
                .collect();
            assert!(
                diff.is_empty(),
                "client {} diverged:\n{}",
                c.ep,
                diff.join("\n")
            );
        }
    }

    #[test]
    fn starts_with_root_and_roles() {
        let e = Engine::new();
        let ids: Vec<String> = e.tree().nodes.into_iter().map(|n| n.id).collect();
        assert_eq!(ids, ["root", "toolbar", "other", "mobile"]);
    }

    #[test]
    fn add_is_delivered_to_the_other_device_but_not_echoed() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        let r = a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "One")]);
        assert!(r.ops.is_empty() && r.rejected.is_empty());
        let r = b.sync(&mut e, vec![]);
        assert_eq!(r.ops.len(), 1);
        assert_eq!(b.get("b1").unwrap().title, "One");
        assert_converged(&mut e, &mut [&mut a, &mut b]);
    }

    #[test]
    fn retried_request_is_idempotent() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let ops = vec![a.op(add_bm("b1", "toolbar", "V", "One"))];
        a.send(&mut e, ops.clone());
        let rev = e.rev();
        let r = e
            .sync(&SyncRequest {
                endpoint_id: "a".into(),
                since_rev: 0,
                ops,
            })
            .unwrap();
        assert_eq!(e.rev(), rev, "replay must not bump rev");
        assert!(r.rejected.is_empty());
    }

    #[test]
    fn unchanged_update_is_a_noop() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "One")]);
        let rev = e.rev();
        a.sync(&mut e, vec![rename("b1", "One")]);
        assert_eq!(e.rev(), rev);
    }

    #[test]
    fn same_field_last_write_wins_and_both_converge() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "One")]);
        b.sync(&mut e, vec![]);
        a.sync(&mut e, vec![rename("b1", "A")]);
        b.rev = 1; // b has not seen a's rename yet
        b.sync(&mut e, vec![rename("b1", "B")]);
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        assert_eq!(a.get("b1").unwrap().title, "B");
    }

    #[test]
    fn different_fields_both_apply() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "One")]);
        b.sync(&mut e, vec![]);
        let since = b.rev;
        a.sync(&mut e, vec![rename("b1", "A")]);
        b.rev = since;
        b.sync(
            &mut e,
            vec![OpBody::Update {
                id: "b1".into(),
                title: None,
                url: Some("https://new.example".into()),
            }],
        );
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        let n = a.get("b1").unwrap();
        assert_eq!(n.title, "A");
        assert_eq!(n.url.as_deref(), Some("https://new.example"));
    }

    #[test]
    fn edit_beats_concurrent_delete_stale_editor() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "One")]);
        b.sync(&mut e, vec![]);
        let since = b.rev;
        a.sync(&mut e, vec![rm("b1")]);
        b.rev = since; // b edits without having seen the delete
        let r = b.sync(&mut e, vec![rename("b1", "Edited")]);
        assert!(r.rejected.is_empty());
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        assert_eq!(a.get("b1").unwrap().title, "Edited");
    }

    #[test]
    fn edit_beats_concurrent_delete_stale_deleter() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "One")]);
        b.sync(&mut e, vec![]);
        let since = b.rev;
        a.sync(&mut e, vec![rename("b1", "Edited")]);
        b.rev = since;
        let r = b.sync(&mut e, vec![rm("b1")]);
        assert_eq!(r.rejected[0].reason, RejectReason::ConflictLost);
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        assert_eq!(b.get("b1").unwrap().title, "Edited");
    }

    #[test]
    fn delete_of_a_known_deleted_node_is_gone_for_edits() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "One")]);
        a.sync(&mut e, vec![rm("b1")]);
        let r = a.sync(&mut e, vec![rename("b1", "x")]);
        assert_eq!(r.rejected[0].reason, RejectReason::Gone);
    }

    #[test]
    fn delete_vs_delete_is_a_noop() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "One")]);
        b.sync(&mut e, vec![]);
        let since = b.rev;
        a.sync(&mut e, vec![rm("b1")]);
        b.rev = since;
        let r = b.sync(&mut e, vec![rm("b1")]);
        assert!(r.rejected.is_empty());
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        assert!(a.get("b1").is_none());
    }

    #[test]
    fn add_and_remove_in_one_request_do_not_conflict() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let r = a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "One"), rm("b1")]);
        assert!(r.rejected.is_empty());
        assert!(e.tree().nodes.iter().all(|n| n.id != "b1"));
    }

    #[test]
    fn move_into_deleted_folder_climbs_to_surviving_ancestor() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(
            &mut e,
            vec![
                add_folder("f1", "toolbar", "V", "F"),
                add_bm("b1", "toolbar", "W", "One"),
            ],
        );
        b.sync(&mut e, vec![]);
        let since = b.rev;
        a.sync(&mut e, vec![rm("f1")]);
        b.rev = since;
        b.sync(&mut e, vec![mv("b1", "f1", "V")]);
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        assert_eq!(a.get("b1").unwrap().parent.as_deref(), Some("toolbar"));
        assert!(a.get("f1").is_none());
    }

    #[test]
    fn new_child_in_deleted_folder_revives_only_the_folder() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(
            &mut e,
            vec![
                add_folder("f1", "toolbar", "V", "F"),
                add_bm("old", "f1", "V", "Old"),
            ],
        );
        b.sync(&mut e, vec![]);
        let since = b.rev;
        a.sync(&mut e, vec![rm("old"), rm("f1")]);
        b.rev = since;
        b.sync(&mut e, vec![add_bm("new", "f1", "V", "New")]);
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        assert_eq!(a.get("new").unwrap().parent.as_deref(), Some("f1"));
        assert!(a.get("f1").is_some());
        assert!(a.get("old").is_none());
    }

    #[test]
    fn deleting_a_folder_with_a_concurrently_added_child_keeps_the_folder() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(
            &mut e,
            vec![
                add_folder("f1", "toolbar", "V", "F"),
                add_bm("old", "f1", "V", "Old"),
            ],
        );
        b.sync(&mut e, vec![]);
        let since = b.rev;
        b.sync(&mut e, vec![add_bm("new", "f1", "W", "New")]);
        a.rev = since;
        let r = a.sync(&mut e, vec![rm("old"), rm("f1")]);
        assert_eq!(r.rejected.len(), 1);
        assert_eq!(r.rejected[0].reason, RejectReason::ConflictLost);
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        assert!(a.get("f1").is_some() && a.get("new").is_some());
        assert!(a.get("old").is_none());
    }

    #[test]
    fn edited_child_of_a_deleted_folder_survives_in_the_ancestor() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(
            &mut e,
            vec![
                add_folder("f1", "toolbar", "V", "F"),
                add_bm("c", "f1", "V", "Child"),
            ],
        );
        b.sync(&mut e, vec![]);
        let since = b.rev;
        b.sync(&mut e, vec![rename("c", "Edited")]);
        a.rev = since;
        a.sync(&mut e, vec![rm("c"), rm("f1")]);
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        assert_eq!(a.get("c").unwrap().title, "Edited");
        assert_eq!(a.get("c").unwrap().parent.as_deref(), Some("toolbar"));
        assert!(a.get("f1").is_none());
    }

    #[test]
    fn cycle_creating_move_is_rejected() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(
            &mut e,
            vec![
                add_folder("x", "toolbar", "V", "X"),
                add_folder("y", "toolbar", "W", "Y"),
            ],
        );
        b.sync(&mut e, vec![]);
        let since = b.rev;
        a.sync(&mut e, vec![mv("x", "y", "V")]);
        b.rev = since;
        let r = b.sync(&mut e, vec![mv("y", "x", "V")]);
        assert_eq!(r.rejected[0].reason, RejectReason::ConflictLost);
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        assert_eq!(b.get("x").unwrap().parent.as_deref(), Some("y"));
        assert_eq!(b.get("y").unwrap().parent.as_deref(), Some("toolbar"));
    }

    #[test]
    fn move_into_own_subtree_is_rejected() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        a.sync(
            &mut e,
            vec![
                add_folder("x", "toolbar", "V", "X"),
                add_folder("y", "x", "V", "Y"),
            ],
        );
        let r = a.sync(&mut e, vec![mv("x", "y", "V")]);
        assert_eq!(r.rejected[0].reason, RejectReason::ConflictLost);
        assert_eq!(a.get("x").unwrap().parent.as_deref(), Some("toolbar"));
    }

    #[test]
    fn concurrent_inserts_at_the_same_position_are_ordered_by_id() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "A")]);
        b.sync(&mut e, vec![add_bm("a1", "toolbar", "V", "B")]);
        assert_converged(&mut e, &mut [&mut a, &mut b]);
        let order: Vec<String> = e
            .tree()
            .nodes
            .into_iter()
            .filter(|n| n.parent.as_deref() == Some("toolbar"))
            .map(|n| n.id)
            .collect();
        assert_eq!(order, ["a1", "b1"]);
    }

    #[test]
    fn invalid_ops_are_rejected_without_effect() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let rev = e.rev();
        let bad = vec![
            // bad url
            OpBody::Add {
                id: "b1".into(),
                parent: "toolbar".into(),
                position: "V".into(),
                kind: Kind::Bookmark,
                title: "t".into(),
                url: Some("nope".into()),
            },
            // bookmark without url
            OpBody::Add {
                id: "b2".into(),
                parent: "toolbar".into(),
                position: "V".into(),
                kind: Kind::Bookmark,
                title: "t".into(),
                url: None,
            },
            // invalid position (trailing zero)
            add_bm("b3", "toolbar", "V0", "t"),
            // unknown parent
            add_bm("b4", "missing", "V", "t"),
            // roots are fixed
            rm("toolbar"),
            mv("toolbar", "other", "V"),
        ];
        let r = a.sync(&mut e, bad);
        assert_eq!(r.rejected.len(), 6);
        assert!(r.rejected.iter().all(|x| x.reason == RejectReason::Invalid));
        assert_eq!(e.rev(), rev);
    }

    #[test]
    fn oversized_fields_and_full_trees_are_rejected() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let long = "x".repeat(MAX_FIELD_BYTES + 1);
        let r = a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", &long)]);
        assert_eq!(r.rejected[0].reason, RejectReason::Invalid);
        let r = a.sync(&mut e, vec![add_bm(&long, "toolbar", "V", "t")]);
        assert_eq!(r.rejected[0].reason, RejectReason::Invalid);
        // Fill the engine up to the limit with cheap direct inserts.
        let template = e.nodes["toolbar"].clone();
        for i in e.nodes.len()..MAX_NODES {
            let mut n = template.clone();
            n.id = format!("fill{i}");
            e.nodes.insert(n.id.clone(), n);
        }
        let r = a.sync(&mut e, vec![add_bm("one-too-many", "toolbar", "V", "t")]);
        assert_eq!(r.rejected[0].reason, RejectReason::Invalid);
    }

    #[test]
    fn rejected_root_removal_is_cheap_on_a_big_tree() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let template = e.nodes["toolbar"].clone();
        for i in 0..50_000 {
            let mut n = template.clone();
            n.id = format!("fill{i}");
            e.nodes.insert(n.id.clone(), n);
        }
        let t = std::time::Instant::now();
        let r = a.sync(&mut e, vec![rm("root")]);
        assert_eq!(r.rejected[0].reason, RejectReason::Invalid);
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn too_many_ops_are_refused() {
        let mut e = Engine::new();
        e.register_endpoint("a");
        let ops = (0..=MAX_OPS_PER_REQUEST)
            .map(|i| Op {
                op_id: i.to_string(),
                body: rm("x"),
            })
            .collect();
        let err = e
            .sync(&SyncRequest {
                endpoint_id: "a".into(),
                since_rev: 0,
                ops,
            })
            .unwrap_err();
        assert_eq!(err, SyncError::TooManyOps { max: 500 });
    }

    #[test]
    fn unknown_endpoint_stale_cursor_and_pruned_log_require_resync() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let req = |ep: &str, since| SyncRequest {
            endpoint_id: ep.into(),
            since_rev: since,
            ops: vec![],
        };
        assert!(e.sync(&req("ghost", 0)).unwrap().resync);
        assert!(e.sync(&req("a", 99)).unwrap().resync);
        a.sync(&mut e, vec![add_bm("b1", "toolbar", "V", "One")]);
        a.sync(&mut e, vec![rm("b1")]);
        e.prune(e.rev());
        assert!(e.sync(&req("a", 0)).unwrap().resync);
        assert!(!e.sync(&req("a", e.rev())).unwrap().resync);
        // Tombstones are gone after pruning: the id is unknown again.
        let r = a.sync(&mut e, vec![rename("b1", "x")]);
        assert_eq!(r.rejected[0].reason, RejectReason::Gone);
    }

    #[test]
    fn replacing_the_tree_makes_devices_resync_and_keeps_them_registered() {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        a.sync(
            &mut e,
            vec![
                add_bm("b1", "toolbar", "V", "One"),
                add_bm("b2", "toolbar", "W", "Two"),
            ],
        );
        let mut keep = e.tree().nodes;
        keep.retain(|n| n.id != "b2");
        e.replace_tree(&keep).unwrap();
        let req = SyncRequest {
            endpoint_id: "a".into(),
            since_rev: a.rev,
            ops: vec![],
        };
        assert!(e.sync(&req).unwrap().resync);
        // After the full tree is taken over, syncing works again.
        let tree = e.tree();
        assert!(tree.nodes.iter().any(|n| n.id == "b1"));
        assert!(!tree.nodes.iter().any(|n| n.id == "b2"));
        let req = SyncRequest {
            since_rev: tree.rev,
            ..req
        };
        assert!(!e.sync(&req).unwrap().resync);
    }

    #[test]
    fn a_broken_replacement_is_refused_and_changes_nothing() {
        let mut e = Engine::new();
        let mut nodes = e.tree().nodes;
        let before = e.rev();
        nodes.push(WireNode {
            id: "x".into(),
            parent: Some("ghost".into()),
            position: "V".into(),
            kind: Kind::Folder,
            title: "x".into(),
            url: None,
        });
        assert!(e.replace_tree(&nodes).is_err());
        nodes.pop();
        nodes.retain(|n| n.id != "toolbar");
        assert!(e.replace_tree(&nodes).is_err());
        assert_eq!(e.rev(), before);
    }

    #[test]
    fn removed_endpoint_must_resync() {
        let mut e = Engine::new();
        e.register_endpoint("a");
        e.remove_endpoint("a");
        let r = e
            .sync(&SyncRequest {
                endpoint_id: "a".into(),
                since_rev: 0,
                ops: vec![],
            })
            .unwrap();
        assert!(r.resync);
    }

    #[test]
    fn pseudo_random_interleavings_converge() {
        for seed in 1..=40u64 {
            run_random(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        }
    }

    fn run_random(seed: u64) {
        let mut e = Engine::new();
        let mut a = Client::new(&mut e, "a");
        let mut b = Client::new(&mut e, "b");
        let mut seed = seed;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut counter = 0u32;
        for _ in 0..400 {
            let who_a = next() % 2 == 0;
            let c = if who_a { &mut a } else { &mut b };
            // Sometimes act on a stale view, sometimes pull first.
            if next() % 3 == 0 {
                c.sync(&mut e, vec![]);
            }
            // A browser only lets the user act on legal targets.
            let live: Vec<String> = c
                .nodes
                .keys()
                .filter(|k| !Engine::is_protected(k))
                .cloned()
                .collect();
            let folders: Vec<String> = c
                .nodes
                .values()
                .filter(|n| n.kind == Kind::Folder)
                .map(|n| n.id.clone())
                .collect();
            let pick = |v: &Vec<String>, n: u64| v[(n as usize) % v.len()].clone();
            let body = match next() % 5 {
                0 => {
                    counter += 1;
                    add_bm(&format!("n{counter}"), &pick(&folders, next()), "V", "t")
                }
                1 => {
                    counter += 1;
                    add_folder(&format!("n{counter}"), &pick(&folders, next()), "W", "f")
                }
                _ if live.is_empty() => continue,
                2 => rename(&pick(&live, next()), &format!("t{}", next() % 100)),
                3 => {
                    let (n, p) = (pick(&live, next()), pick(&folders, next()));
                    if c.within(&p, &n) {
                        continue;
                    }
                    mv(&n, &p, "X")
                }
                _ => rm(&pick(&live, next())),
            };
            c.sync(&mut e, vec![body]);
            // Whatever happened, the canonical tree must be well formed.
            let ids: HashSet<String> = e.tree().nodes.iter().map(|n| n.id.clone()).collect();
            for n in e.tree().nodes {
                if let Some(p) = &n.parent {
                    assert!(ids.contains(p), "orphan {}", n.id);
                }
            }
        }
        assert_converged(&mut e, &mut [&mut a, &mut b]);
    }
}
