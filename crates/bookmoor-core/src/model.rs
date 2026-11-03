use crate::position::cmp_siblings;
use std::collections::BTreeMap;

/// Stable, server-side identifier of a node in the canonical tree.
pub type NodeId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub id: NodeId,
    /// Fractional index among the siblings, see [`crate::position`].
    pub position: String,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: NodeId,
    /// Fractional index among the siblings; empty for the tree root.
    pub position: String,
    pub title: String,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Folder(Folder),
    Bookmark(Bookmark),
}

impl Node {
    pub fn id(&self) -> &str {
        match self {
            Node::Folder(f) => &f.id,
            Node::Bookmark(b) => &b.id,
        }
    }

    pub fn position(&self) -> &str {
        match self {
            Node::Folder(f) => &f.position,
            Node::Bookmark(b) => &b.position,
        }
    }
}

/// A bookmark tree with a single root folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub root: Folder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Folder,
    Bookmark,
}

/// One node with its parent link, used for id-based comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatNode {
    pub parent: Option<NodeId>,
    pub kind: Kind,
    pub title: String,
    pub url: Option<String>,
    /// Fractional index among the siblings (empty for the root).
    pub position: String,
}

impl Folder {
    /// Order `children` by `(position, id)`, recursively. This is the
    /// canonical sibling order; ties on position are broken by id.
    pub fn sort_children(&mut self) {
        self.children
            .sort_by(|a, b| cmp_siblings((a.position(), a.id()), (b.position(), b.id())));
        for child in &mut self.children {
            if let Node::Folder(f) = child {
                f.sort_children();
            }
        }
    }
}

impl Tree {
    pub fn new(root_id: impl Into<NodeId>, title: impl Into<String>) -> Self {
        Tree {
            root: Folder {
                id: root_id.into(),
                title: title.into(),
                children: Vec::new(),
                position: String::new(),
            },
        }
    }

    /// Put every folder's children into canonical `(position, id)` order.
    pub fn sort(&mut self) {
        self.root.sort_children();
    }

    /// Flatten into an id -> node map (the root has `parent == None`).
    pub fn flatten(&self) -> BTreeMap<NodeId, FlatNode> {
        let mut out = BTreeMap::new();
        out.insert(
            self.root.id.clone(),
            FlatNode {
                parent: None,
                kind: Kind::Folder,
                title: self.root.title.clone(),
                url: None,
                position: String::new(),
            },
        );
        flatten_children(&self.root, &mut out);
        out
    }
}

fn flatten_children(folder: &Folder, out: &mut BTreeMap<NodeId, FlatNode>) {
    for child in &folder.children {
        match child {
            Node::Folder(f) => {
                out.insert(
                    f.id.clone(),
                    FlatNode {
                        parent: Some(folder.id.clone()),
                        kind: Kind::Folder,
                        title: f.title.clone(),
                        url: None,
                        position: f.position.clone(),
                    },
                );
                flatten_children(f, out);
            }
            Node::Bookmark(b) => {
                out.insert(
                    b.id.clone(),
                    FlatNode {
                        parent: Some(folder.id.clone()),
                        kind: Kind::Bookmark,
                        title: b.title.clone(),
                        url: Some(b.url.clone()),
                        position: b.position.clone(),
                    },
                );
            }
        }
    }
}
