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

/// Within one folder, bookmarks with the same URL are one bookmark (the one
/// that comes first in the folder's order stays, whatever its title) and sub-folders with the same title are one
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

/// One place a duplicated link is saved in.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DupItem {
    pub id: String,
    pub title: String,
    pub url: String,
    /// Titles of the folders above it, outermost first.
    pub path: Vec<String>,
}

/// Bookmarks that point to the same page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DupGroup {
    pub url: String,
    pub items: Vec<DupItem>,
    /// The one suggested to stay.
    pub keep: String,
}

/// The URL as far as it names the page: scheme and host in lower case, no
/// `www.`, no default port, no fragment (unless it is a `#/route`), no
/// trailing slash. The query stays,
/// since it usually picks the page. Anything that is not http(s) is compared
/// as it is.
pub fn normalize_url(url: &str) -> String {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    let (scheme, rest) = if lower.starts_with("https://") {
        ("https", &u[8..])
    } else if lower.starts_with("http://") {
        ("http", &u[7..])
    } else {
        return u.to_string();
    };
    // A fragment is dropped, unless it is a route of a single-page app.
    let rest = match rest.split_once('#') {
        Some((r, f)) if !(f.starts_with('/') || f.starts_with('!')) => r,
        _ => rest,
    };
    let (auth, tail) = match rest.find(['/', '?']) {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    // Credentials in the URL are kept as they are; only the host is folded.
    let (userinfo, hostport) = match auth.rsplit_once('@') {
        Some((u, h)) => (format!("{u}@"), h),
        None => (String::new(), auth),
    };
    let mut host = hostport.trim_end_matches('.').to_ascii_lowercase();
    let default = if scheme == "https" { ":443" } else { ":80" };
    if let Some(h) = host.strip_suffix(default) {
        host = h.to_string();
    }
    if let Some(h) = host.strip_prefix("www.") {
        host = h.to_string();
    }
    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (tail, None),
    };
    let path = path.trim_end_matches('/');
    let mut out = format!("{scheme}://{userinfo}{host}{path}");
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        out.push('?');
        out.push_str(q);
    }
    out
}

fn role_label(id: &str) -> Option<&'static str> {
    match id {
        "toolbar" => Some("Bookmarks bar"),
        "other" => Some("Other bookmarks"),
        "mobile" => Some("Mobile bookmarks"),
        _ => None,
    }
}

/// Bookmarks saved more than once, wherever they are in the tree, grouped by
/// [`normalize_url`]. Groups and their members follow the tree order. The
/// suggestion to keep is the first one that has a title, else the first.
pub fn find_duplicates(nodes: &[WireNode]) -> Vec<DupGroup> {
    let by_id: HashMap<&str, &WireNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut kids: HashMap<&str, Vec<&WireNode>> = HashMap::new();
    for n in nodes {
        if let Some(p) = &n.parent {
            kids.entry(p.as_str()).or_default().push(n);
        }
    }
    for v in kids.values_mut() {
        v.sort_by(|a, b| (&a.position, &a.id).cmp(&(&b.position, &b.id)));
    }
    let Some(root) = nodes.iter().find(|n| n.parent.is_none()) else {
        return Vec::new();
    };
    // Depth-first in tree order.
    let mut ordered: Vec<&WireNode> = Vec::new();
    let mut stack: Vec<&WireNode> = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind == Kind::Bookmark {
            ordered.push(n);
        }
        if let Some(k) = kids.get(n.id.as_str()) {
            stack.extend(k.iter().rev());
        }
    }
    let path_of = |n: &WireNode| -> Vec<String> {
        let mut parts = Vec::new();
        let mut cur = n.parent.as_deref().and_then(|p| by_id.get(p));
        while let Some(f) = cur {
            if f.parent.is_none() {
                break;
            }
            parts.push(
                role_label(&f.id)
                    .map(str::to_string)
                    .unwrap_or_else(|| f.title.clone()),
            );
            cur = f.parent.as_deref().and_then(|p| by_id.get(p));
        }
        parts.reverse();
        parts
    };
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<Vec<&WireNode>> = Vec::new();
    for n in ordered {
        let key = normalize_url(n.url.as_deref().unwrap_or(""));
        if key.is_empty() {
            continue;
        }
        match index.get(&key) {
            Some(&i) => groups[i].push(n),
            None => {
                index.insert(key, groups.len());
                groups.push(vec![n]);
            }
        }
    }
    groups
        .into_iter()
        .filter(|g| g.len() > 1)
        .map(|g| {
            let keep = g
                .iter()
                .find(|n| !n.title.trim().is_empty())
                .unwrap_or(&g[0])
                .id
                .clone();
            DupGroup {
                url: g[0].url.clone().unwrap_or_default(),
                items: g
                    .iter()
                    .map(|n| DupItem {
                        id: n.id.clone(),
                        title: n.title.clone(),
                        url: n.url.clone().unwrap_or_default(),
                        path: path_of(n),
                    })
                    .collect(),
                keep,
            }
        })
        .collect()
}

/// The tree without the given bookmarks. Only bookmarks that are part of a
/// duplicate group can be removed, and every group keeps at least one.
pub fn remove_duplicates(nodes: &[WireNode], ids: &[String]) -> Result<Vec<WireNode>, String> {
    let gone: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let groups = find_duplicates(nodes);
    let known: HashSet<&str> = groups
        .iter()
        .flat_map(|g| g.items.iter().map(|i| i.id.as_str()))
        .collect();
    if let Some(bad) = gone.iter().find(|i| !known.contains(**i)) {
        return Err(format!(
            "{bad} is not a duplicate; look for duplicates again"
        ));
    }
    if groups
        .iter()
        .any(|g| g.items.iter().all(|i| gone.contains(i.id.as_str())))
    {
        return Err("keep at least one bookmark of every link".into());
    }
    Ok(nodes
        .iter()
        .filter(|n| !gone.contains(n.id.as_str()))
        .cloned()
        .collect())
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

    #[test]
    fn urls_that_name_the_same_page_are_equal() {
        let a = normalize_url(" HTTPS://WWW.Example.com:443/a/b/#top ");
        assert_eq!(a, "https://example.com/a/b");
        assert_eq!(
            normalize_url("http://example.com"),
            normalize_url("http://example.com/")
        );
        assert_ne!(
            normalize_url("http://example.com/?q=1"),
            normalize_url("http://example.com/?q=2")
        );
        assert_ne!(
            normalize_url("http://example.com"),
            normalize_url("https://example.com")
        );
        assert_eq!(normalize_url("mailto:x@y"), "mailto:x@y");
        assert_ne!(
            normalize_url("https://a.example/#/inbox"),
            normalize_url("https://a.example/#/settings")
        );
        assert_ne!(
            normalize_url("https://Bob:x@h.example"),
            normalize_url("https://bob:x@h.example")
        );
    }

    #[test]
    fn duplicates_are_found_across_folders_and_can_be_removed() {
        let mut n = base();
        n.push(folder("f", "other", "V", "Dev"));
        n.push(bm("a", "toolbar", "V", "", "https://rust-lang.org/"));
        n.push(bm("b", "f", "V", "Rust", "https://www.rust-lang.org"));
        n.push(bm(
            "c",
            "other",
            "W",
            "Rust again",
            "https://rust-lang.org#x",
        ));
        n.push(bm("d", "other", "X", "Solo", "https://solo.example"));
        let g = find_duplicates(&n);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].items.len(), 3);
        assert_eq!(g[0].keep, "b", "the first one with a title");
        assert_eq!(g[0].items[0].path, vec!["Bookmarks bar".to_string()]);
        assert_eq!(
            g[0].items[1].path,
            vec!["Other bookmarks".to_string(), "Dev".to_string()]
        );
        let out = remove_duplicates(&n, &["a".into(), "c".into()]).unwrap();
        let ids: Vec<&str> = out.iter().map(|x| x.id.as_str()).collect();
        assert!(
            ids.contains(&"b") && ids.contains(&"d") && !ids.contains(&"a") && !ids.contains(&"c")
        );
        assert!(find_duplicates(&out).is_empty());
        assert!(remove_duplicates(&n, &["a".into(), "b".into(), "c".into()]).is_err());
        assert!(remove_duplicates(&n, &["d".into()]).is_err());
    }
}
