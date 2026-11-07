//! Two-way sync of one profile (or one folder of it) with a Linkwarden
//! collection or a Karakeep list. The comparison lives in
//! `bookmoor_core::linkwarden` (used for both services); this module talks to
//! Linkwarden, applies the plan to both sides and keeps the pairing between
//! them, `crate::karakeep` talks to Karakeep. See
//! `docs/adapters/linkwarden.md` and `docs/adapters/karakeep.md`.

use crate::api::{AppState, Backup, Shared};
use crate::auth::random_id;
use crate::engine::UiEdit;
use bookmoor_core::linkwarden::{
    plan, Desired, LNode, LocalAction, Mode, Pair, Plan, RItem, Ref, RemoteAction,
};
use bookmoor_core::model::Kind;
use openidconnect::reqwest;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Most Linkwarden connections an admin may set up.
pub const MAX_LINKS: usize = 10;
pub const DEFAULT_INTERVAL: u64 = 300;
pub const MIN_INTERVAL: u64 = 30;
pub const MAX_INTERVAL: u64 = 86_400;
/// Longest wait between two attempts after an error.
const RETRY_SECS: u64 = 120;

pub fn default_interval() -> u64 {
    DEFAULT_INTERVAL
}

/// Which link manager a connection talks to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    #[default]
    Linkwarden,
    Karakeep,
}

impl Service {
    /// The name shown to people.
    pub fn name(self) -> &'static str {
        match self {
            Service::Linkwarden => "Linkwarden",
            Service::Karakeep => "Karakeep",
        }
    }

    /// What it calls the thing a folder becomes.
    pub fn container(self) -> &'static str {
        match self {
            Service::Linkwarden => "collection",
            Service::Karakeep => "list",
        }
    }

    /// Prefix of the ids of its connections.
    pub fn id_prefix(self) -> &'static str {
        match self {
            Service::Linkwarden => "lw_",
            Service::Karakeep => "kk_",
        }
    }

    /// Whether a bookmark's address can be a link there.
    pub fn can_hold(self, url: &str) -> bool {
        let u = url.trim();
        match self {
            Service::Linkwarden => is_linkable(u),
            // Karakeep crawls what it is given: only web addresses.
            Service::Karakeep => {
                let l = u.to_ascii_lowercase();
                (l.starts_with("http://") || l.starts_with("https://")) && is_linkable(u)
            }
        }
    }
}

fn is_linkwarden(s: &Service) -> bool {
    *s == Service::Linkwarden
}

/// One connection, kept in the state file. It holds the access token, which
/// is stored as it is (the state file is readable by the server user only).
/// The type keeps the name of the first service it served; `service` says
/// which one a connection is for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Linkwarden {
    pub id: String,
    #[serde(default, skip_serializing_if = "is_linkwarden")]
    pub service: Service,
    pub profile_id: String,
    /// Folder titles from the profile's root (empty = the whole profile).
    #[serde(default)]
    pub folder: Vec<String>,
    /// Base URL of Linkwarden, e.g. `https://links.example.com`.
    pub url: String,
    /// Linkwarden access token (Settings → Access Tokens).
    pub token: String,
    /// Name of the Linkwarden collection that holds everything; made when it
    /// does not exist yet.
    pub collection: String,
    /// Id of that collection once it is known (a rename in Linkwarden is
    /// followed; a deleted collection stops the sync).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_id: Option<i64>,
    #[serde(default)]
    pub mode: Mode,
    /// Seconds between checks for changes made in Linkwarden.
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default)]
    pub pairs: Vec<Pair>,
    /// Karakeep only: its ids are text, the comparison works with numbers.
    /// This is the table between them, kept so that a pair keeps its number.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ids: BTreeMap<String, i64>,
}

impl Linkwarden {
    /// Trim and check what an admin typed.
    pub fn check(&mut self) -> Result<(), String> {
        self.url = self.url.trim().trim_end_matches('/').to_string();
        self.token = self.token.trim().to_string();
        self.collection = self.collection.trim().to_string();
        let name = self.service.name();
        let rest = self
            .url
            .strip_prefix("https://")
            .or_else(|| self.url.strip_prefix("http://"));
        if !rest.is_some_and(|r| !r.is_empty() && !r.contains(char::is_whitespace)) {
            return Err(format!(
                "{name} address: must start with https:// or http://"
            ));
        }
        if self.token.is_empty() {
            return Err("access token: required".into());
        }
        let n = self.collection.chars().count();
        // Karakeep lists have names of up to 100 characters.
        let max = if self.service == Service::Karakeep {
            100
        } else {
            200
        };
        if n == 0 || n > max {
            return Err(format!(
                "{}: 1 to {max} characters",
                self.service.container()
            ));
        }
        if !(MIN_INTERVAL..=MAX_INTERVAL).contains(&self.interval) {
            return Err(format!(
                "interval: {MIN_INTERVAL} to {MAX_INTERVAL} seconds"
            ));
        }
        Ok(())
    }
}

/// How the last run went; kept in memory only.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub running: bool,
    /// Seconds since the epoch.
    pub last_run: Option<u64>,
    pub last_ok: Option<u64>,
    /// What the last run did, or what went wrong.
    pub message: String,
    pub failed: bool,
    /// Revision of the tree after the last good run: a different one means
    /// Bookmoor has changes to send.
    pub rev: Option<u64>,
    pub next_at: u64,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---- Linkwarden's HTTP API -------------------------------------------------

#[derive(Debug)]
pub struct ApiErr {
    pub status: Option<u16>,
    pub msg: String,
}

impl std::fmt::Display for ApiErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    pub fn new(base: &str, token: &str) -> Result<Client, String> {
        let http = reqwest::ClientBuilder::new()
            // The token must not follow a redirect to another host.
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
        body: Option<Value>,
    ) -> Result<Value, ApiErr> {
        let url = format!("{}{path}", self.base);
        let mut req = self
            .http
            .request(method.clone(), &url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req
                .header("Content-Type", "application/json")
                .body(b.to_string());
        }
        let resp = req.send().await.map_err(|e| ApiErr {
            status: None,
            msg: format!("cannot reach Linkwarden at {}: {e}", self.base),
        })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| ApiErr {
            status: Some(status.as_u16()),
            msg: format!("{method} {path}: {e}"),
        })?;
        if status.is_redirection() {
            return Err(ApiErr {
                status: Some(status.as_u16()),
                msg: format!("{method} {path}: Linkwarden redirected (use its final address)"),
            });
        }
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            let why = json
                .get("response")
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

    pub async fn collections(&self) -> Result<Vec<Value>, ApiErr> {
        let v = self
            .send(reqwest::Method::GET, "/api/v1/collections", None)
            .await?;
        v.get("response")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| ApiErr {
                status: None,
                msg: "GET /api/v1/collections: unexpected answer (is this Linkwarden?)".into(),
            })
    }

    /// All links of one collection (oldest first, by cursor).
    pub async fn links(&self, collection: i64) -> Result<Vec<Value>, ApiErr> {
        match self.links_via("/api/v1/search", collection).await {
            Err(ApiErr {
                status: Some(404 | 405),
                ..
            }) => self.links_via("/api/v1/links", collection).await,
            other => other,
        }
    }

    async fn links_via(&self, route: &str, collection: i64) -> Result<Vec<Value>, ApiErr> {
        let mut all: Vec<Value> = Vec::new();
        let mut seen = BTreeSet::new();
        let mut cursor: Option<i64> = None;
        // Linkwarden pages hold 50 links by default; this allows 500 000.
        for _ in 0..10_000 {
            let mut path = format!("{route}?collectionId={collection}&sort=1");
            if let Some(c) = cursor {
                path.push_str(&format!("&cursor={c}"));
            }
            let v = self.send(reqwest::Method::GET, &path, None).await?;
            // The search route: {data: {links, nextCursor}}; the old one:
            // {response: [links]}.
            let (page, next): (Vec<Value>, Option<i64>) =
                if let Some(d) = v.get("data").filter(|d| d.is_object()) {
                    (
                        d.get("links")
                            .and_then(Value::as_array)
                            .cloned()
                            .ok_or_else(|| ApiErr {
                                status: None,
                                msg: format!("GET {route}: the answer holds no list of links"),
                            })?,
                        d.get("nextCursor").and_then(Value::as_i64),
                    )
                } else if let Some(r) = v.get("response").and_then(Value::as_array) {
                    let last = r.last().and_then(|l| l["id"].as_i64());
                    (r.clone(), last)
                } else if v.get("data").is_some() {
                    // Nothing found.
                    (Vec::new(), None)
                } else {
                    return Err(ApiErr {
                        status: None,
                        msg: format!("GET {route}: unexpected answer"),
                    });
                };
            let fresh = page
                .iter()
                .filter(|l| l["id"].as_i64().is_some_and(|i| seen.insert(i)))
                .count();
            all.extend(page.into_iter().filter(|l| l["id"].is_i64()));
            match next {
                // A page of nothing new ends the loop (the old route repeats
                // its last page when there is no more).
                Some(n) if fresh > 0 => cursor = Some(n),
                _ => break,
            }
        }
        Ok(all)
    }

    pub async fn create_collection(&self, name: &str, parent: Option<i64>) -> Result<i64, ApiErr> {
        let mut body = json!({ "name": name });
        if let Some(p) = parent {
            body["parentId"] = json!(p);
        }
        let v = self
            .send(reqwest::Method::POST, "/api/v1/collections", Some(body))
            .await?;
        id_of(&v, "collection")
    }

    pub async fn update_collection(&self, id: i64, body: Value) -> Result<(), ApiErr> {
        self.send(
            reqwest::Method::PUT,
            &format!("/api/v1/collections/{id}"),
            Some(body),
        )
        .await
        .map(drop)
    }

    pub async fn delete_collection(&self, id: i64) -> Result<(), ApiErr> {
        ok_if_gone(
            self.send(
                reqwest::Method::DELETE,
                &format!("/api/v1/collections/{id}"),
                None,
            )
            .await,
        )
    }

    pub async fn create_link(&self, body: Value) -> Result<i64, ApiErr> {
        let v = self
            .send(reqwest::Method::POST, "/api/v1/links", Some(body))
            .await?;
        id_of(&v, "link")
    }

    pub async fn update_link(&self, id: i64, body: Value) -> Result<(), ApiErr> {
        self.send(
            reqwest::Method::PUT,
            &format!("/api/v1/links/{id}"),
            Some(body),
        )
        .await
        .map(drop)
    }

    pub async fn delete_link(&self, id: i64) -> Result<(), ApiErr> {
        ok_if_gone(
            self.send(
                reqwest::Method::DELETE,
                &format!("/api/v1/links/{id}"),
                None,
            )
            .await,
        )
    }
}

fn id_of(v: &Value, what: &str) -> Result<i64, ApiErr> {
    v["response"]["id"].as_i64().ok_or_else(|| ApiErr {
        status: None,
        msg: format!("Linkwarden did not say which {what} it made"),
    })
}

/// Deleting what is already gone is fine.
pub(crate) fn ok_if_gone(r: Result<Value, ApiErr>) -> Result<(), ApiErr> {
    match r {
        Ok(_) => Ok(()),
        Err(ApiErr {
            status: Some(404), ..
        }) => Ok(()),
        Err(e) => Err(e),
    }
}

// ---- reading both sides ------------------------------------------------------

/// What Linkwarden holds below the bound collection.
struct LwRemote {
    root: i64,
    items: BTreeMap<Ref, RItem>,
    raw: BTreeMap<Ref, Value>,
    /// Owner of each collection (a link's target collection must name it).
    owner: BTreeMap<i64, i64>,
    /// Links whose address cannot be a bookmark's.
    unlinkable: BTreeSet<i64>,
}

pub(crate) fn is_linkable(url: &str) -> bool {
    let u = url.trim();
    u.len() > 3 && u.contains(':') && !u.contains(char::is_whitespace)
}

async fn fetch(client: &Client, cfg: &Linkwarden) -> Result<LwRemote, String> {
    let cols = client.collections().await.map_err(|e| e.to_string())?;
    let name = |c: &Value| c["name"].as_str().unwrap_or("").to_string();
    let parent = |c: &Value| c["parentId"].as_i64();
    let mut created_root = false;
    let root = match cfg.collection_id {
        Some(id) => {
            if !cols.iter().any(|c| c["id"].as_i64() == Some(id)) {
                return Err(format!(
                    "the Linkwarden collection \"{}\" is gone (deleted, or this token cannot see it). Nothing was changed; remove this connection and add it again to start over",
                    cfg.collection
                ));
            }
            id
        }
        None => {
            let mut hits: Vec<i64> = cols
                .iter()
                .filter(|c| parent(c).is_none() && name(c) == cfg.collection)
                .filter_map(|c| c["id"].as_i64())
                .collect();
            hits.sort();
            match hits.first() {
                Some(id) => *id,
                None => {
                    created_root = true;
                    client
                        .create_collection(&cfg.collection, None)
                        .await
                        .map_err(|e| e.to_string())?
                }
            }
        }
    };
    let mut kids: BTreeMap<i64, Vec<&Value>> = BTreeMap::new();
    for c in &cols {
        if let Some(p) = parent(c) {
            kids.entry(p).or_default().push(c);
        }
    }
    let mut out = LwRemote {
        root,
        items: BTreeMap::new(),
        raw: BTreeMap::new(),
        owner: BTreeMap::new(),
        unlinkable: BTreeSet::new(),
    };
    if let Some(r) = cols.iter().find(|c| c["id"].as_i64() == Some(root)) {
        if let Some(o) = r["ownerId"].as_i64() {
            out.owner.insert(root, o);
        }
    }
    let mut todo = vec![root];
    let mut visited = BTreeSet::new();
    while let Some(cid) = todo.pop() {
        if !visited.insert(cid) {
            continue;
        }
        if !created_root || cid != root {
            for l in client.links(cid).await.map_err(|e| e.to_string())? {
                let (Some(id), Some(url)) = (l["id"].as_i64(), l["url"].as_str()) else {
                    // A PDF or an image has no address: nothing to bookmark.
                    continue;
                };
                if !is_linkable(url) {
                    out.unlinkable.insert(id);
                    continue;
                }
                let title = match l["name"].as_str().map(str::trim) {
                    Some(n) if !n.is_empty() => n.to_string(),
                    _ => url.to_string(),
                };
                out.items.insert(
                    Ref::Link(id),
                    RItem {
                        parent: cid,
                        title,
                        url: Some(url.trim().to_string()),
                    },
                );
                out.raw.insert(Ref::Link(id), l);
            }
        }
        for c in kids.get(&cid).into_iter().flatten() {
            let Some(id) = c["id"].as_i64() else { continue };
            out.items.insert(
                Ref::Collection(id),
                RItem {
                    parent: cid,
                    title: name(c),
                    url: None,
                },
            );
            if let Some(o) = c["ownerId"].as_i64() {
                out.owner.insert(id, o);
            }
            out.raw.insert(Ref::Collection(id), (*c).clone());
            todo.push(id);
        }
    }
    Ok(out)
}

/// A connection's other side as read for one run.
enum Backend {
    Linkwarden(Client, LwRemote),
    Karakeep(crate::karakeep::Client, crate::karakeep::KRemote),
}

impl Backend {
    async fn open(cfg: &Linkwarden) -> Result<Backend, String> {
        match cfg.service {
            Service::Linkwarden => {
                let client = Client::new(&cfg.url, &cfg.token)?;
                let remote = fetch(&client, cfg).await?;
                Ok(Backend::Linkwarden(client, remote))
            }
            Service::Karakeep => {
                let client = crate::karakeep::Client::new(&cfg.url, &cfg.token)?;
                let remote = crate::karakeep::fetch(&client, cfg).await?;
                Ok(Backend::Karakeep(client, remote))
            }
        }
    }

    fn root(&self) -> i64 {
        match self {
            Backend::Linkwarden(_, r) => r.root,
            Backend::Karakeep(_, r) => r.root,
        }
    }

    fn items(&self) -> &BTreeMap<Ref, RItem> {
        match self {
            Backend::Linkwarden(_, r) => &r.items,
            Backend::Karakeep(_, r) => &r.items,
        }
    }

    /// Whether this link's address cannot be a bookmark's.
    fn unlinkable(&self, link: i64) -> bool {
        match self {
            Backend::Linkwarden(_, r) => r.unlinkable.contains(&link),
            Backend::Karakeep(_, r) => r.unlinkable.contains(&link),
        }
    }

    async fn run(&mut self, plan: &Plan, pairs: &[Pair], root_node: &str) -> Done {
        match self {
            Backend::Linkwarden(c, r) => run_remote(c, plan, pairs, r, root_node).await,
            Backend::Karakeep(c, r) => {
                crate::karakeep::run_remote(c, plan, pairs, r, root_node).await
            }
        }
    }

    /// Karakeep: the id table for what is paired after the run and the root.
    fn ids_to_keep(&self, pairs: &[Pair]) -> BTreeMap<String, i64> {
        match self {
            Backend::Linkwarden(..) => BTreeMap::new(),
            Backend::Karakeep(_, r) => {
                let mut used: BTreeSet<i64> = pairs
                    .iter()
                    .map(|p| match p.remote {
                        Ref::Collection(i) | Ref::Link(i) => i,
                    })
                    .collect();
                used.insert(r.root);
                r.ids.table(&used)
            }
        }
    }
}

/// Everything read from the tree for one run.
pub(crate) struct Local {
    cfg: Linkwarden,
    root: String,
    nodes: BTreeMap<String, LNode>,
    /// Bookmarks left out because Linkwarden cannot hold their address.
    skipped: usize,
    before: Vec<bookmoor_core::wire::WireNode>,
    rev: u64,
    /// The browser's own top-level folders (toolbar, other, mobile): they
    /// are never renamed, moved or removed by what Linkwarden does.
    protected: BTreeSet<String>,
    /// Bookmarks of the tree left out for their address.
    unlinkable: BTreeSet<String>,
}

fn read_local(st: &AppState, id: &str) -> Result<Local, String> {
    let cfg = st
        .linkwarden
        .iter()
        .find(|l| l.id == id)
        .cloned()
        .ok_or("this connection no longer exists")?;
    let engine = st
        .engines
        .get(&cfg.profile_id)
        .ok_or("the profile of this connection is gone")?;
    let tree = engine.tree();
    let mut root = tree
        .nodes
        .iter()
        .find(|n| n.parent.is_none())
        .map(|n| n.id.clone())
        .ok_or("the profile has no tree")?;
    for title in &cfg.folder {
        root = tree
            .nodes
            .iter()
            .find(|n| {
                n.kind == Kind::Folder
                    && n.parent.as_deref() == Some(root.as_str())
                    && n.title == *title
            })
            .map(|n| n.id.clone())
            .ok_or_else(|| {
                format!(
                    "the folder \"{}\" no longer exists in the profile",
                    cfg.folder.join("/")
                )
            })?;
    }
    let tree_root = tree
        .nodes
        .iter()
        .find(|n| n.parent.is_none())
        .map(|n| n.id.as_str());
    let protected: BTreeSet<String> = if tree_root == Some(root.as_str()) {
        tree.nodes
            .iter()
            .filter(|n| n.parent.as_deref() == tree_root)
            .map(|n| n.id.clone())
            .collect()
    } else {
        BTreeSet::new()
    };
    // Parent-first order: a node is in scope when its parent is.
    let mut inside: BTreeSet<String> = BTreeSet::new();
    inside.insert(root.clone());
    let mut nodes = BTreeMap::new();
    let mut skipped = 0;
    let mut unlinkable = BTreeSet::new();
    // Karakeep keeps one bookmark per address: of several copies the one that
    // is paired already stays, otherwise the first; the others stay out.
    let paired_nodes: BTreeSet<&str> = cfg.pairs.iter().map(|p| p.node.as_str()).collect();
    let mut keeper_of: BTreeMap<String, String> = BTreeMap::new();
    if cfg.service == Service::Karakeep {
        for n in &tree.nodes {
            if let (Kind::Bookmark, true, Some(u)) = (
                n.kind,
                paired_nodes.contains(n.id.as_str()),
                n.url.as_deref(),
            ) {
                keeper_of
                    .entry(u.trim().to_string())
                    .or_insert_with(|| n.id.clone());
            }
        }
    }
    for n in &tree.nodes {
        let Some(p) = n.parent.as_deref() else {
            continue;
        };
        if !inside.contains(p) {
            continue;
        }
        match n.kind {
            Kind::Folder => {
                inside.insert(n.id.clone());
            }
            Kind::Bookmark => {
                let held = n.url.as_deref().is_some_and(|u| cfg.service.can_hold(u));
                let twin = held
                    && cfg.service == Service::Karakeep
                    && *keeper_of
                        .entry(n.url.as_deref().unwrap_or("").trim().to_string())
                        .or_insert_with(|| n.id.clone())
                        != n.id;
                if !held || twin {
                    skipped += 1;
                    unlinkable.insert(n.id.clone());
                    continue;
                }
            }
        }
        nodes.insert(
            n.id.clone(),
            LNode {
                parent: p.to_string(),
                folder: n.kind == Kind::Folder,
                title: n.title.clone(),
                url: n.url.as_ref().map(|u| u.trim().to_string()),
                position: n.position.clone(),
            },
        );
    }
    Ok(Local {
        cfg,
        root,
        nodes,
        skipped,
        rev: tree.rev,
        protected,
        unlinkable,
        before: tree.nodes,
    })
}

// ---- doing the changes -------------------------------------------------------

#[derive(Default)]
pub(crate) struct Done {
    pub refs: BTreeMap<String, Ref>,
    pub failed: BTreeSet<String>,
    /// Changes put off to the next run because Bookmoor changed meanwhile.
    pub deferred: BTreeSet<String>,
    pub errors: Vec<String>,
    /// Bookmarks not created because the other side cannot have them.
    pub left_out: usize,
}

fn collection_body(raw: &Value, id: i64, name: &str, parent: i64) -> Value {
    let mut b = json!({ "id": id, "name": name, "parentId": parent });
    for k in ["description", "color"] {
        if let Some(s) = raw[k].as_str() {
            b[k] = json!(s);
        }
    }
    if let Some(p) = raw["isPublic"].as_bool() {
        b["isPublic"] = json!(p);
    }
    for k in ["icon", "iconWeight"] {
        if let Some(s) = raw[k].as_str() {
            b[k] = json!(s);
        }
    }
    let members: Vec<Value> = raw["members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(json!({
                "userId": m["userId"].as_i64()?,
                "canCreate": m["canCreate"].as_bool().unwrap_or(false),
                "canUpdate": m["canUpdate"].as_bool().unwrap_or(false),
                "canDelete": m["canDelete"].as_bool().unwrap_or(false),
            }))
        })
        .collect();
    b["members"] = json!(members);
    b
}

fn link_body(raw: &Value, id: i64, name: &str, url: &str, coll: i64, owner: i64) -> Value {
    let mut b = json!({
        "id": id,
        "name": name,
        "url": url,
        "collection": { "id": coll, "ownerId": owner },
    });
    for k in ["description", "icon", "iconWeight", "color"] {
        if let Some(s) = raw[k].as_str() {
            b[k] = json!(s);
        }
    }
    // The tags stay as they are: Linkwarden-only metadata is never touched.
    let tags: Vec<Value> = raw["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some(json!({
                "id": t["id"].as_i64(),
                "name": t["name"].as_str()?,
            }))
        })
        .collect();
    b["tags"] = json!(tags);
    b
}

async fn run_remote(
    client: &Client,
    plan: &Plan,
    pairs: &[Pair],
    remote: &mut LwRemote,
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
    // Where Linkwarden has an item now (follows creates, moves and renames).
    let coll = |done: &Done, node: &str| -> Option<i64> {
        if node == root_node {
            return Some(remote.root);
        }
        match done.refs.get(node) {
            Some(Ref::Collection(c)) if !done.failed.contains(node) => Some(*c),
            _ => None,
        }
    };
    for a in &plan.remote {
        let node = match a {
            RemoteAction::Create { node, .. }
            | RemoteAction::Update { node, .. }
            | RemoteAction::Delete { node } => node.clone(),
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
                    let pc = coll(&done, parent).ok_or("its folder could not be created")?;
                    if *folder {
                        let id = client
                            .create_collection(title, Some(pc))
                            .await
                            .map_err(|e| e.to_string())?;
                        remote
                            .owner
                            .insert(id, remote.owner.get(&remote.root).copied().unwrap_or(0));
                        Ok(Some(Ref::Collection(id)))
                    } else {
                        let id = client
                            .create_link(json!({
                                "type": "url",
                                "name": title,
                                "url": url,
                                "collection": { "id": pc },
                            }))
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(Some(Ref::Link(id)))
                    }
                }
                RemoteAction::Update {
                    title, url, parent, ..
                } => {
                    let r = *done.refs.get(&node).ok_or("not paired")?;
                    let item = remote.items.get(&r).ok_or("not in Linkwarden")?.clone();
                    let raw = remote.raw.get(&r).cloned().unwrap_or(Value::Null);
                    let new_parent = match parent {
                        Some(p) => coll(&done, p).ok_or("its new folder is not in Linkwarden")?,
                        None => item.parent,
                    };
                    let name = title.clone().unwrap_or_else(|| {
                        raw["name"]
                            .as_str()
                            .map_or(item.title.clone(), str::to_string)
                    });
                    match r {
                        Ref::Collection(id) => {
                            client
                                .update_collection(id, collection_body(&raw, id, &name, new_parent))
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        Ref::Link(id) => {
                            let u = url.clone().or(item.url.clone()).unwrap_or_default();
                            let owner = remote
                                .owner
                                .get(&new_parent)
                                .or_else(|| remote.owner.get(&remote.root))
                                .copied()
                                .unwrap_or(0);
                            client
                                .update_link(id, link_body(&raw, id, &name, &u, new_parent, owner))
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                    }
                    Ok(None)
                }
                RemoteAction::Delete { .. } => {
                    match *done.refs.get(&node).ok_or("not paired")? {
                        Ref::Collection(id) => client.delete_collection(id).await,
                        Ref::Link(id) => client.delete_link(id).await,
                    }
                    .map_err(|e| e.to_string())?;
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

/// The pairs after a run: what was agreed, except where a change failed or
/// was put off (`keep`: those pairs stay as they were).
fn finalize(
    old: &[Pair],
    plan: &Plan,
    refs: &BTreeMap<String, Ref>,
    keep: &BTreeSet<String>,
) -> Vec<Pair> {
    let old_by: BTreeMap<&str, &Pair> = old.iter().map(|p| (p.node.as_str(), p)).collect();
    let mut out: BTreeMap<String, Pair> = BTreeMap::new();
    for (
        node,
        Desired {
            remote,
            title,
            url,
            parent,
        },
    ) in &plan.desired
    {
        if keep.contains(node) {
            continue;
        }
        let Some(r) = remote.or_else(|| refs.get(node).copied()) else {
            continue;
        };
        out.insert(
            node.clone(),
            Pair {
                node: node.clone(),
                remote: r,
                title: title.clone(),
                url: url.clone(),
                parent: parent.clone(),
            },
        );
    }
    for node in keep {
        if let Some(p) = old_by.get(node.as_str()) {
            out.insert(node.clone(), (*p).clone());
        }
    }
    out.into_values().collect()
}

/// Apply the local half of the plan. Edits made in Bookmoor since the tree
/// was read are not overwritten: if the tree moved on, only additions are
/// made and the rest waits for the next run (`deferred`).
fn apply_local(
    st: &mut AppState,
    profile: &str,
    plan: &Plan,
    seen_rev: u64,
    before: &[bookmoor_core::wire::WireNode],
) -> Done {
    let mut done = Done::default();
    let Some(engine) = st.engines.get(profile) else {
        done.errors.push("the profile is gone".into());
        return done;
    };
    let moved_on = engine.rev() != seen_rev;
    let removed: Vec<&String> = plan
        .local
        .iter()
        .filter_map(|a| match a {
            LocalAction::Remove { node } => Some(node),
            _ => None,
        })
        .collect();
    if !moved_on && (removed.len() > 5 || removed.iter().any(|n| engine.has_children(n.as_str()))) {
        // An undo point, as for a folder removed by hand; not for a single
        // bookmark, which would wipe an older undo point for little.
        st.backups.insert(
            profile.to_string(),
            Backup {
                at: crate::api::utc_now(),
                nodes: before.to_vec(),
            },
        );
    }
    let Some(engine) = st.engines.get_mut(profile) else {
        return done;
    };
    for a in &plan.local {
        let (node, res) = match a {
            LocalAction::Add {
                node,
                parent,
                folder,
                title,
                url,
                ..
            } => (
                node,
                engine.ui_edit(
                    UiEdit::Add {
                        parent: parent.clone(),
                        kind: if *folder {
                            Kind::Folder
                        } else {
                            Kind::Bookmark
                        },
                        title: title.clone(),
                        url: url.clone(),
                        before: None,
                    },
                    node,
                ),
            ),
            LocalAction::Update { node, .. }
            | LocalAction::Move { node, .. }
            | LocalAction::Remove { node }
                if moved_on =>
            {
                done.deferred.insert(node.clone());
                continue;
            }
            LocalAction::Update { node, title, url } => (
                node,
                engine.ui_edit(
                    UiEdit::Update {
                        id: node.clone(),
                        title: title.clone(),
                        url: url.clone(),
                    },
                    "",
                ),
            ),
            LocalAction::Move { node, parent } => (
                node,
                engine.ui_edit(
                    UiEdit::Move {
                        id: node.clone(),
                        parent: parent.clone(),
                        before: None,
                    },
                    "",
                ),
            ),
            LocalAction::Remove { node } => (
                node,
                engine.ui_edit(UiEdit::Remove { id: node.clone() }, ""),
            ),
        };
        if let Err(e) = res {
            done.failed.insert(node.clone());
            if done.errors.len() < 5 {
                done.errors.push(format!("Bookmoor: {e}"));
            }
        }
    }
    done
}

/// What a run did, for the status line.
fn summary(service: Service, plan: &Plan, failed: usize, skipped: usize) -> String {
    let count = |f: fn(&RemoteAction) -> bool| plan.remote.iter().filter(|a| f(a)).count();
    let created = count(|a| matches!(a, RemoteAction::Create { .. }));
    let changed = count(|a| matches!(a, RemoteAction::Update { .. }));
    let removed = count(|a| matches!(a, RemoteAction::Delete { .. }));
    let taken = plan
        .local
        .iter()
        .filter(|a| matches!(a, LocalAction::Add { .. }))
        .count();
    let lchanged = plan
        .local
        .iter()
        .filter(|a| matches!(a, LocalAction::Update { .. } | LocalAction::Move { .. }))
        .count();
    let lremoved = plan.local.len() - taken - lchanged;
    let mut bits = Vec::new();
    if created + changed + removed > 0 {
        bits.push(format!(
            "to {}: {created} new, {changed} changed, {removed} removed",
            service.name()
        ));
    }
    if plan.local.is_empty() {
        // nothing
    } else {
        bits.push(format!(
            "from {}: {taken} new, {lchanged} changed, {lremoved} removed",
            service.name()
        ));
    }
    let mut s = if bits.is_empty() {
        "in sync".to_string()
    } else {
        bits.join("; ")
    };
    if skipped > 0 {
        s.push_str(&format!(
            "; {skipped} bookmark{} left out (address {} cannot hold, or a copy of one already there)",
            if skipped == 1 { "" } else { "s" },
            service.name()
        ));
    }
    if failed > 0 {
        s.push_str(&format!("; {failed} failed"));
    }
    s
}

/// Clears a connection's `running` flag however the run ends, even when the
/// task is cancelled.
struct Running {
    state: Shared,
    id: String,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Ok(mut st) = self.state.lock() {
            if let Some(s) = st.linkwarden_status.get_mut(&self.id) {
                s.running = false;
            }
        }
    }
}

/// Run a connection once. `force` lets a run delete most of what it knows
/// (otherwise it stops: that usually means something is wrong). The run goes
/// on in its own task, so a request that gives up does not cut it short.
pub async fn sync_now(state: &Shared, id: &str, force: bool) -> Result<String, String> {
    {
        let mut st = state.lock().unwrap();
        if !st.linkwarden.iter().any(|l| l.id == id) {
            return Err("no such connection".into());
        }
        let s = st.linkwarden_status.entry(id.to_string()).or_default();
        if s.running {
            return Err("a run is in progress".into());
        }
        s.running = true;
    }
    let (state, id) = (state.clone(), id.to_string());
    tokio::spawn(async move {
        let _running = Running {
            state: state.clone(),
            id: id.clone(),
        };
        let result = run(&state, &id, force).await;
        let t = now();
        let mut st = state.lock().unwrap();
        let interval = st
            .linkwarden
            .iter()
            .find(|l| l.id == id)
            .map_or(DEFAULT_INTERVAL, |l| l.interval);
        let s = st.linkwarden_status.entry(id.clone()).or_default();
        s.last_run = Some(t);
        match &result {
            Ok(r) => {
                s.message = r.message.clone();
                s.failed = r.failed;
                if !r.failed {
                    s.last_ok = Some(t);
                    s.rev = r.rev;
                }
            }
            Err(e) => {
                s.message = e.clone();
                s.failed = true;
            }
        }
        // Soon again after a problem, otherwise after the interval.
        s.next_at = t + if s.failed {
            interval.min(RETRY_SECS)
        } else {
            interval
        };
        result.map(|r| r.message)
    })
    .await
    .unwrap_or_else(|e| Err(format!("the run stopped unexpectedly: {e}")))
}

struct Outcome {
    message: String,
    rev: Option<u64>,
    failed: bool,
}

/// A run that would remove most of what is known, or everything of a
/// non-trivial set, is probably not what was meant.
fn needs_confirmation(plan: &Plan, known: usize, local_empty: bool, remote_empty: bool) -> bool {
    let wiped = (local_empty || remote_empty) && known >= 3 && plan.deletions() > 0;
    wiped || (known >= 10 && plan.deletions() * 2 > known)
}

async fn run(state: &Shared, id: &str, force: bool) -> Result<Outcome, String> {
    let local = {
        let st = state.lock().unwrap();
        read_local(&st, id)?
    };
    let cfg = local.cfg.clone();
    let mut remote = Backend::open(&cfg).await?;
    let mut mint = || random_id("bk_");
    let mut plan = plan(
        &local.root,
        remote.root(),
        &local.nodes,
        remote.items(),
        &cfg.pairs,
        cfg.mode,
        &mut mint,
    );
    // Linkwarden cannot rename, move or delete the browser's own folders. The
    // next run sees the difference and puts them right on its side.
    plan.local.retain(|a| match a {
        LocalAction::Update { node, .. }
        | LocalAction::Move { node, .. }
        | LocalAction::Remove { node } => !local.protected.contains(node),
        LocalAction::Add { .. } => true,
    });
    // Bookmarks whose address one side cannot hold are on neither list, but
    // the other side's copy is not to be deleted or rewritten for that.
    let mut held: BTreeSet<String> = local.unlinkable.clone();
    for p in &cfg.pairs {
        if let Ref::Link(l) = p.remote {
            if remote.unlinkable(l) {
                held.insert(p.node.clone());
            }
        }
    }
    let touches = |node: &String| held.contains(node);
    plan.remote.retain(|a| match a {
        RemoteAction::Update { node, .. } | RemoteAction::Delete { node } => !touches(node),
        RemoteAction::Create { .. } => true,
    });
    plan.local.retain(|a| match a {
        LocalAction::Update { node, .. }
        | LocalAction::Move { node, .. }
        | LocalAction::Remove { node } => !touches(node),
        LocalAction::Add { .. } => true,
    });
    let known = cfg.pairs.len();
    if !force
        && needs_confirmation(
            &plan,
            known,
            local.nodes.is_empty(),
            remote.items().is_empty(),
        )
    {
        return Err(format!(
            "Paused: this run would delete {} of the {} bookmarks and folders it knows (an empty or half-read list on one side looks the same). If that is what you want, use \"Sync anyway\"; otherwise check the collection and the profile",
            plan.deletions(),
            known
        ));
    }
    let mut done = remote.run(&plan, &cfg.pairs, &local.root).await;
    let ldone =
        {
            let mut st = state.lock().unwrap();
            // The connection may have been changed or removed while this ran.
            match st.linkwarden.iter().find(|l| l.id == id) {
                Some(now) if same_place(now, &cfg) => {}
                _ => return Err(
                    "the connection was changed or removed during the run; nothing more was done"
                        .into(),
                ),
            }
            apply_local(&mut st, &cfg.profile_id, &plan, local.rev, &local.before)
        };
    done.failed.extend(ldone.failed);
    done.deferred.extend(ldone.deferred);
    for e in ldone.errors {
        if done.errors.len() < 5 {
            done.errors.push(e);
        }
    }
    let mut keep: BTreeSet<String> = done.failed.union(&done.deferred).cloned().collect();
    keep.extend(held.iter().cloned());
    let pairs = finalize(&cfg.pairs, &plan, &done.refs, &keep);
    {
        let mut st = state.lock().unwrap();
        match st.linkwarden.iter_mut().find(|l| l.id == id) {
            Some(l) if same_place(l, &cfg) => {
                l.ids = remote.ids_to_keep(&pairs);
                l.pairs = pairs;
                l.collection_id = Some(remote.root());
            }
            // Changed or removed while the run was going: the new settings
            // start from scratch.
            _ => return Err("the connection was changed or removed during the run".into()),
        }
        if let Err(e) = st.persist_or_revert() {
            return Err(format!("cannot save the state: {e}"));
        }
        if !plan.local.is_empty() {
            if let Err(e) = st.publish_dynacat(Some(&cfg.profile_id)) {
                eprintln!("warning: cannot write the Dynacat file: {e}");
            }
        }
    }
    let failed = !done.failed.is_empty();
    let mut msg = summary(
        cfg.service,
        &plan,
        done.failed.len(),
        local.skipped + done.left_out,
    );
    if !done.deferred.is_empty() {
        msg.push_str("; Bookmoor changed meanwhile, the rest follows in the next run");
    }
    if !done.errors.is_empty() {
        msg.push_str(". ");
        msg.push_str(&done.errors.join("; "));
    }
    // After changes taken over from Linkwarden, or put off, the tree is not
    // the one that was compared: one more (cheap) run then settles it.
    let rev = (plan.local.is_empty() && done.deferred.is_empty()).then_some(local.rev);
    Ok(Outcome {
        message: msg,
        rev,
        failed,
    })
}

/// Same Linkwarden and same place in the tree (not the pace or direction).
fn same_place(a: &Linkwarden, b: &Linkwarden) -> bool {
    a.service == b.service
        && a.profile_id == b.profile_id
        && a.folder == b.folder
        && a.url == b.url
        && a.collection == b.collection
        && a.token == b.token
}

/// Runs the connections that are due: after changes in Bookmoor (shortly
/// after) and every `interval` seconds for changes made in Linkwarden.
pub fn spawn(state: Shared) {
    let wake = state.lock().unwrap().linkwarden_wake.clone();
    tokio::spawn(async move {
        loop {
            let _ = tokio::time::timeout(Duration::from_secs(15), wake.notified()).await;
            for id in due(&state) {
                let _ = sync_now(&state, &id, false).await;
            }
        }
    });
}

fn due(state: &Shared) -> Vec<String> {
    let st = state.lock().unwrap();
    let t = now();
    st.linkwarden
        .iter()
        .filter(|l| {
            let Some(s) = st.linkwarden_status.get(&l.id) else {
                return true;
            };
            if s.running {
                return false;
            }
            let changed = !s.failed
                && s.rev != st.engines.get(&l.profile_id).map(|e| e.rev())
                && s.last_run.is_none_or(|r| t >= r + 5);
            changed || t >= s.next_at
        })
        .map(|l| l.id.clone())
        .collect()
}
