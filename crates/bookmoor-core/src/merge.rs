//! Three-way merge of bookmark trees (`docs/sync-architecture.md`, section 4).
//!
//! `merge(base, ours, theirs, prefer)` merges per node and per field group
//! (`title`, `url`, `placement` = parent + position):
//!
//! * Different field groups edited on both sides: both applied.
//! * Same group edited differently: `prefer` wins; the loser is reported in
//!   [`Merged::rejected`] so nothing is lost silently.
//! * Edit vs. delete: **the edit wins**, the node survives.
//! * Delete vs. delete: deleted.
//! * Move into a folder that was deleted: re-parented to the nearest
//!   surviving ancestor.
//! * New node inside a deleted folder: the folder (and deleted ancestors) is
//!   resurrected; its other, untouched children stay deleted.
//! * A move that would create a cycle is rejected (`theirs`, the later side,
//!   loses) and the node keeps its placement from `ours`.
//! * Concurrent inserts with equal positions are ordered by id.
//!
//! `ours` is the canonical state on the server, `theirs` the incoming
//! client state. The roots of all three trees must have the same id.

use crate::diff::Change;
use crate::model::{FlatNode, Folder, Kind, Node, NodeId, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// One side of a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

/// Why a change did not make it into the merged tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Same field edited on both sides; the other side was preferred.
    ConflictLost,
    /// The move would have created a cycle.
    CycleAvoided,
}

/// A change that was overwritten or refused during the merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub side: Side,
    pub change: Change,
    pub reason: Reason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub tree: Tree,
    pub rejected: Vec<Rejected>,
}

type Flat = BTreeMap<NodeId, FlatNode>;
type Placement = (Option<NodeId>, String);

fn placement(n: &FlatNode) -> Placement {
    (n.parent.clone(), n.position.clone())
}

/// Pick the merged value of one field group. Returns the value and, on a
/// real conflict, the side whose value lost.
fn pick<T: PartialEq + Clone>(
    base: Option<&T>,
    ours: &T,
    theirs: &T,
    prefer: Side,
) -> (T, Option<Side>) {
    if ours == theirs {
        (ours.clone(), None)
    } else if base == Some(ours) {
        (theirs.clone(), None)
    } else if base == Some(theirs) {
        (ours.clone(), None)
    } else if prefer == Side::Ours {
        (ours.clone(), Some(Side::Theirs))
    } else {
        (theirs.clone(), Some(Side::Ours))
    }
}

pub fn merge(base: &Tree, ours: &Tree, theirs: &Tree, prefer: Side) -> Merged {
    let b = base.flatten();
    let o = ours.flatten();
    let t = theirs.flatten();
    let mut rejected = Vec::new();
    let mut merged: Flat = BTreeMap::new();

    let ids: BTreeSet<&NodeId> = b.keys().chain(o.keys()).chain(t.keys()).collect();
    for id in ids {
        let (bn, on, tn) = (b.get(id), o.get(id), t.get(id));
        let survives = match bn {
            None => on.is_some() || tn.is_some(),
            Some(bn) => match (on, tn) {
                (None, None) => false,
                (None, Some(tn)) => tn != bn, // edit wins over delete
                (Some(on), None) => on != bn,
                (Some(_), Some(_)) => true,
            },
        };
        if !survives {
            continue;
        }
        // A side that deleted the node contributes no changes.
        let (on, tn) = match (on.or(bn), tn.or(bn)) {
            (Some(on), Some(tn)) => (on, tn),
            (Some(n), None) | (None, Some(n)) => {
                merged.insert(id.clone(), n.clone());
                continue;
            }
            (None, None) => continue,
        };

        let (title, lost) = pick(bn.map(|n| &n.title), &on.title, &tn.title, prefer);
        if let Some(side) = lost {
            let loser = if side == Side::Ours { on } else { tn };
            rejected.push(Rejected {
                side,
                change: Change::Renamed {
                    id: id.clone(),
                    title: loser.title.clone(),
                },
                reason: Reason::ConflictLost,
            });
        }
        let (url, lost) = pick(bn.map(|n| &n.url), &on.url, &tn.url, prefer);
        if let Some(side) = lost {
            let loser = if side == Side::Ours { on } else { tn };
            if let Some(u) = &loser.url {
                rejected.push(Rejected {
                    side,
                    change: Change::UrlChanged {
                        id: id.clone(),
                        url: u.clone(),
                    },
                    reason: Reason::ConflictLost,
                });
            }
        }
        let bp = bn.map(placement);
        let (place, lost) = pick(bp.as_ref(), &placement(on), &placement(tn), prefer);
        if let Some(side) = lost {
            let loser = if side == Side::Ours { on } else { tn };
            if loser.parent != place.0 {
                if let Some(p) = &loser.parent {
                    rejected.push(Rejected {
                        side,
                        change: Change::Moved {
                            id: id.clone(),
                            new_parent: p.clone(),
                        },
                        reason: Reason::ConflictLost,
                    });
                }
            }
            if loser.position != place.1 {
                rejected.push(Rejected {
                    side,
                    change: Change::Reordered {
                        id: id.clone(),
                        position: loser.position.clone(),
                    },
                    reason: Reason::ConflictLost,
                });
            }
        }
        merged.insert(
            id.clone(),
            FlatNode {
                parent: place.0,
                kind: on.kind,
                title,
                url,
                position: place.1,
            },
        );
    }

    let root_id = base.root.id.clone();
    fix_missing_parents(&mut merged, &b, &o, &t, &root_id);
    break_cycles(&mut merged, &b, &o, &mut rejected);

    Merged {
        tree: build_tree(&root_id, &merged),
        rejected,
    }
}

/// Make sure every node's parent survives.
fn fix_missing_parents(merged: &mut Flat, b: &Flat, o: &Flat, t: &Flat, root_id: &NodeId) {
    let lookup = |id: &NodeId| b.get(id).or_else(|| o.get(id)).or_else(|| t.get(id));
    let mut resurrected: BTreeSet<NodeId> = BTreeSet::new();
    loop {
        let mut inserted = false;
        let ids: Vec<NodeId> = merged.keys().cloned().collect();
        for id in ids {
            let Some(parent) = merged[&id].parent.clone() else {
                continue;
            };
            if merged.contains_key(&parent) {
                continue;
            }
            if !b.contains_key(&id) || resurrected.contains(&id) {
                // A new node needs its folder: bring it back.
                match lookup(&parent) {
                    Some(src) => {
                        merged.insert(parent.clone(), src.clone());
                        resurrected.insert(parent);
                        inserted = true;
                    }
                    None => merged.get_mut(&id).unwrap().parent = Some(root_id.clone()),
                }
            } else {
                // An existing node moved into a deleted folder: climb to the
                // nearest surviving ancestor.
                let mut cur = Some(parent);
                let mut target = root_id.clone();
                while let Some(p) = cur {
                    if merged.contains_key(&p) {
                        target = p;
                        break;
                    }
                    cur = lookup(&p).and_then(|n| n.parent.clone());
                }
                merged.get_mut(&id).unwrap().parent = Some(target);
            }
        }
        if !inserted {
            break;
        }
    }
}

fn find_cycle(merged: &Flat) -> Option<Vec<NodeId>> {
    for start in merged.keys() {
        let mut seen: Vec<&NodeId> = Vec::new();
        let mut cur = Some(start);
        while let Some(id) = cur {
            if let Some(i) = seen.iter().position(|s| *s == id) {
                return Some(seen[i..].iter().map(|s| (*s).clone()).collect());
            }
            seen.push(id);
            cur = merged.get(id).and_then(|n| n.parent.as_ref());
        }
    }
    None
}

/// Reject moves that created a parent cycle. `theirs` is the later side, so
/// its placement loses and the node falls back to `ours` (or to `base`).
fn break_cycles(merged: &mut Flat, b: &Flat, o: &Flat, rejected: &mut Vec<Rejected>) {
    let mut reverted: BTreeSet<NodeId> = BTreeSet::new();
    while let Some(mut cycle) = find_cycle(merged) {
        cycle.sort();
        let pick = |from: &Flat, merged: &Flat, reverted: &BTreeSet<NodeId>| {
            cycle
                .iter()
                .find(|id| {
                    !reverted.contains(*id)
                        && from
                            .get(*id)
                            .is_some_and(|n| placement(n) != placement(&merged[*id]))
                })
                .cloned()
        };
        let (id, source) = match pick(o, merged, &reverted) {
            Some(id) => (id, o),
            None => match pick(b, merged, &reverted) {
                Some(id) => (id, b),
                None => break, // unreachable for acyclic inputs
            },
        };
        let n = source[&id].clone();
        let m = merged.get_mut(&id).unwrap();
        let (lost_parent, lost_pos) = (m.parent.clone(), m.position.clone());
        m.parent = n.parent.clone();
        m.position = n.position.clone();
        reverted.insert(id.clone());
        if let Some(p) = lost_parent {
            rejected.push(Rejected {
                side: Side::Theirs,
                change: Change::Moved {
                    id: id.clone(),
                    new_parent: p,
                },
                reason: Reason::CycleAvoided,
            });
        }
        if lost_pos != n.position {
            rejected.push(Rejected {
                side: Side::Theirs,
                change: Change::Reordered {
                    id,
                    position: lost_pos,
                },
                reason: Reason::CycleAvoided,
            });
        }
    }
}

fn build_tree(root_id: &NodeId, merged: &Flat) -> Tree {
    let mut kids: BTreeMap<&NodeId, Vec<&NodeId>> = BTreeMap::new();
    for (id, n) in merged {
        if let Some(p) = &n.parent {
            kids.entry(p).or_default().push(id);
        }
    }
    fn folder(id: &NodeId, merged: &Flat, kids: &BTreeMap<&NodeId, Vec<&NodeId>>) -> Folder {
        let n = &merged[id];
        let children = kids
            .get(id)
            .map(|v| v.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|cid| {
                let c = &merged[*cid];
                match c.kind {
                    Kind::Folder => Node::Folder(folder(cid, merged, kids)),
                    Kind::Bookmark => Node::Bookmark(crate::model::Bookmark {
                        id: (*cid).clone(),
                        position: c.position.clone(),
                        title: c.title.clone(),
                        url: c.url.clone().unwrap_or_default(),
                    }),
                }
            })
            .collect();
        Folder {
            id: id.clone(),
            position: n.position.clone(),
            title: n.title.clone(),
            children,
        }
    }
    let mut tree = Tree {
        root: folder(root_id, merged, &kids),
    };
    tree.sort();
    tree
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Bookmark;

    fn bm(id: &str, pos: &str, title: &str, url: &str) -> Node {
        Node::Bookmark(Bookmark {
            id: id.into(),
            position: pos.into(),
            title: title.into(),
            url: url.into(),
        })
    }
    fn folder(id: &str, pos: &str, title: &str, children: Vec<Node>) -> Node {
        Node::Folder(Folder {
            id: id.into(),
            position: pos.into(),
            title: title.into(),
            children,
        })
    }
    fn tree(children: Vec<Node>) -> Tree {
        let mut t = Tree::new("root", "Bookmarks");
        t.root.children = children;
        t.sort();
        t
    }

    /// root: f1 "Dev" [b1 GitHub, b2 Rust], f2 "News" [b3 HN], f3 "Misc" []
    fn base() -> Tree {
        tree(vec![
            folder(
                "f1",
                "V",
                "Dev",
                vec![
                    bm("b1", "V", "GitHub", "https://github.com"),
                    bm("b2", "W", "Rust", "https://rust-lang.org"),
                ],
            ),
            folder(
                "f2",
                "W",
                "News",
                vec![bm("b3", "V", "HN", "https://news.ycombinator.com")],
            ),
            folder("f3", "X", "Misc", vec![]),
        ])
    }

    fn with(f: impl FnOnce(&mut Flat)) -> Tree {
        let mut flat = base().flatten();
        f(&mut flat);
        build_tree(&"root".to_string(), &flat)
    }

    fn m(ours: &Tree, theirs: &Tree) -> Merged {
        merge(&base(), ours, theirs, Side::Theirs)
    }

    fn node<'a>(f: &'a Flat, id: &str) -> &'a FlatNode {
        f.get(id).unwrap_or_else(|| panic!("{id} missing"))
    }

    #[test]
    fn different_fields_are_both_applied() {
        let ours = with(|f| f.get_mut("b1").unwrap().title = "GH".into());
        let theirs = with(|f| f.get_mut("b1").unwrap().url = Some("https://gh.io".into()));
        let r = m(&ours, &theirs);
        let flat = r.tree.flatten();
        assert_eq!(node(&flat, "b1").title, "GH");
        assert_eq!(node(&flat, "b1").url.as_deref(), Some("https://gh.io"));
        assert!(r.rejected.is_empty());
    }

    #[test]
    fn same_field_prefers_configured_side_and_reports_loser() {
        let ours = with(|f| f.get_mut("b1").unwrap().title = "A".into());
        let theirs = with(|f| f.get_mut("b1").unwrap().title = "B".into());
        let r = merge(&base(), &ours, &theirs, Side::Theirs);
        assert_eq!(node(&r.tree.flatten(), "b1").title, "B");
        assert_eq!(
            r.rejected,
            vec![Rejected {
                side: Side::Ours,
                change: Change::Renamed {
                    id: "b1".into(),
                    title: "A".into()
                },
                reason: Reason::ConflictLost
            }]
        );
        let r = merge(&base(), &ours, &theirs, Side::Ours);
        assert_eq!(node(&r.tree.flatten(), "b1").title, "A");
        assert_eq!(r.rejected[0].side, Side::Theirs);
    }

    #[test]
    fn identical_changes_are_not_conflicts() {
        let both = with(|f| f.get_mut("b1").unwrap().title = "GH".into());
        let r = m(&both, &both);
        assert_eq!(node(&r.tree.flatten(), "b1").title, "GH");
        assert!(r.rejected.is_empty());
    }

    #[test]
    fn edit_wins_against_delete_in_both_directions() {
        let edited = with(|f| f.get_mut("b2").unwrap().title = "Rust!".into());
        let deleted = with(|f| {
            f.remove("b2");
        });
        for (o, t) in [(&edited, &deleted), (&deleted, &edited)] {
            for prefer in [Side::Ours, Side::Theirs] {
                let r = merge(&base(), o, t, prefer);
                let flat = r.tree.flatten();
                assert_eq!(node(&flat, "b2").title, "Rust!");
                assert!(r.rejected.is_empty());
            }
        }
    }

    #[test]
    fn delete_vs_unchanged_and_delete_vs_delete() {
        let deleted = with(|f| {
            f.remove("b2");
        });
        let r = m(&deleted, &base());
        assert!(!r.tree.flatten().contains_key("b2"));
        let r = m(&base(), &deleted);
        assert!(!r.tree.flatten().contains_key("b2"));
        let r = m(&deleted, &deleted);
        assert!(!r.tree.flatten().contains_key("b2"));
    }

    #[test]
    fn adds_on_both_sides_are_kept() {
        let add = |id: &'static str| {
            with(move |f| {
                f.insert(
                    id.into(),
                    FlatNode {
                        parent: Some("f1".into()),
                        kind: Kind::Bookmark,
                        title: id.into(),
                        url: Some(format!("https://{id}.example")),
                        position: "X".into(),
                    },
                );
            })
        };
        let r = m(&add("n1"), &add("n2"));
        let flat = r.tree.flatten();
        assert!(flat.contains_key("n1") && flat.contains_key("n2"));
        // Equal positions: ordered by id.
        let Node::Folder(f1) = &r.tree.root.children[0] else {
            panic!()
        };
        let ids: Vec<&str> = f1.children.iter().map(Node::id).collect();
        assert_eq!(ids, ["b1", "b2", "n1", "n2"]);
    }

    #[test]
    fn move_vs_move_uses_preference() {
        let ours = with(|f| f.get_mut("b1").unwrap().parent = Some("f2".into()));
        let theirs = with(|f| f.get_mut("b1").unwrap().parent = Some("f3".into()));
        let r = merge(&base(), &ours, &theirs, Side::Theirs);
        assert_eq!(node(&r.tree.flatten(), "b1").parent.as_deref(), Some("f3"));
        assert!(r.rejected.iter().any(|x| x.side == Side::Ours
            && x.change
                == Change::Moved {
                    id: "b1".into(),
                    new_parent: "f2".into()
                }));
    }

    #[test]
    fn concurrent_reorder_of_different_nodes_composes() {
        let ours = with(|f| f.get_mut("b1").unwrap().position = "a".into());
        let theirs = with(|f| f.get_mut("b2").unwrap().position = "b".into());
        let r = m(&ours, &theirs);
        let Node::Folder(f1) = &r.tree.root.children[0] else {
            panic!()
        };
        let ids: Vec<&str> = f1.children.iter().map(Node::id).collect();
        assert_eq!(ids, ["b1", "b2"]);
        let flat = r.tree.flatten();
        assert_eq!(node(&flat, "b1").position, "a");
        assert_eq!(node(&flat, "b2").position, "b");
    }

    #[test]
    fn concurrent_reorder_of_same_node_is_a_field_conflict() {
        let ours = with(|f| f.get_mut("b2").unwrap().position = "G".into());
        let theirs = with(|f| f.get_mut("b2").unwrap().position = "H".into());
        let r = m(&ours, &theirs);
        assert_eq!(node(&r.tree.flatten(), "b2").position, "H");
        assert_eq!(
            r.rejected[0].change,
            Change::Reordered {
                id: "b2".into(),
                position: "G".into()
            }
        );
    }

    #[test]
    fn move_into_deleted_folder_climbs_to_surviving_ancestor() {
        // ours deletes folder f3 (empty); theirs moves b1 into f3.
        let ours = with(|f| {
            f.remove("f3");
        });
        let theirs = with(|f| f.get_mut("b1").unwrap().parent = Some("f3".into()));
        let r = m(&ours, &theirs);
        let flat = r.tree.flatten();
        assert!(!flat.contains_key("f3"));
        assert_eq!(node(&flat, "b1").parent.as_deref(), Some("root"));
    }

    #[test]
    fn new_child_in_deleted_folder_resurrects_only_the_folder() {
        let ours = with(|f| {
            f.remove("f1");
            f.remove("b1");
            f.remove("b2");
        });
        let theirs = with(|f| {
            f.insert(
                "n1".into(),
                FlatNode {
                    parent: Some("f1".into()),
                    kind: Kind::Bookmark,
                    title: "New".into(),
                    url: Some("https://new.example".into()),
                    position: "Y".into(),
                },
            );
        });
        let r = m(&ours, &theirs);
        let flat = r.tree.flatten();
        assert_eq!(node(&flat, "n1").parent.as_deref(), Some("f1"));
        assert!(flat.contains_key("f1"));
        assert!(!flat.contains_key("b1") && !flat.contains_key("b2"));
    }

    #[test]
    fn nested_deleted_ancestors_are_resurrected() {
        let nested = tree(vec![folder(
            "f1",
            "V",
            "A",
            vec![folder("f2", "V", "B", vec![])],
        )]);
        let ours = tree(vec![]);
        let mut theirs_flat = nested.flatten();
        theirs_flat.insert(
            "n1".into(),
            FlatNode {
                parent: Some("f2".into()),
                kind: Kind::Bookmark,
                title: "N".into(),
                url: Some("https://n.example".into()),
                position: "V".into(),
            },
        );
        let theirs = build_tree(&"root".to_string(), &theirs_flat);
        let r = merge(&nested, &ours, &theirs, Side::Theirs);
        let flat = r.tree.flatten();
        assert_eq!(node(&flat, "n1").parent.as_deref(), Some("f2"));
        assert_eq!(node(&flat, "f2").parent.as_deref(), Some("f1"));
    }

    #[test]
    fn cycle_creating_move_is_rejected() {
        let nested = tree(vec![
            folder("x", "V", "X", vec![]),
            folder("y", "W", "Y", vec![]),
        ]);
        let mut o = nested.flatten();
        o.get_mut("x").unwrap().parent = Some("y".into()); // x into y
        let mut t = nested.flatten();
        t.get_mut("y").unwrap().parent = Some("x".into()); // y into x
        let ours = build_tree(&"root".to_string(), &o);
        let theirs = build_tree(&"root".to_string(), &t);
        let r = merge(&nested, &ours, &theirs, Side::Theirs);
        let flat = r.tree.flatten();
        assert_eq!(node(&flat, "x").parent.as_deref(), Some("y"));
        assert_eq!(node(&flat, "y").parent.as_deref(), Some("root"));
        assert!(r.rejected.iter().any(|x| x.reason == Reason::CycleAvoided
            && x.side == Side::Theirs
            && matches!(&x.change, Change::Moved { id, .. } if id == "y")));
        assert_eq!(r.tree.flatten().len(), nested.flatten().len());
    }

    #[test]
    fn merge_without_conflicts_is_commutative_and_idempotent() {
        let ours = with(|f| {
            f.get_mut("b1").unwrap().title = "GH".into();
            f.remove("b3");
        });
        let theirs = with(|f| {
            f.get_mut("b2").unwrap().parent = Some("f3".into());
            f.get_mut("f2").unwrap().title = "Feeds".into();
        });
        let a = merge(&base(), &ours, &theirs, Side::Theirs);
        let b = merge(&base(), &theirs, &ours, Side::Theirs);
        assert_eq!(a.tree, b.tree);
        assert!(a.rejected.is_empty() && b.rejected.is_empty());
        let again = merge(&base(), &a.tree, &theirs, Side::Theirs);
        assert_eq!(again.tree, a.tree);
    }

    #[test]
    fn unchanged_sides_return_the_other_side() {
        let theirs = with(|f| f.get_mut("b1").unwrap().title = "GH".into());
        assert_eq!(m(&base(), &theirs).tree, theirs);
        assert_eq!(m(&theirs, &base()).tree, theirs);
    }
}
