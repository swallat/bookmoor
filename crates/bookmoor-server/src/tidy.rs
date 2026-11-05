//! Tidying a bookmark tree: merge folders of the same name and drop repeated
//! bookmarks. This is what two hosts that already hold the same links leave
//! behind when their titles or folder layouts differ a little.

use bookmoor_core::model::Kind;
use bookmoor_core::wire::WireNode;
use std::collections::{HashMap, HashSet};

/// What [`dedupe`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tidied {
    pub bookmarks_removed: usize,
    pub folders_merged: usize,
}

impl Tidied {
    pub fn total(&self) -> usize {
        self.bookmarks_removed + self.folders_merged
    }
}

/// Within one folder, bookmarks with the same URL are one bookmark (the first
/// stays, whatever its title) and sub-folders with the same title are one
/// folder (their contents are merged, then checked again). The top-level
/// folders (toolbar, other, mobile) are never merged with each other.
pub fn dedupe(nodes: &[WireNode]) -> (Vec<WireNode>, Tidied) {
    let mut kids: HashMap<&str, Vec<&WireNode>> = HashMap::new();
    for n in nodes {
        if let Some(p) = &n.parent {
            kids.entry(p.as_str()).or_default().push(n);
        }
    }
    for v in kids.values_mut() {
        v.sort_by(|a, b| (&a.position, &a.id).cmp(&(&b.position, &b.id)));
    }
    let mut out = Vec::new();
    let mut t = Tidied::default();
    let Some(root) = nodes.iter().find(|n| n.parent.is_none()) else {
        return (nodes.to_vec(), t);
    };
    out.push(root.clone());
    // Folders to fill: (kept folder id, ids of the folders merged into it).
    let mut work: Vec<(String, Vec<String>)> = Vec::new();
    // The root's children are the fixed roles: keep them as they are.
    for r in kids.get(root.id.as_str()).into_iter().flatten() {
        out.push((*r).clone());
        work.push((r.id.clone(), vec![r.id.clone()]));
    }
    while let Some((keep, sources)) = work.pop() {
        let mut urls: HashSet<&str> = HashSet::new();
        let mut folders: Vec<(&WireNode, Vec<String>)> = Vec::new();
        let mut by_title: HashMap<&str, usize> = HashMap::new();
        let mut items: Vec<&WireNode> = sources
            .iter()
            .flat_map(|s| kids.get(s.as_str()).into_iter().flatten().copied())
            .collect();
        // Several sources: interleave by position so the merge is stable.
        if sources.len() > 1 {
            items.sort_by(|a, b| (&a.position, &a.id).cmp(&(&b.position, &b.id)));
        }
        for n in items {
            match n.kind {
                Kind::Bookmark => {
                    let url = n.url.as_deref().unwrap_or("").trim();
                    if urls.insert(url) {
                        let mut k = n.clone();
                        k.parent = Some(keep.clone());
                        out.push(k);
                    } else {
                        t.bookmarks_removed += 1;
                    }
                }
                Kind::Folder => match by_title.get(n.title.trim()) {
                    Some(&i) => {
                        folders[i].1.push(n.id.clone());
                        t.folders_merged += 1;
                    }
                    None => {
                        by_title.insert(n.title.trim(), folders.len());
                        folders.push((n, vec![n.id.clone()]));
                    }
                },
            }
        }
        for (f, merged) in folders {
            let mut k = f.clone();
            k.parent = Some(keep.clone());
            out.push(k);
            work.push((f.id.clone(), merged));
        }
    }
    (out, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: &str, parent: &str, pos: &str, title: &str) -> WireNode {
        WireNode {
            id: id.into(),
            parent: Some(parent.into()),
            position: pos.into(),
            kind: Kind::Folder,
            title: title.into(),
            url: None,
        }
    }
    fn bm(id: &str, parent: &str, pos: &str, title: &str, url: &str) -> WireNode {
        WireNode {
            kind: Kind::Bookmark,
            url: Some(url.into()),
            ..folder(id, parent, pos, title)
        }
    }
    fn base() -> Vec<WireNode> {
        vec![
            WireNode {
                parent: None,
                ..folder("root", "", "", "Bookmarks")
            },
            folder("toolbar", "root", "V", "toolbar"),
            folder("other", "root", "W", "other"),
            folder("mobile", "root", "X", "mobile"),
        ]
    }

    #[test]
    fn repeated_urls_in_one_folder_collapse_to_the_first() {
        let mut n = base();
        n.push(bm("a", "toolbar", "V", "Rust", "https://rust-lang.org"));
        n.push(bm(
            "b",
            "toolbar",
            "W",
            "Rust Lang",
            "https://rust-lang.org",
        ));
        n.push(bm("c", "other", "V", "Rust", "https://rust-lang.org"));
        let (out, t) = dedupe(&n);
        assert_eq!(t.bookmarks_removed, 1);
        let ids: Vec<&str> = out.iter().map(|x| x.id.as_str()).collect();
        assert!(ids.contains(&"a") && !ids.contains(&"b") && ids.contains(&"c"));
    }

    #[test]
    fn same_named_folders_merge_and_their_content_is_checked_again() {
        let mut n = base();
        n.push(folder("f1", "toolbar", "V", "Dev"));
        n.push(folder("f2", "toolbar", "W", "Dev "));
        n.push(bm("a", "f1", "V", "x", "https://x.example"));
        n.push(bm("b", "f2", "V", "x", "https://x.example"));
        n.push(bm("c", "f2", "W", "y", "https://y.example"));
        n.push(folder("g1", "f1", "W", "Sub"));
        n.push(folder("g2", "f2", "X", "Sub"));
        n.push(bm("d", "g2", "V", "z", "https://z.example"));
        let (out, t) = dedupe(&n);
        assert_eq!(t.folders_merged, 2);
        assert_eq!(t.bookmarks_removed, 1);
        let get = |id: &str| out.iter().find(|x| x.id == id).cloned();
        assert!(get("f2").is_none() && get("g2").is_none() && get("b").is_none());
        assert_eq!(get("c").unwrap().parent.as_deref(), Some("f1"));
        assert_eq!(get("d").unwrap().parent.as_deref(), Some("g1"));
    }

    #[test]
    fn a_clean_tree_is_unchanged() {
        let mut n = base();
        n.push(bm("a", "toolbar", "V", "Rust", "https://rust-lang.org"));
        n.push(bm("b", "other", "V", "Rust", "https://rust-lang.org"));
        let (out, t) = dedupe(&n);
        assert_eq!(t, Tidied::default());
        assert_eq!(out.len(), n.len());
    }
}
