use crate::model::{Kind, NodeId, Tree};

/// A difference between two trees, keyed by node id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added { id: NodeId, parent: NodeId, kind: Kind, title: String, url: Option<String> },
    Removed { id: NodeId },
    Renamed { id: NodeId, title: String },
    UrlChanged { id: NodeId, url: String },
    Moved { id: NodeId, new_parent: NodeId },
}

/// Compute what changed from `base` to `current`.
///
/// `Added` changes are ordered parent-first so they can be applied in sequence.
/// Reordering among siblings is not reported yet.
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
                    },
                ));
            }
            Some(before) => {
                if before.title != now.title {
                    changes.push(Change::Renamed { id: id.clone(), title: now.title.clone() });
                }
                if before.url != now.url {
                    if let Some(url) = &now.url {
                        changes.push(Change::UrlChanged { id: id.clone(), url: url.clone() });
                    }
                }
                if before.parent != now.parent {
                    if let Some(p) = &now.parent {
                        changes.push(Change::Moved { id: id.clone(), new_parent: p.clone() });
                    }
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

    fn bm(id: &str, title: &str, url: &str) -> Node {
        Node::Bookmark(Bookmark { id: id.into(), title: title.into(), url: url.into() })
    }
    fn folder(id: &str, title: &str, children: Vec<Node>) -> Node {
        Node::Folder(Folder { id: id.into(), title: title.into(), children })
    }
    fn tree(children: Vec<Node>) -> Tree {
        let mut t = Tree::new("root", "Bookmarks");
        t.root.children = children;
        t
    }

    fn base() -> Tree {
        tree(vec![
            folder("f1", "Dev", vec![bm("b1", "GitHub", "https://github.com")]),
            folder("f2", "News", vec![]),
        ])
    }

    #[test]
    fn identical_trees_have_no_changes() {
        assert!(diff(&base(), &base()).is_empty());
    }

    #[test]
    fn detects_rename_and_url_change() {
        let cur = tree(vec![
            folder("f1", "Dev", vec![bm("b1", "GH", "https://github.com/explore")]),
            folder("f2", "News", vec![]),
        ]);
        let d = diff(&base(), &cur);
        assert!(d.contains(&Change::Renamed { id: "b1".into(), title: "GH".into() }));
        assert!(d.contains(&Change::UrlChanged { id: "b1".into(), url: "https://github.com/explore".into() }));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn detects_removal() {
        let cur = tree(vec![folder("f1", "Dev", vec![]), folder("f2", "News", vec![])]);
        assert_eq!(diff(&base(), &cur), vec![Change::Removed { id: "b1".into() }]);
    }

    #[test]
    fn detects_move_between_folders() {
        let cur = tree(vec![
            folder("f1", "Dev", vec![]),
            folder("f2", "News", vec![bm("b1", "GitHub", "https://github.com")]),
        ]);
        assert_eq!(diff(&base(), &cur), vec![Change::Moved { id: "b1".into(), new_parent: "f2".into() }]);
    }

    #[test]
    fn added_nodes_are_ordered_parent_first() {
        let cur = tree(vec![
            folder("f1", "Dev", vec![bm("b1", "GitHub", "https://github.com")]),
            folder("f2", "News", vec![]),
            // child id sorts before parent id on purpose
            folder("z9", "New", vec![bm("a1", "HN", "https://news.ycombinator.com")]),
        ]);
        let d = diff(&base(), &cur);
        let pos = |id: &str| {
            d.iter().position(|c| matches!(c, Change::Added { id: i, .. } if i == id)).unwrap()
        };
        assert!(pos("z9") < pos("a1"));
    }
}
