use crate::model::{Kind, NodeId, Tree};

/// A difference between two trees, keyed by node id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added {
        id: NodeId,
        parent: NodeId,
        kind: Kind,
        title: String,
        url: Option<String>,
        position: String,
    },
    Removed {
        id: NodeId,
    },
    Renamed {
        id: NodeId,
        title: String,
    },
    UrlChanged {
        id: NodeId,
        url: String,
    },
    Moved {
        id: NodeId,
        new_parent: NodeId,
    },
    /// The node's fractional position among its siblings changed. A move to
    /// another folder that also changes the position yields `Moved` and
    /// `Reordered`.
    Reordered {
        id: NodeId,
        position: String,
    },
}

/// Compute what changed from `base` to `current`.
///
/// `Added` changes are ordered parent-first so they can be applied in sequence.
/// Sibling order is compared by `position`, not by index in `children`.
pub fn diff(base: &Tree, current: &Tree) -> Vec<Change> {
    let b = base.flatten();
    let c = current.flatten();
    let mut changes = Vec::new();

    let mut added: Vec<(usize, Change)> = Vec::new();
    for (id, now) in &c {
        match b.get(id) {
            None => {
                let parent = match &now.parent {
                    Some(p) => p.clone(),
                    None => continue, // a new root is not a change we can express
                };
                let mut depth = 0;
                let mut cur = now.parent.clone();
                while let Some(p) = cur {
                    depth += 1;
                    cur = c.get(&p).and_then(|n| n.parent.clone());
                }
                added.push((
                    depth,
                    Change::Added {
                        id: id.clone(),
                        parent,
                        kind: now.kind,
                        title: now.title.clone(),
                        url: now.url.clone(),
                        position: now.position.clone(),
                    },
                ));
            }
            Some(before) => {
                if before.title != now.title {
                    changes.push(Change::Renamed {
                        id: id.clone(),
                        title: now.title.clone(),
                    });
                }
                if before.url != now.url {
                    if let Some(url) = &now.url {
                        changes.push(Change::UrlChanged {
                            id: id.clone(),
                            url: url.clone(),
                        });
                    }
                }
                if before.parent != now.parent {
                    if let Some(p) = &now.parent {
                        changes.push(Change::Moved {
                            id: id.clone(),
                            new_parent: p.clone(),
                        });
                    }
                }
                if before.position != now.position && now.parent.is_some() {
                    changes.push(Change::Reordered {
                        id: id.clone(),
                        position: now.position.clone(),
                    });
                }
            }
        }
    }
    added.sort_by_key(|(depth, _)| *depth);
    let mut result: Vec<Change> = added.into_iter().map(|(_, c)| c).collect();
    result.extend(changes);

    for id in b.keys() {
        if !c.contains_key(id) {
            result.push(Change::Removed { id: id.clone() });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Bookmark, Folder, Node};

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
        t
    }

    fn gh() -> Node {
        bm("b1", "V", "GitHub", "https://github.com")
    }

    fn base() -> Tree {
        tree(vec![
            folder("f1", "V", "Dev", vec![gh()]),
            folder("f2", "W", "News", vec![]),
        ])
    }

    #[test]
    fn identical_trees_have_no_changes() {
        assert!(diff(&base(), &base()).is_empty());
    }

    #[test]
    fn detects_rename_and_url_change() {
        let cur = tree(vec![
            folder(
                "f1",
                "V",
                "Dev",
                vec![bm("b1", "V", "GH", "https://github.com/explore")],
            ),
            folder("f2", "W", "News", vec![]),
        ]);
        let d = diff(&base(), &cur);
        assert!(d.contains(&Change::Renamed {
            id: "b1".into(),
            title: "GH".into()
        }));
        assert!(d.contains(&Change::UrlChanged {
            id: "b1".into(),
            url: "https://github.com/explore".into()
        }));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn detects_removal() {
        let cur = tree(vec![
            folder("f1", "V", "Dev", vec![]),
            folder("f2", "W", "News", vec![]),
        ]);
        assert_eq!(
            diff(&base(), &cur),
            vec![Change::Removed { id: "b1".into() }]
        );
    }

    #[test]
    fn detects_move_between_folders() {
        let cur = tree(vec![
            folder("f1", "V", "Dev", vec![]),
            folder("f2", "W", "News", vec![gh()]),
        ]);
        assert_eq!(
            diff(&base(), &cur),
            vec![Change::Moved {
                id: "b1".into(),
                new_parent: "f2".into()
            }]
        );
    }

    #[test]
    fn move_with_new_position_reports_both() {
        let cur = tree(vec![
            folder("f1", "V", "Dev", vec![]),
            folder(
                "f2",
                "W",
                "News",
                vec![bm("b1", "a", "GitHub", "https://github.com")],
            ),
        ]);
        let d = diff(&base(), &cur);
        assert_eq!(
            d,
            vec![
                Change::Moved {
                    id: "b1".into(),
                    new_parent: "f2".into()
                },
                Change::Reordered {
                    id: "b1".into(),
                    position: "a".into()
                }
            ]
        );
    }

    #[test]
    fn detects_reorder_within_folder() {
        let before = tree(vec![folder(
            "f1",
            "V",
            "Dev",
            vec![
                bm("b1", "V", "A", "https://a.example"),
                bm("b2", "W", "B", "https://b.example"),
            ],
        )]);
        // b2 moves in front of b1: only b2 changes position.
        let after = tree(vec![folder(
            "f1",
            "V",
            "Dev",
            vec![
                bm("b2", "G", "B", "https://b.example"),
                bm("b1", "V", "A", "https://a.example"),
            ],
        )]);
        assert_eq!(
            diff(&before, &after),
            vec![Change::Reordered {
                id: "b2".into(),
                position: "G".into()
            }]
        );
    }

    #[test]
    fn child_order_in_vec_is_irrelevant_to_diff() {
        let a = tree(vec![
            folder("f1", "V", "Dev", vec![]),
            folder("f2", "W", "News", vec![]),
        ]);
        let b = tree(vec![
            folder("f2", "W", "News", vec![]),
            folder("f1", "V", "Dev", vec![]),
        ]);
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn sort_orders_by_position_then_id() {
        let mut t = tree(vec![
            bm("b3", "W", "C", "https://c.example"),
            bm("b2", "V", "B", "https://b.example"),
            bm("b1", "V", "A", "https://a.example"),
        ]);
        t.sort();
        let ids: Vec<&str> = t.root.children.iter().map(Node::id).collect();
        assert_eq!(ids, ["b1", "b2", "b3"]);
    }

    #[test]
    fn added_nodes_carry_position_and_are_ordered_parent_first() {
        let cur = tree(vec![
            folder("f1", "V", "Dev", vec![gh()]),
            folder("f2", "W", "News", vec![]),
            // child id sorts before parent id on purpose
            folder(
                "z9",
                "X",
                "New",
                vec![bm("a1", "V", "HN", "https://news.ycombinator.com")],
            ),
        ]);
        let d = diff(&base(), &cur);
        let pos = |id: &str| {
            d.iter()
                .position(|c| matches!(c, Change::Added { id: i, .. } if i == id))
                .unwrap()
        };
        assert!(pos("z9") < pos("a1"));
        assert!(matches!(
            &d[pos("z9")],
            Change::Added { position, .. } if position == "X"
        ));
    }
}
