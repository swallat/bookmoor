//! The Karakeep half of a link-manager connection: its HTTP client, reading
//! its lists and bookmarks, and carrying out the changes the comparison
//! (`bookmoor_core::linkwarden`) asks for. The runner, the pairing and the
//! local side are shared with Linkwarden (`crate::linkwarden`). See
//! `docs/adapters/karakeep.md`.
//!
//! Karakeep ids are text, the comparison works with numbers: `Ids` hands out a
//! number per id and the connection keeps the table (`Linkwarden::ids`).
//!
//! A folder is a list, a bookmark is a link bookmark in a list. A bookmark can
//! be in several lists (or none) while a tree puts it in one place: inside
//! the synced lists the one with the lowest number counts, and a move takes
//! the bookmark out of its other synced lists. Lists outside the bound one
//! and bookmarks in no list are not looked at.

use crate::linkwarden::{ok_if_gone, ApiErr, Done, Linkwarden};
use bookmoor_core::linkwarden::{Pair, Plan, RItem, Ref, RemoteAction};
use openidconnect::reqwest;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// The icon of lists made here (Karakeep requires one).
const LIST_ICON: &str = "📁";

pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    pub fn new(base: &str, token: &str) -> Result<Client, String> {
        let http = reqwest::ClientBuilder::new()
            // The key must not follow a redirect to another host.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Client {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, ApiErr> {
        let url = format!("{}/api/v1{path}", self.base);
        let mut req = self
            .http
            .request(method.clone(), &url)
            .query(query)
            .bearer_auth(&self.token)
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req
                .header("Content-Type", "application/json")
                .body(b.to_string());
        }
        let resp = req.send().await.map_err(|e| ApiErr {
            status: None,
            msg: format!("cannot reach Karakeep at {}: {e}", self.base),
        })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| ApiErr {
            status: Some(status.as_u16()),
            msg: format!("{method} {path}: {e}"),
        })?;
        if status.is_redirection() {
            return Err(ApiErr {
                status: Some(status.as_u16()),
                msg: format!("{method} {path}: Karakeep redirected (use its final address)"),
            });
        }
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            let why = json
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| text.chars().take(120).collect());
            return Err(ApiErr {
                status: Some(status.as_u16()),
                msg: format!("{method} {path}: HTTP {} {why}", status.as_u16()),
            });
        }
        Ok(json)
    }

    /// All lists of the account (also the ones shared with it).
    pub async fn lists(&self) -> Result<Vec<Value>, ApiErr> {
        let v = self.send(reqwest::Method::GET, "/lists", &[], None).await?;
        v.get("lists")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| ApiErr {
                status: None,
                msg: "GET /lists: unexpected answer (is this Karakeep?)".into(),
            })
    }

    /// All bookmarks of one list, by cursor.
    async fn bookmarks(&self, list: &str) -> Result<Vec<Value>, ApiErr> {
        let path = format!("/lists/{list}/bookmarks");
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        // 100 bookmarks a page; this allows a million.
        for _ in 0..10_000 {
            let mut q = vec![("limit", "100")];
            if let Some(c) = cursor.as_deref() {
                q.push(("cursor", c));
            }
            let v = self.send(reqwest::Method::GET, &path, &q, None).await?;
            let page = v
                .get("bookmarks")
                .and_then(Value::as_array)
                .ok_or_else(|| ApiErr {
                    status: None,
                    msg: format!("GET {path}: the answer holds no list of bookmarks"),
                })?;
            all.extend(page.iter().cloned());
            let next = v
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(String::from);
            // No cursor, or one that does not move on, ends the loop.
            if next.is_none() || next == cursor || page.is_empty() {
                break;
            }
            cursor = next;
        }
        Ok(all)
    }

    /// A list; top-level without a parent.
    async fn create_list(&self, name: &str, parent: Option<&str>) -> Result<String, ApiErr> {
        let mut body = json!({ "name": name, "icon": LIST_ICON, "type": "manual" });
        if let Some(p) = parent {
            body["parentId"] = json!(p);
        }
        let v = self
            .send(reqwest::Method::POST, "/lists", &[], Some(body))
            .await?;
        id_of(&v, "list")
    }

    async fn update_list(&self, id: &str, body: Value) -> Result<(), ApiErr> {
        self.send(
            reqwest::Method::PATCH,
            &format!("/lists/{id}"),
            &[],
            Some(body),
        )
        .await
        .map(drop)
    }

    async fn delete_list(&self, id: &str) -> Result<(), ApiErr> {
        ok_if_gone(
            self.send(reqwest::Method::DELETE, &format!("/lists/{id}"), &[], None)
                .await,
        )
    }

    /// Makes the bookmark, or finds the one Karakeep already has for this
    /// address. Returns its id, whether it existed, and its own title.
    async fn create_bookmark(
        &self,
        title: &str,
        url: &str,
    ) -> Result<(String, bool, Option<String>), ApiErr> {
        let v = self
            .send(
                reqwest::Method::POST,
                "/bookmarks",
                &[],
                Some(json!({ "type": "link", "url": url, "title": title })),
            )
            .await?;
        let existed = v["alreadyExists"].as_bool().unwrap_or(false);
        let own = v["title"].as_str().map(String::from);
        id_of(&v, "bookmark").map(|id| (id, existed, own))
    }

    async fn update_bookmark(&self, id: &str, body: Value) -> Result<(), ApiErr> {
        self.send(
            reqwest::Method::PATCH,
            &format!("/bookmarks/{id}"),
            &[],
            Some(body),
        )
        .await
        .map(drop)
    }

    async fn add_to_list(&self, list: &str, bookmark: &str) -> Result<(), ApiErr> {
        self.send(
            reqwest::Method::PUT,
            &format!("/lists/{list}/bookmarks/{bookmark}"),
            &[],
            None,
        )
        .await
        .map(drop)
    }

    async fn remove_from_list(&self, list: &str, bookmark: &str) -> Result<(), ApiErr> {
        ok_if_gone(
            self.send(
                reqwest::Method::DELETE,
                &format!("/lists/{list}/bookmarks/{bookmark}"),
                &[],
                None,
            )
            .await,
        )
    }
}

fn id_of(v: &Value, what: &str) -> Result<String, ApiErr> {
    v["id"].as_str().map(String::from).ok_or_else(|| ApiErr {
        status: None,
        msg: format!("Karakeep did not say which {what} it made"),
    })
}

/// Numbers for Karakeep's text ids, stable from run to run.
#[derive(Debug, Default)]
pub struct Ids {
    by_text: BTreeMap<String, i64>,
    by_num: BTreeMap<i64, String>,
    next: i64,
}

impl Ids {
    pub fn new(table: &BTreeMap<String, i64>) -> Ids {
        Ids {
            by_text: table.clone(),
            by_num: table.iter().map(|(k, v)| (*v, k.clone())).collect(),
            next: table.values().copied().max().unwrap_or(0) + 1,
        }
    }

    pub fn num(&mut self, text: &str) -> i64 {
        if let Some(n) = self.by_text.get(text) {
            return *n;
        }
        let n = self.next;
        self.next += 1;
        self.by_text.insert(text.to_string(), n);
        self.by_num.insert(n, text.to_string());
        n
    }

    pub fn text(&self, num: i64) -> Option<&str> {
        self.by_num.get(&num).map(String::as_str)
    }

    /// The table without the numbers nobody refers to any more.
    pub fn table(&self, used: &BTreeSet<i64>) -> BTreeMap<String, i64> {
        self.by_text
            .iter()
            .filter(|(_, n)| used.contains(n))
            .map(|(k, n)| (k.clone(), *n))
            .collect()
    }
}

/// What Karakeep holds below the bound list.
pub struct KRemote {
    pub root: i64,
    pub items: BTreeMap<Ref, RItem>,
    pub ids: Ids,
    /// Bookmarks whose address cannot be a bookmark's here.
    pub unlinkable: BTreeSet<i64>,
    /// The lists (of the synced ones) each bookmark is in.
    in_lists: BTreeMap<i64, Vec<i64>>,
}

fn is_manual(l: &Value) -> bool {
    l["type"].as_str() != Some("smart")
}

pub async fn fetch(client: &Client, cfg: &Linkwarden) -> Result<KRemote, String> {
    let mut lists = client.lists().await.map_err(|e| e.to_string())?;
    lists.retain(|l| l["id"].as_str().is_some());
    lists.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let mut ids = Ids::new(&cfg.ids);
    let mut created_root = false;
    let root_text: String = match cfg.collection_id {
        Some(n) => {
            let text = ids.text(n).map(String::from);
            match text {
                Some(t) if lists.iter().any(|l| l["id"].as_str() == Some(t.as_str()) && is_manual(l)) => {
                    let viewer = lists.iter().any(|l| {
                        l["id"].as_str() == Some(t.as_str()) && l["userRole"].as_str() == Some("viewer")
                    });
                    if viewer {
                        return Err(format!(
                            "the Karakeep list \"{}\" can only be viewed with this key; nothing was changed",
                            cfg.collection
                        ));
                    }
                    t
                }
                _ => {
                    return Err(format!(
                        "the Karakeep list \"{}\" is gone (deleted, or this key cannot see it). Nothing was changed; remove this connection and add it again to start over",
                        cfg.collection
                    ))
                }
            }
        }
        None => {
            let hit = lists.iter().find(|l| {
                l["parentId"].is_null()
                    && is_manual(l)
                    && l["name"].as_str() == Some(cfg.collection.as_str())
                    && l["userRole"].as_str().is_none_or(|r| r == "owner")
            });
            match hit {
                Some(l) => l["id"].as_str().unwrap_or_default().to_string(),
                None => {
                    created_root = true;
                    client
                        .create_list(&cfg.collection, None)
                        .await
                        .map_err(|e| e.to_string())?
                }
            }
        }
    };
    let root = ids.num(&root_text);
    let mut kids: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for l in &lists {
        if let (Some(p), true) = (l["parentId"].as_str(), is_manual(l)) {
            kids.entry(p).or_default().push(l);
        }
    }
    let mut out = KRemote {
        root,
        items: BTreeMap::new(),
        ids,
        unlinkable: BTreeSet::new(),
        in_lists: BTreeMap::new(),
    };
    // Bookmark id -> (lists it is in, title, address).
    let mut seen: BTreeMap<String, (Vec<i64>, String, String)> = BTreeMap::new();
    let mut todo = vec![root_text];
    let mut visited = BTreeSet::new();
    while let Some(lid) = todo.pop() {
        if !visited.insert(lid.clone()) {
            continue;
        }
        let list_num = out.ids.num(&lid);
        if !(created_root && list_num == root) {
            for b in client.bookmarks(&lid).await.map_err(|e| e.to_string())? {
                // Notes and uploaded files have no address: nothing to bookmark.
                let (Some(id), Some("link"), Some(url)) = (
                    b["id"].as_str(),
                    b["content"]["type"].as_str(),
                    b["content"]["url"].as_str(),
                ) else {
                    continue;
                };
                let title = [b["title"].as_str(), b["content"]["title"].as_str()]
                    .into_iter()
                    .flatten()
                    .map(str::trim)
                    .find(|t| !t.is_empty())
                    .unwrap_or(url.trim())
                    .to_string();
                let e = seen.entry(id.to_string()).or_insert((
                    Vec::new(),
                    title,
                    url.trim().to_string(),
                ));
                if !e.0.contains(&list_num) {
                    e.0.push(list_num);
                }
            }
        }
        for c in kids.get(lid.as_str()).into_iter().flatten() {
            let cid = c["id"].as_str().unwrap_or_default();
            let n = out.ids.num(cid);
            out.items.insert(
                Ref::Collection(n),
                RItem {
                    parent: list_num,
                    title: c["name"].as_str().unwrap_or("").to_string(),
                    url: None,
                },
            );
            todo.push(cid.to_string());
        }
    }
    // A bookmark in several synced lists stays where the last run put it, as
    // long as it is still in that list; otherwise the lowest-numbered one.
    let folder_of: BTreeMap<&str, i64> = cfg
        .pairs
        .iter()
        .filter_map(|p| match p.remote {
            Ref::Collection(c) => Some((p.node.as_str(), c)),
            Ref::Link(_) => None,
        })
        .collect();
    let was_in: BTreeMap<i64, i64> = cfg
        .pairs
        .iter()
        .filter_map(|p| match p.remote {
            Ref::Link(n) => Some((n, folder_of.get(p.parent.as_str()).copied().unwrap_or(root))),
            Ref::Collection(_) => None,
        })
        .collect();
    for (id, (mut in_lists, title, url)) in seen {
        in_lists.sort();
        let n = out.ids.num(&id);
        if let Some(i) = was_in
            .get(&n)
            .and_then(|w| in_lists.iter().position(|l| l == w))
        {
            in_lists.swap(0, i);
        }
        if !crate::linkwarden::Service::Karakeep.can_hold(&url) {
            out.unlinkable.insert(n);
            continue;
        }
        out.items.insert(
            Ref::Link(n),
            RItem {
                parent: in_lists[0],
                title,
                url: Some(url),
            },
        );
        out.in_lists.insert(n, in_lists);
    }
    Ok(out)
}

pub(crate) async fn run_remote(
    client: &Client,
    plan: &Plan,
    pairs: &[Pair],
    remote: &mut KRemote,
    root_node: &str,
) -> Done {
    let mut done = Done::default();
    for p in pairs {
        done.refs.insert(p.node.clone(), p.remote);
    }
    for (n, d) in &plan.desired {
        if let Some(r) = d.remote {
            done.refs.insert(n.clone(), r);
        }
    }
    let mut claimed: BTreeSet<Ref> = remote.items.keys().copied().collect();
    for a in &plan.remote {
        let node = match a {
            RemoteAction::Create { node, .. }
            | RemoteAction::Update { node, .. }
            | RemoteAction::Delete { node } => node.clone(),
        };
        // The Karakeep list a node's folder is (the bound list for the root).
        let list_of = |done: &Done, remote: &KRemote, node: &str| -> Option<String> {
            if node == root_node {
                return remote.ids.text(remote.root).map(String::from);
            }
            match done.refs.get(node) {
                Some(Ref::Collection(c)) if !done.failed.contains(node) => {
                    remote.ids.text(*c).map(String::from)
                }
                _ => None,
            }
        };
        let res: Result<Option<Ref>, String> = async {
            match a {
                RemoteAction::Create {
                    parent,
                    folder,
                    title,
                    url,
                    ..
                } => {
                    let pl =
                        list_of(&done, remote, parent).ok_or("its folder could not be created")?;
                    if *folder {
                        let id = client
                            .create_list(title, Some(&pl))
                            .await
                            .map_err(|e| e.to_string())?;
                        let n = remote.ids.num(&id);
                        claimed.insert(Ref::Collection(n));
                        Ok(Some(Ref::Collection(n)))
                    } else {
                        let url = url.clone().unwrap_or_default();
                        let (id, existed, own) = client
                            .create_bookmark(title, &url)
                            .await
                            .map_err(|e| e.to_string())?;
                        let n = remote.ids.num(&id);
                        // Karakeep has one bookmark per address: when another
                        // node has it, this one stays out (not an error, it
                        // would only repeat).
                        if !claimed.insert(Ref::Link(n)) {
                            done.left_out += 1;
                            return Ok(None);
                        }
                        if existed && own.as_deref() != Some(title.as_str()) {
                            client
                                .update_bookmark(&id, json!({ "title": title }))
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        client
                            .add_to_list(&pl, &id)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(Some(Ref::Link(n)))
                    }
                }
                RemoteAction::Update {
                    title, url, parent, ..
                } => {
                    let r = *done.refs.get(&node).ok_or("not paired")?;
                    let new_list = match parent {
                        Some(p) => Some(
                            list_of(&done, remote, p).ok_or("its new folder is not in Karakeep")?,
                        ),
                        None => None,
                    };
                    match r {
                        Ref::Collection(c) => {
                            let id = remote.ids.text(c).ok_or("not in Karakeep")?.to_string();
                            let mut body = json!({});
                            if let Some(t) = title {
                                body["name"] = json!(t);
                            }
                            if let Some(p) = &new_list {
                                body["parentId"] = json!(p);
                            }
                            client
                                .update_list(&id, body)
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        Ref::Link(b) => {
                            let id = remote.ids.text(b).ok_or("not in Karakeep")?.to_string();
                            if title.is_some() || url.is_some() {
                                let mut body = json!({});
                                if let Some(t) = title {
                                    body["title"] = json!(t);
                                }
                                if let Some(u) = url {
                                    body["url"] = json!(u);
                                }
                                client
                                    .update_bookmark(&id, body)
                                    .await
                                    .map_err(|e| e.to_string())?;
                            }
                            if let Some(target) = &new_list {
                                let target_n = remote.ids.num(target);
                                let had = remote.in_lists.get(&b).cloned().unwrap_or_default();
                                // In the new list first, so a failure midway
                                // never leaves the bookmark in no list.
                                if !had.contains(&target_n) {
                                    client
                                        .add_to_list(target, &id)
                                        .await
                                        .map_err(|e| e.to_string())?;
                                }
                                for l in had.into_iter().filter(|l| *l != target_n) {
                                    if let Some(lt) = remote.ids.text(l) {
                                        client
                                            .remove_from_list(lt, &id)
                                            .await
                                            .map_err(|e| e.to_string())?;
                                    }
                                }
                            }
                        }
                    }
                    Ok(None)
                }
                RemoteAction::Delete { .. } => {
                    match *done.refs.get(&node).ok_or("not paired")? {
                        Ref::Collection(c) => {
                            if let Some(id) = remote.ids.text(c) {
                                client.delete_list(id).await.map_err(|e| e.to_string())?;
                            }
                        }
                        // The bookmark itself stays in Karakeep (with its tags,
                        // notes and archives); it only leaves the synced lists.
                        Ref::Link(b) => {
                            let had = remote.in_lists.get(&b).cloned().unwrap_or_default();
                            if let Some(id) = remote.ids.text(b).map(String::from) {
                                for l in had {
                                    if let Some(lt) = remote.ids.text(l) {
                                        client
                                            .remove_from_list(lt, &id)
                                            .await
                                            .map_err(|e| e.to_string())?;
                                    }
                                }
                            }
                        }
                    }
                    Ok(None)
                }
            }
        }
        .await;
        match res {
            Ok(Some(r)) => {
                done.refs.insert(node, r);
            }
            Ok(None) => {}
            Err(e) => {
                done.failed.insert(node.clone());
                if done.errors.len() < 5 {
                    done.errors.push(e);
                }
            }
        }
    }
    done
}
