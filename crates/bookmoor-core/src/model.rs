use std::collections::BTreeMap;

/// Stable, server-side identifier of a node in the canonical tree.
pub type NodeId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub id: NodeId,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: NodeId,
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
}

impl Tree {
    pub fn new(root_id: impl Into<NodeId>, title: impl Into<String>) -> Self {
        Tree {
            root: Folder {
                id: root_id.into(),
                title: title.into(),
                children: Vec::new(),
            },
        }
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
                    },
                );
            }
        }
    }
}
