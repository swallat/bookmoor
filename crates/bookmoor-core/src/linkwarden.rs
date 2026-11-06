//! Two-way reconciliation between a bookmark tree and the collections and
//! links of one Linkwarden collection. See `docs/adapters/linkwarden.md`.
//!
//! This module is pure: it compares what each side looks like now with what
//! both looked like after the last run (`Pair`) and says what to change on
//! which side. Talking to Linkwarden and to the canonical tree is the
//! server's job.
//!
//! * A folder is a collection, a bookmark is a link. The folder the sync
//!   starts at (`root`) is the Linkwarden collection it is bound to.
//! * Every field (title, URL, parent) is merged on its own: the side that
//!   changed it since the last run wins; when both did, Bookmoor wins.
//! * An edit wins over a delete, like everywhere in Bookmoor: an item deleted
//!   on one side is only deleted on the other if nothing in it changed there.
//! * Order is not synced (Linkwarden has none). New items from Linkwarden are
//!   appended.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A thing on the Linkwarden side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "lowercase")]
pub enum Ref {
    Collection(i64),
    Link(i64),
}

impl Ref {
    pub fn is_folder(self) -> bool {
        matches!(self, Ref::Collection(_))
    }
}

/// Which way changes travel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Both ways; a field changed on both sides takes Bookmoor's value.
    #[default]
    Both,
    /// Bookmoor to Linkwarden only: Linkwarden is made to match for everything
    /// that is paired; things only Linkwarden has are left alone.
    Push,
    /// Linkwarden to Bookmoor only: Bookmoor is made to match for everything
    /// that is paired; things only Bookmoor has are left alone.
    Pull,
}

/// A node of the local tree below the sync root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LNode {
    pub parent: String,
    pub folder: bool,
    pub title: String,
    pub url: Option<String>,
    pub position: String,
}

/// An item of the Linkwarden collection tree below the bound collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RItem {
    /// Id of the collection it is in (the bound collection for the top level).
    pub parent: i64,
    pub title: String,
    pub url: Option<String>,
}

/// What both sides agreed on after the last run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pair {
    pub node: String,
    pub remote: Ref,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Node id of the parent (the sync root's id for the top level).
    pub parent: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAction {
    Create {
        node: String,
        parent: String,
        folder: bool,
        title: String,
        url: Option<String>,
    },
    Update {
        node: String,
        title: Option<String>,
        url: Option<String>,
        parent: Option<String>,
    },
    /// The item paired with this node.
    Delete { node: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalAction {
    Add {
        node: String,
        remote: Ref,
        parent: String,
        folder: bool,
        title: String,
        url: Option<String>,
    },
    Update {
        node: String,
        title: Option<String>,
        url: Option<String>,
    },
    Move {
        node: String,
        parent: String,
    },
    Remove {
        node: String,
    },
}

/// The state of a pair after the run, if all its actions succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub remote: Option<Ref>,
    pub title: String,
    pub url: Option<String>,
    pub parent: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub remote: Vec<RemoteAction>,
    pub local: Vec<LocalAction>,
    /// Pairs as they should be after the run, by local node id.
    pub desired: BTreeMap<String, Desired>,
}

impl Plan {
    /// Items that would be deleted on either side.
    pub fn deletions(&self) -> usize {
        self.remote
            .iter()
            .filter(|a| matches!(a, RemoteAction::Delete { .. }))
            .count()
            + self
                .local
                .iter()
                .filter(|a| matches!(a, LocalAction::Remove { .. }))
                .count()
    }

    pub fn is_empty(&self) -> bool {
        self.remote.is_empty() && self.local.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Winner {
    Local,
    Remote,
}

fn winner(mode: Mode, local_changed: bool) -> Winner {
    match mode {
        Mode::Push => Winner::Local,
        Mode::Pull => Winner::Remote,
        Mode::Both => {
            if local_changed {
                Winner::Local
            } else {
                Winner::Remote
            }
        }
    }
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

struct Ctx<'a> {
    root: &'a str,
    root_coll: i64,
    local: &'a BTreeMap<String, LNode>,
    remote: &'a BTreeMap<Ref, RItem>,
    /// Node of each paired remote item (local side may be gone).
    node_of: BTreeMap<Ref, String>,
    pair_of: BTreeMap<String, Pair>,
    /// Remote items that get a new local node because their pair is recreated.
    recreated: BTreeMap<Ref, String>,
    kids_local: BTreeMap<&'a str, Vec<&'a str>>,
    kids_remote: BTreeMap<i64, Vec<Ref>>,
}

impl<'a> Ctx<'a> {
    /// The node the remote parent collection stands for after this run.
    fn node_of_coll(&self, coll: i64) -> Option<String> {
        if coll == self.root_coll {
            return Some(self.root.to_string());
        }
        let r = Ref::Collection(coll);
        if let Some(n) = self.recreated.get(&r) {
            return Some(n.clone());
        }
        let n = self.node_of.get(&r)?;
        // A pair whose local side is gone only counts if it is recreated.
        self.local.contains_key(n).then(|| n.clone())
    }

    fn local_changed(p: &Pair, l: &LNode) -> bool {
        l.title != p.title || l.url != p.url || l.parent != p.parent
    }

    /// Something in the remote item (or below it) is new or changed.
    fn remote_dirty(&self, key: Ref, depth: usize) -> bool {
        if depth > 64 {
            return true;
        }
        let Some(r) = self.remote.get(&key) else {
            return false;
        };
        match self.node_of.get(&key).and_then(|n| self.pair_of.get(n)) {
            None => return true,
            Some(p) => {
                if self.remote_changed_plain(p, r) {
                    return true;
                }
            }
        }
        if let Ref::Collection(c) = key {
            for k in self.kids_remote.get(&c).into_iter().flatten() {
                if self.remote_dirty(*k, depth + 1) {
                    return true;
                }
            }
        }
        false
    }

    /// Like `remote_changed` but not looking at the parent's local mapping
    /// (used while that mapping is still being decided).
    fn remote_changed_plain(&self, p: &Pair, r: &RItem) -> bool {
        if r.title != p.title || r.url != p.url {
            return true;
        }
        // Parent: unchanged when the parent collection is the paired one.
        let parent_node = if r.parent == self.root_coll {
            Some(self.root.to_string())
        } else {
            self.node_of.get(&Ref::Collection(r.parent)).cloned()
        };
        parent_node.as_deref() != Some(p.parent.as_str())
    }

    fn local_dirty(&self, node: &str, depth: usize) -> bool {
        if depth > 64 {
            return true;
        }
        let Some(l) = self.local.get(node) else {
            return false;
        };
        match self.pair_of.get(node) {
            None => return true,
            Some(p) => {
                // A pair whose remote item is gone cannot tell; counts as clean.
                if Self::local_changed(p, l) {
                    return true;
                }
            }
        }
        if l.folder {
            for k in self.kids_local.get(node).into_iter().flatten() {
                if self.local_dirty(k, depth + 1) {
                    return true;
                }
            }
        }
        false
    }
}

fn depth_local(local: &BTreeMap<String, LNode>, root: &str, node: &str) -> usize {
    let mut d = 0;
    let mut cur = node;
    while cur != root && d < 1000 {
        match local.get(cur) {
            Some(l) => cur = &l.parent,
            None => break,
        }
        d += 1;
    }
    d
}

fn depth_remote(remote: &BTreeMap<Ref, RItem>, root: i64, key: Ref) -> usize {
    let mut d = 0;
    let mut cur = key;
    while d < 1000 {
        let Some(r) = remote.get(&cur) else { break };
        d += 1;
        if r.parent == root {
            break;
        }
        cur = Ref::Collection(r.parent);
    }
    d
}

/// Work out what to change. `local` holds the nodes below `root` (not `root`
/// itself), `remote` everything below the bound collection `root_coll`.
/// `new_id` makes ids for local nodes that are created.
pub fn plan(
    root: &str,
    root_coll: i64,
    local: &BTreeMap<String, LNode>,
    remote: &BTreeMap<Ref, RItem>,
    pairs: &[Pair],
    mode: Mode,
    new_id: &mut dyn FnMut() -> String,
) -> Plan {
    let mut out = Plan::default();

    // Pairs with neither side left just end.
    let mut pair_of: BTreeMap<String, Pair> = BTreeMap::new();
    let mut node_of: BTreeMap<Ref, String> = BTreeMap::new();
    for p in pairs {
        if !local.contains_key(&p.node) && !remote.contains_key(&p.remote) {
            continue;
        }
        // A pair whose kind no longer fits (should not happen) is dropped.
        if local
            .get(&p.node)
            .is_some_and(|l| l.folder != p.remote.is_folder())
        {
            continue;
        }
        node_of.insert(p.remote, p.node.clone());
        pair_of.insert(p.node.clone(), p.clone());
    }

    let mut kids_local: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, l) in local {
        kids_local.entry(l.parent.as_str()).or_default().push(id);
    }
    let mut kids_remote: BTreeMap<i64, Vec<Ref>> = BTreeMap::new();
    for (k, r) in remote {
        kids_remote.entry(r.parent).or_default().push(*k);
    }

    let mut ctx = Ctx {
        root,
        root_coll,
        local,
        remote,
        node_of,
        pair_of,
        recreated: BTreeMap::new(),
        kids_local,
        kids_remote,
    };

    // 1. Items that only one side has, matched by place and name on the
    // first run (or after a pair was lost) so nothing is doubled.
    let mut unpaired_local: Vec<&str> = local
        .keys()
        .filter(|n| !ctx.pair_of.contains_key(*n))
        .map(String::as_str)
        .collect();
    unpaired_local.sort_by_key(|n| (depth_local(local, root, n), &local[*n].position, *n));
    let mut unpaired_remote: BTreeSet<Ref> = remote
        .keys()
        .filter(|k| !ctx.node_of.contains_key(*k))
        .copied()
        .collect();
    for n in &unpaired_local {
        let l = &local[*n];
        // Parent must already be on both sides.
        let parent_coll = if l.parent == root {
            Some(root_coll)
        } else {
            ctx.pair_of.get(&l.parent).and_then(|p| match p.remote {
                Ref::Collection(c) => Some(c),
                _ => None,
            })
        };
        let Some(pc) = parent_coll else { continue };
        let hit = unpaired_remote.iter().copied().find(|k| {
            let r = &remote[k];
            k.is_folder() == l.folder
                && r.parent == pc
                && if l.folder {
                    norm(&r.title) == norm(&l.title)
                } else {
                    r.url.as_deref().map(str::trim) == l.url.as_deref().map(str::trim)
                }
        });
        if let Some(k) = hit {
            unpaired_remote.remove(&k);
            // Linkwarden's spelling is the starting point, so a difference in
            // case or spacing counts as a local edit (or a remote one in Pull).
            let p = Pair {
                node: n.to_string(),
                remote: k,
                title: remote[&k].title.clone(),
                url: l.url.clone(),
                parent: l.parent.clone(),
            };
            ctx.pair_of.insert(p.node.clone(), p);
            ctx.node_of.insert(k, n.to_string());
        }
    }
    let unpaired_local: Vec<&str> = unpaired_local
        .into_iter()
        .filter(|n| !ctx.pair_of.contains_key(*n))
        .collect();

    // 2. Pairs with one side gone: an edit on the other side wins over the
    // delete, otherwise the delete is passed on.
    let mut remote_deletes: Vec<String> = Vec::new();
    let mut recreate_remote: BTreeSet<String> = BTreeSet::new();
    for (node, p) in ctx.pair_of.clone() {
        match (local.get(&node), remote.get(&p.remote)) {
            (None, Some(_)) => {
                let keep = match mode {
                    Mode::Push => false,
                    Mode::Pull => true,
                    Mode::Both => ctx.remote_dirty(p.remote, 0),
                };
                if keep {
                    // A new local node stands in for the one that was deleted.
                    ctx.recreated.insert(p.remote, new_id());
                } else {
                    remote_deletes.push(node);
                }
            }
            (Some(_), None) => {
                let keep = match mode {
                    Mode::Push => true,
                    Mode::Pull => false,
                    Mode::Both => ctx.local_dirty(&node, 0),
                };
                if keep {
                    recreate_remote.insert(node);
                } else {
                    out.local.push(LocalAction::Remove { node });
                }
            }
            _ => {}
        }
    }
    // A node created again on the remote side starts like an unpaired one.
    for n in &recreate_remote {
        ctx.pair_of.remove(n);
    }
    for node in remote_deletes {
        out.remote.push(RemoteAction::Delete { node });
    }

    // 3. Pairs present on both sides: merge field by field.
    for (node, p) in ctx.pair_of.clone() {
        let (Some(l), Some(r)) = (local.get(&node), remote.get(&p.remote)) else {
            continue;
        };
        let mut d = Desired {
            remote: Some(p.remote),
            title: p.title.clone(),
            url: p.url.clone(),
            parent: p.parent.clone(),
        };
        let mut r_upd = (None::<String>, None::<String>, None::<String>);
        let mut l_upd = (None::<String>, None::<String>);
        let mut l_move = None::<String>;

        // title
        if l.title == r.title {
            d.title = l.title.clone();
        } else if winner(mode, l.title != p.title) == Winner::Local {
            d.title = l.title.clone();
            r_upd.0 = Some(l.title.clone());
        } else {
            d.title = r.title.clone();
            l_upd.0 = Some(r.title.clone());
        }
        // url
        if !l.folder {
            if l.url == r.url {
                d.url = l.url.clone();
            } else if winner(mode, l.url != p.url) == Winner::Local {
                d.url = l.url.clone();
                r_upd.1 = l.url.clone();
            } else {
                d.url = r.url.clone();
                l_upd.1 = r.url.clone();
            }
        }
        // parent
        let rp = ctx.node_of_coll(r.parent);
        if rp.as_deref() == Some(l.parent.as_str()) {
            d.parent = l.parent.clone();
        } else if rp.is_none() {
            // Remote parent unknown (not taken over): only Bookmoor can move it.
            d.parent = l.parent.clone();
            if mode != Mode::Pull {
                r_upd.2 = Some(l.parent.clone());
            }
        } else if winner(mode, l.parent != p.parent) == Winner::Local {
            d.parent = l.parent.clone();
            r_upd.2 = Some(l.parent.clone());
        } else {
            let np = rp.unwrap();
            d.parent = np.clone();
            l_move = Some(np);
        }

        if r_upd != (None, None, None) {
            out.remote.push(RemoteAction::Update {
                node: node.clone(),
                title: r_upd.0,
                url: r_upd.1,
                parent: r_upd.2,
            });
        }
        if l_upd != (None, None) {
            out.local.push(LocalAction::Update {
                node: node.clone(),
                title: l_upd.0,
                url: l_upd.1,
            });
        }
        if let Some(parent) = l_move {
            out.local.push(LocalAction::Move {
                node: node.clone(),
                parent,
            });
        }
        out.desired.insert(node, d);
    }

    // 4. Things only one side has.
    let creates_remote: Vec<&str> = unpaired_local
        .iter()
        .copied()
        .chain(recreate_remote.iter().map(String::as_str))
        .collect();
    if mode != Mode::Pull {
        let mut list: Vec<&str> = creates_remote;
        list.sort_by_key(|n| (depth_local(local, root, n), &local[*n].position, *n));
        for n in list {
            let l = &local[n];
            out.remote.push(RemoteAction::Create {
                node: n.to_string(),
                parent: l.parent.clone(),
                folder: l.folder,
                title: l.title.clone(),
                url: l.url.clone(),
            });
            out.desired.insert(
                n.to_string(),
                Desired {
                    remote: None,
                    title: l.title.clone(),
                    url: l.url.clone(),
                    parent: l.parent.clone(),
                },
            );
        }
    }
    if mode != Mode::Push {
        // Everything Linkwarden has that no local node stands for: new items
        // and items kept alive against a local delete.
        let mut adds: Vec<Ref> = unpaired_remote.iter().copied().collect();
        adds.extend(ctx.recreated.keys().copied());
        adds.sort_by_key(|k| (depth_remote(remote, root_coll, *k), *k));
        for k in adds {
            let r = &remote[&k];
            let node = match ctx.recreated.get(&k) {
                Some(n) => n.clone(),
                None => {
                    let n = new_id();
                    ctx.recreated.insert(k, n.clone());
                    n
                }
            };
            let Some(parent) = ctx.node_of_coll(r.parent) else {
                // Its collection was not taken over (cannot happen in Both/Pull).
                ctx.recreated.remove(&k);
                continue;
            };
            out.local.push(LocalAction::Add {
                node: node.clone(),
                remote: k,
                parent: parent.clone(),
                folder: k.is_folder(),
                title: r.title.clone(),
                url: r.url.clone(),
            });
            out.desired.insert(
                node,
                Desired {
                    remote: Some(k),
                    title: r.title.clone(),
                    url: r.url.clone(),
                    parent,
                },
            );
        }
    }

    // Order: creates parent-first, removals child-first and after the rest.
    out.remote.sort_by_key(|a| match a {
        RemoteAction::Create { node, .. } => (0, depth_local(local, root, node)),
        RemoteAction::Update { .. } => (1, 0),
        RemoteAction::Delete { node } => {
            // Links first, then collections from the bottom up.
            let folder = ctx.pair_of.get(node).is_some_and(|p| p.remote.is_folder());
            let d = ctx
                .pair_of
                .get(node)
                .map(|p| depth_remote(remote, root_coll, p.remote))
                .unwrap_or(0);
            (2 + folder as usize, usize::MAX - d)
        }
    });
    out.local.sort_by_key(|a| match a {
        LocalAction::Add { remote: k, .. } => (0, depth_remote(remote, root_coll, *k)),
        LocalAction::Update { .. } => (1, 0),
        LocalAction::Move { .. } => (2, 0),
        LocalAction::Remove { node } => (3, usize::MAX - depth_local(local, root, node)),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "root";
    const RC: i64 = 1;

    /// Both sides in memory, with the executor the server implements.
    struct Sim {
        local: BTreeMap<String, LNode>,
        remote: BTreeMap<Ref, RItem>,
        pairs: Vec<Pair>,
        next: i64,
        seq: u32,
    }

    impl Sim {
        fn new() -> Sim {
            Sim {
                local: BTreeMap::new(),
                remote: BTreeMap::new(),
                pairs: Vec::new(),
                next: 100,
                seq: 0,
            }
        }

        fn lfolder(&mut self, id: &str, parent: &str, title: &str) {
            self.ladd(id, parent, true, title, None);
        }
        fn lmark(&mut self, id: &str, parent: &str, title: &str, url: &str) {
            self.ladd(id, parent, false, title, Some(url));
        }
        fn ladd(&mut self, id: &str, parent: &str, folder: bool, title: &str, url: Option<&str>) {
            self.seq += 1;
            self.local.insert(
                id.into(),
                LNode {
                    parent: parent.into(),
                    folder,
                    title: title.into(),
                    url: url.map(Into::into),
                    position: format!("{:05}", self.seq),
                },
            );
        }
        fn rfolder(&mut self, id: i64, parent: i64, title: &str) {
            self.remote.insert(
                Ref::Collection(id),
                RItem {
                    parent,
                    title: title.into(),
                    url: None,
                },
            );
        }
        fn rlink(&mut self, id: i64, parent: i64, title: &str, url: &str) {
            self.remote.insert(
                Ref::Link(id),
                RItem {
                    parent,
                    title: title.into(),
                    url: Some(url.into()),
                },
            );
        }

        fn plan(&mut self, mode: Mode) -> Plan {
            let mut n = 0;
            let mut ids = || {
                n += 1;
                format!("new{n}")
            };
            plan(
                ROOT,
                RC,
                &self.local,
                &self.remote,
                &self.pairs,
                mode,
                &mut ids,
            )
        }

        fn run(&mut self, mode: Mode) -> Plan {
            let pl = self.plan(mode);
            let mut refs: BTreeMap<String, Ref> = self
                .pairs
                .iter()
                .map(|p| (p.node.clone(), p.remote))
                .collect();
            for (n, d) in &pl.desired {
                if let Some(r) = d.remote {
                    refs.insert(n.clone(), r);
                }
            }
            let coll = |refs: &BTreeMap<String, Ref>, node: &str| -> i64 {
                if node == ROOT {
                    RC
                } else {
                    match refs[node] {
                        Ref::Collection(c) => c,
                        _ => panic!("not a folder"),
                    }
                }
            };
            for a in &pl.remote {
                match a {
                    RemoteAction::Create {
                        node,
                        parent,
                        folder,
                        title,
                        url,
                    } => {
                        self.next += 1;
                        let r = if *folder {
                            Ref::Collection(self.next)
                        } else {
                            Ref::Link(self.next)
                        };
                        self.remote.insert(
                            r,
                            RItem {
                                parent: coll(&refs, parent),
                                title: title.clone(),
                                url: url.clone(),
                            },
                        );
                        refs.insert(node.clone(), r);
                    }
                    RemoteAction::Update {
                        node,
                        title,
                        url,
                        parent,
                    } => {
                        let r = refs[node];
                        let p = parent.as_ref().map(|p| coll(&refs, p));
                        let it = self.remote.get_mut(&r).unwrap();
                        if let Some(t) = title {
                            it.title = t.clone();
                        }
                        if let Some(u) = url {
                            it.url = Some(u.clone());
                        }
                        if let Some(p) = p {
                            it.parent = p;
                        }
                    }
                    RemoteAction::Delete { node } => {
                        let r = refs[node];
                        self.remote.remove(&r);
                        if let Ref::Collection(c) = r {
                            // Linkwarden removes what is inside.
                            let inner: Vec<Ref> = self
                                .remote
                                .iter()
                                .filter(|(_, v)| v.parent == c)
                                .map(|(k, _)| *k)
                                .collect();
                            for k in inner {
                                self.remote.remove(&k);
                            }
                        }
                    }
                }
            }
            for a in &pl.local {
                match a {
                    LocalAction::Add {
                        node,
                        remote,
                        parent,
                        folder,
                        title,
                        url,
                    } => {
                        self.ladd(node, parent, *folder, title, url.as_deref());
                        refs.insert(node.clone(), *remote);
                    }
                    LocalAction::Update { node, title, url } => {
                        let l = self.local.get_mut(node).unwrap();
                        if let Some(t) = title {
                            l.title = t.clone();
                        }
                        if let Some(u) = url {
                            l.url = Some(u.clone());
                        }
                    }
                    LocalAction::Move { node, parent } => {
                        self.local.get_mut(node).unwrap().parent = parent.clone();
                    }
                    LocalAction::Remove { node } => {
                        let mut stack = vec![node.clone()];
                        while let Some(c) = stack.pop() {
                            self.local.remove(&c);
                            stack.extend(
                                self.local
                                    .iter()
                                    .filter(|(_, v)| v.parent == c)
                                    .map(|(k, _)| k.clone()),
                            );
                        }
                    }
                }
            }
            self.pairs = pl
                .desired
                .iter()
                .map(|(n, d)| Pair {
                    node: n.clone(),
                    remote: d.remote.unwrap_or_else(|| refs[n]),
                    title: d.title.clone(),
                    url: d.url.clone(),
                    parent: d.parent.clone(),
                })
                .collect();
            pl
        }

        /// Run until nothing is left to do; the second run must be empty.
        fn settle(&mut self, mode: Mode) {
            self.run(mode);
            let again = self.plan(mode);
            assert!(again.is_empty(), "not settled: {again:?}");
        }

        fn titles(&self) -> Vec<String> {
            let mut v: Vec<String> = self.local.values().map(|l| l.title.clone()).collect();
            v.sort();
            v
        }
        fn rtitles(&self) -> Vec<String> {
            let mut v: Vec<String> = self.remote.values().map(|l| l.title.clone()).collect();
            v.sort();
            v
        }
    }

    fn base() -> Sim {
        let mut s = Sim::new();
        s.lfolder("f1", ROOT, "Dev");
        s.lmark("b1", "f1", "Rust", "https://rust-lang.org");
        s.lmark("b2", ROOT, "Home", "https://example.com");
        s.settle(Mode::Both);
        s
    }

    #[test]
    fn first_run_creates_everything_on_the_remote_side() {
        let s = base();
        assert_eq!(s.remote.len(), 3);
        assert_eq!(s.pairs.len(), 3);
        assert_eq!(s.rtitles(), vec!["Dev", "Home", "Rust"]);
    }

    #[test]
    fn first_run_matches_what_both_sides_have() {
        let mut s = Sim::new();
        s.lfolder("f1", ROOT, "Dev");
        s.lmark("b1", "f1", "Rust", "https://rust-lang.org");
        s.lmark("b3", "f1", "Only here", "https://a.example");
        s.rfolder(10, RC, "dev");
        s.rlink(11, 10, "The Rust site", "https://rust-lang.org");
        s.rlink(12, 10, "Only there", "https://b.example");
        s.settle(Mode::Both);
        // nothing doubled: the folder and the shared link are one each
        assert_eq!(s.local.values().filter(|l| l.folder).count(), 1);
        assert_eq!(s.remote.len(), 4);
        assert_eq!(s.local.len(), 4);
        assert!(s.titles().contains(&"Only there".to_string()));
        assert!(s.rtitles().contains(&"Only here".to_string()));
    }

    #[test]
    fn edits_travel_to_the_other_side() {
        let mut s = base();
        s.local.get_mut("b1").unwrap().title = "Rust lang".into();
        let rid = s.pairs.iter().find(|p| p.node == "b2").unwrap().remote;
        s.remote.get_mut(&rid).unwrap().url = Some("https://example.org".into());
        s.settle(Mode::Both);
        assert!(s.rtitles().contains(&"Rust lang".to_string()));
        assert_eq!(s.local["b2"].url.as_deref(), Some("https://example.org"));
    }

    #[test]
    fn both_changed_the_same_field_bookmoor_wins() {
        let mut s = base();
        s.local.get_mut("b2").unwrap().title = "Mine".into();
        let rid = s.pairs.iter().find(|p| p.node == "b2").unwrap().remote;
        s.remote.get_mut(&rid).unwrap().title = "Theirs".into();
        s.settle(Mode::Both);
        assert_eq!(s.local["b2"].title, "Mine");
        assert_eq!(s.remote[&rid].title, "Mine");
    }

    #[test]
    fn different_fields_merge() {
        let mut s = base();
        s.local.get_mut("b2").unwrap().title = "Mine".into();
        let rid = s.pairs.iter().find(|p| p.node == "b2").unwrap().remote;
        s.remote.get_mut(&rid).unwrap().url = Some("https://example.net".into());
        s.settle(Mode::Both);
        assert_eq!(s.local["b2"].title, "Mine");
        assert_eq!(s.local["b2"].url.as_deref(), Some("https://example.net"));
        assert_eq!(s.remote[&rid].title, "Mine");
    }

    #[test]
    fn new_items_and_moves_travel() {
        let mut s = base();
        s.lmark("b9", "f1", "New local", "https://n.example");
        s.rlink(500, RC, "New remote", "https://r.example");
        // move b2 into the folder on the Linkwarden side
        let b2 = s.pairs.iter().find(|p| p.node == "b2").unwrap().remote;
        let f1 = s.pairs.iter().find(|p| p.node == "f1").unwrap().remote;
        let Ref::Collection(fc) = f1 else { panic!() };
        s.remote.get_mut(&b2).unwrap().parent = fc;
        s.settle(Mode::Both);
        assert_eq!(s.local["b2"].parent, "f1");
        assert!(s.rtitles().contains(&"New local".to_string()));
        assert!(s.titles().contains(&"New remote".to_string()));
    }

    #[test]
    fn a_delete_is_passed_on_when_nothing_changed() {
        let mut s = base();
        s.local.remove("b1");
        let home = s.pairs.iter().find(|p| p.node == "b2").unwrap().remote;
        s.remote.remove(&home);
        s.settle(Mode::Both);
        assert_eq!(s.rtitles(), vec!["Dev"]);
        assert_eq!(s.titles(), vec!["Dev"]);
    }

    #[test]
    fn an_edit_beats_a_delete() {
        let mut s = base();
        // deleted in Bookmoor, renamed in Linkwarden: it stays
        s.local.remove("b2");
        let home = s.pairs.iter().find(|p| p.node == "b2").unwrap().remote;
        s.remote.get_mut(&home).unwrap().title = "Renamed there".into();
        // deleted in Linkwarden, renamed in Bookmoor: it stays
        let rust = s.pairs.iter().find(|p| p.node == "b1").unwrap().remote;
        s.remote.remove(&rust);
        s.local.get_mut("b1").unwrap().title = "Renamed here".into();
        s.settle(Mode::Both);
        assert!(s.titles().contains(&"Renamed there".to_string()));
        assert!(s.rtitles().contains(&"Renamed here".to_string()));
        assert_eq!(s.local.len(), 3);
        assert_eq!(s.remote.len(), 3);
    }

    #[test]
    fn a_deleted_folder_stays_while_something_new_is_in_it() {
        let mut s = base();
        // folder deleted in Bookmoor, a link added to it in Linkwarden
        s.local.remove("f1");
        s.local.remove("b1");
        let f1 = s.pairs.iter().find(|p| p.node == "f1").unwrap().remote;
        let Ref::Collection(fc) = f1 else { panic!() };
        s.rlink(600, fc, "Added there", "https://x.example");
        s.settle(Mode::Both);
        assert!(s.titles().contains(&"Added there".to_string()));
        assert!(s.titles().contains(&"Dev".to_string()));
        // the untouched link in it was deleted with the folder
        assert!(!s.titles().contains(&"Rust".to_string()));
        assert!(!s.rtitles().contains(&"Rust".to_string()));
    }

    #[test]
    fn a_deleted_collection_comes_back_while_something_new_is_in_it() {
        let mut s = base();
        let f1 = s.pairs.iter().find(|p| p.node == "f1").unwrap().remote;
        let Ref::Collection(fc) = f1 else { panic!() };
        s.remote.retain(|_, v| v.parent != fc);
        s.remote.remove(&f1);
        s.lmark("b7", "f1", "Added here", "https://y.example");
        s.settle(Mode::Both);
        assert!(s.rtitles().contains(&"Dev".to_string()));
        assert!(s.rtitles().contains(&"Added here".to_string()));
        assert!(s.local.contains_key("f1"));
        assert!(!s.local.contains_key("b1"));
    }

    #[test]
    fn push_makes_linkwarden_match_and_leaves_its_extras() {
        let mut s = base();
        let rid = s.pairs.iter().find(|p| p.node == "b2").unwrap().remote;
        s.remote.get_mut(&rid).unwrap().title = "Edited there".into();
        s.rlink(700, RC, "Extra", "https://extra.example");
        s.local.remove("b1");
        s.run(Mode::Push);
        assert_eq!(s.remote[&rid].title, "Home");
        assert!(s.rtitles().contains(&"Extra".to_string()));
        assert!(!s.rtitles().contains(&"Rust".to_string()));
        assert!(!s.titles().contains(&"Extra".to_string()));
        assert!(s.plan(Mode::Push).is_empty());
    }

    #[test]
    fn pull_makes_bookmoor_match_and_leaves_its_extras() {
        let mut s = base();
        let rid = s.pairs.iter().find(|p| p.node == "b2").unwrap().remote;
        s.remote.get_mut(&rid).unwrap().title = "Edited there".into();
        s.rlink(700, RC, "Extra", "https://extra.example");
        s.local.get_mut("b1").unwrap().title = "Edited here".into();
        s.lmark("b8", ROOT, "Local only", "https://l.example");
        s.run(Mode::Pull);
        assert_eq!(s.local["b2"].title, "Edited there");
        assert_eq!(s.local["b1"].title, "Rust");
        assert!(s.titles().contains(&"Extra".to_string()));
        assert!(s.titles().contains(&"Local only".to_string()));
        assert!(!s.rtitles().contains(&"Local only".to_string()));
        assert!(s.plan(Mode::Pull).is_empty());
    }

    #[test]
    fn a_stale_pair_ends_quietly() {
        let mut s = base();
        s.local.remove("b2");
        let home = s.pairs.iter().find(|p| p.node == "b2").unwrap().remote;
        s.remote.remove(&home);
        let pl = s.run(Mode::Both);
        assert!(pl.is_empty());
        assert_eq!(s.pairs.len(), 2);
    }

    #[test]
    fn deletions_are_counted() {
        let mut s = base();
        s.local.remove("f1");
        s.local.remove("b1");
        let pl = s.plan(Mode::Both);
        assert_eq!(pl.deletions(), 2);
    }
}
