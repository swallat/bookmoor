//! Linkwarden sync against a small fake Linkwarden, over real HTTP.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use bookmoor_core::linkwarden::Mode;
use bookmoor_core::model::Kind;
use bookmoor_server::api::{AppState, Shared};
use bookmoor_server::engine::UiEdit;
use bookmoor_server::linkwarden::{sync_now, Linkwarden};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "lw-token";

#[derive(Clone)]
struct Coll {
    id: i64,
    name: String,
    parent: Option<i64>,
}

#[derive(Clone)]
struct Link {
    id: i64,
    name: String,
    url: String,
    collection: i64,
    tags: Vec<String>,
}

#[derive(Default)]
struct Fake {
    colls: Vec<Coll>,
    links: Vec<Link>,
    next: i64,
    /// Requests seen, as "METHOD path".
    log: Vec<String>,
}

type F = Arc<Mutex<Fake>>;

fn authed(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"))
}

fn link_json(l: &Link) -> Value {
    json!({
        "id": l.id, "name": l.name, "url": l.url, "collectionId": l.collection,
        "description": "kept",
        "tags": l.tags.iter().enumerate().map(|(i, t)| json!({"id": i + 1, "name": t})).collect::<Vec<_>>(),
        "collection": {"id": l.collection, "ownerId": 7},
    })
}

async fn collections(State(f): State<F>, h: HeaderMap) -> Result<Json<Value>, StatusCode> {
    if !authed(&h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let f = f.lock().unwrap();
    Ok(Json(json!({"response": f.colls.iter().map(|c| json!({
        "id": c.id, "name": c.name, "parentId": c.parent, "ownerId": 7, "isPublic": false,
        "members": [], "description": "",
    })).collect::<Vec<_>>()})))
}

async fn new_collection(
    State(f): State<F>,
    h: HeaderMap,
    Json(b): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if !authed(&h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut f = f.lock().unwrap();
    f.next += 1;
    let id = f.next;
    f.log.push("POST /collections".into());
    f.colls.push(Coll {
        id,
        name: b["name"].as_str().unwrap().into(),
        parent: b["parentId"].as_i64(),
    });
    Ok(Json(json!({"response": {"id": id}})))
}

async fn change_collection(
    State(f): State<F>,
    Path(id): Path<i64>,
    Json(b): Json<Value>,
) -> StatusCode {
    let mut f = f.lock().unwrap();
    f.log.push("PUT /collections".into());
    // The real API insists on these.
    assert!(b["members"].is_array(), "members missing: {b}");
    match f.colls.iter_mut().find(|c| c.id == id) {
        Some(c) => {
            c.name = b["name"].as_str().unwrap().into();
            c.parent = b["parentId"].as_i64();
            StatusCode::OK
        }
        None => StatusCode::NOT_FOUND,
    }
}

async fn drop_collection(State(f): State<F>, Path(id): Path<i64>) -> StatusCode {
    let mut f = f.lock().unwrap();
    f.log.push("DELETE /collections".into());
    let mut gone = vec![id];
    let mut i = 0;
    while i < gone.len() {
        let p = gone[i];
        gone.extend(f.colls.iter().filter(|c| c.parent == Some(p)).map(|c| c.id));
        i += 1;
    }
    f.colls.retain(|c| !gone.contains(&c.id));
    f.links.retain(|l| !gone.contains(&l.collection));
    StatusCode::OK
}

/// Two links per page, so paging is exercised.
async fn search(State(f): State<F>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let f = f.lock().unwrap();
    let coll: i64 = q["collectionId"].parse().unwrap();
    let cursor: Option<i64> = q.get("cursor").map(|c| c.parse().unwrap());
    let mut ls: Vec<&Link> = f
        .links
        .iter()
        .filter(|l| l.collection == coll && cursor.is_none_or(|c| l.id > c))
        .collect();
    ls.sort_by_key(|l| l.id);
    let page: Vec<&Link> = ls.into_iter().take(2).collect();
    let next = (page.len() == 2).then(|| page[1].id);
    Json(
        json!({"data": {"links": page.iter().map(|l| link_json(l)).collect::<Vec<_>>(), "nextCursor": next}}),
    )
}

async fn new_link(State(f): State<F>, Json(b): Json<Value>) -> Json<Value> {
    let mut f = f.lock().unwrap();
    f.next += 1;
    let id = f.next;
    f.log.push("POST /links".into());
    f.links.push(Link {
        id,
        name: b["name"].as_str().unwrap_or("").into(),
        url: b["url"].as_str().unwrap().into(),
        collection: b["collection"]["id"].as_i64().unwrap(),
        tags: vec![],
    });
    Json(json!({"response": {"id": id}}))
}

async fn change_link(State(f): State<F>, Path(id): Path<i64>, Json(b): Json<Value>) -> StatusCode {
    let mut f = f.lock().unwrap();
    f.log.push("PUT /links".into());
    assert!(b["collection"]["ownerId"].is_i64(), "ownerId missing: {b}");
    assert!(b["tags"].is_array(), "tags missing: {b}");
    match f.links.iter_mut().find(|l| l.id == id) {
        Some(l) => {
            l.name = b["name"].as_str().unwrap().into();
            l.url = b["url"].as_str().unwrap().into();
            l.collection = b["collection"]["id"].as_i64().unwrap();
            StatusCode::OK
        }
        None => StatusCode::NOT_FOUND,
    }
}

async fn drop_link(State(f): State<F>, Path(id): Path<i64>) -> StatusCode {
    let mut f = f.lock().unwrap();
    f.log.push("DELETE /links".into());
    f.links.retain(|l| l.id != id);
    StatusCode::OK
}

async fn serve() -> (String, F) {
    let fake: F = Arc::new(Mutex::new(Fake {
        next: 100,
        ..Default::default()
    }));
    let app = Router::new()
        .route("/api/v1/collections", get(collections).post(new_collection))
        .route(
            "/api/v1/collections/{id}",
            put(change_collection).delete(drop_collection),
        )
        .route("/api/v1/search", get(search))
        .route("/api/v1/links", post(new_link))
        .route("/api/v1/links/{id}", put(change_link))
        .route("/api/v1/links/{id}", delete(drop_link))
        .with_state(fake.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), fake)
}

struct Setup {
    state: Shared,
    profile: String,
    id: String,
    fake: F,
}

fn folder(st: &AppState, profile: &str, title: &str) -> String {
    st.engines[profile]
        .tree()
        .nodes
        .iter()
        .find(|n| n.kind == Kind::Folder && n.title == title)
        .unwrap()
        .id
        .clone()
}

fn add_mark(st: &mut AppState, profile: &str, parent: &str, title: &str, url: &str) {
    st.edit_profile(
        profile,
        UiEdit::Add {
            parent: parent.into(),
            kind: Kind::Bookmark,
            title: title.into(),
            url: Some(url.into()),
            before: None,
        },
    )
    .unwrap();
}

async fn setup(mode: Mode) -> Setup {
    let (url, fake) = serve().await;
    let mut st = AppState::default();
    let uid = st.add_user("anna", false, None).unwrap().id;
    let profile = bookmoor_server::profiles::default_id_for(&uid);
    let toolbar = folder(&st, &profile, "toolbar");
    add_mark(&mut st, &profile, &toolbar, "Rust", "https://rust-lang.org");
    st.edit_profile(
        &profile,
        UiEdit::Add {
            parent: toolbar.clone(),
            kind: Kind::Folder,
            title: "News".into(),
            url: None,
            before: None,
        },
    )
    .unwrap();
    let news = folder(&st, &profile, "News");
    add_mark(
        &mut st,
        &profile,
        &news,
        "HN",
        "https://news.ycombinator.com",
    );
    add_mark(&mut st, &profile, &news, "LWN", "https://lwn.net");
    add_mark(&mut st, &profile, &news, "Heise", "https://heise.de");
    let id = "lw_test".to_string();
    st.linkwarden.push(Linkwarden {
        id: id.clone(),
        profile_id: profile.clone(),
        folder: vec![],
        url,
        token: TOKEN.into(),
        collection: "Bookmoor".into(),
        collection_id: None,
        mode,
        interval: 60,
        pairs: vec![],
    });
    Setup {
        state: Arc::new(Mutex::new(st)),
        profile,
        id,
        fake,
    }
}

fn titles(s: &Setup) -> Vec<String> {
    let st = s.state.lock().unwrap();
    let mut v: Vec<String> = st.engines[&s.profile]
        .tree()
        .nodes
        .iter()
        .map(|n| n.title.clone())
        .collect();
    v.sort();
    v
}

fn count(s: &Setup, what: &str) -> usize {
    s.fake
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|l| *l == what)
        .count()
}

#[tokio::test]
async fn first_run_sends_the_tree_and_a_second_run_changes_nothing() {
    let s = setup(Mode::Both).await;
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    assert!(msg.contains("new"), "{msg}");
    {
        let f = s.fake.lock().unwrap();
        // Bookmoor, toolbar, other, mobile, News
        let names: Vec<&str> = f.colls.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"Bookmoor") && names.contains(&"toolbar") && names.contains(&"News"),
            "{names:?}"
        );
        assert_eq!(f.links.len(), 4);
        // links are in the collection of their folder
        let news = f.colls.iter().find(|c| c.name == "News").unwrap().id;
        assert_eq!(f.links.iter().filter(|l| l.collection == news).count(), 3);
    }
    let before = s.fake.lock().unwrap().log.len();
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    assert_eq!(msg, "in sync");
    assert_eq!(s.fake.lock().unwrap().log.len(), before, "no writes");
}

#[tokio::test]
async fn changes_in_linkwarden_come_back_and_tags_survive_edits() {
    let s = setup(Mode::Both).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    {
        let mut f = s.fake.lock().unwrap();
        let news = f.colls.iter().find(|c| c.name == "News").unwrap().id;
        f.next += 1;
        let id = f.next;
        // a new link and a new sub-collection with a link
        f.links.push(Link {
            id,
            name: "Added in Linkwarden".into(),
            url: "https://added.example".into(),
            collection: news,
            tags: vec!["read-later".into()],
        });
        f.next += 1;
        let cid = f.next;
        f.colls.push(Coll {
            id: cid,
            name: "Deep".into(),
            parent: Some(news),
        });
        f.next += 1;
        let lid = f.next;
        f.links.push(Link {
            id: lid,
            name: "Nested".into(),
            url: "https://nested.example".into(),
            collection: cid,
            tags: vec![],
        });
        // a rename of an existing link
        let rust = f
            .links
            .iter_mut()
            .find(|l| l.url == "https://rust-lang.org")
            .unwrap();
        rust.name = "Rust (renamed there)".into();
    }
    // and an edit made in Bookmoor to a link that carries tags
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    assert!(msg.contains("from Linkwarden: 3 new, 1 changed"), "{msg}");
    let t = titles(&s);
    for want in [
        "Added in Linkwarden",
        "Deep",
        "Nested",
        "Rust (renamed there)",
    ] {
        assert!(t.contains(&want.to_string()), "{want} in {t:?}");
    }
    // Rename the tagged link here: the tags and description stay.
    {
        let mut st = s.state.lock().unwrap();
        let id = st.engines[&s.profile]
            .tree()
            .nodes
            .iter()
            .find(|n| n.title == "Added in Linkwarden")
            .unwrap()
            .id
            .clone();
        st.edit_profile(
            &s.profile,
            UiEdit::Update {
                id,
                title: Some("Renamed here".into()),
                url: None,
            },
        )
        .unwrap();
    }
    // (the run that took the changes over asks for one more)
    sync_now(&s.state, &s.id, false).await.unwrap();
    let f = s.fake.lock().unwrap();
    let l = f
        .links
        .iter()
        .find(|l| l.url == "https://added.example")
        .unwrap();
    assert_eq!(l.name, "Renamed here");
    assert_eq!(l.tags, vec!["read-later".to_string()]);
}

#[tokio::test]
async fn deletes_and_moves_travel_to_linkwarden() {
    let s = setup(Mode::Both).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    {
        let mut st = s.state.lock().unwrap();
        let tree = st.engines[&s.profile].tree();
        let find = |t: &str| tree.nodes.iter().find(|n| n.title == t).unwrap().id.clone();
        st.edit_profile(&s.profile, UiEdit::Remove { id: find("HN") })
            .unwrap();
        let other = find("other");
        st.edit_profile(
            &s.profile,
            UiEdit::Move {
                id: find("LWN"),
                parent: other,
                before: None,
            },
        )
        .unwrap();
    }
    sync_now(&s.state, &s.id, false).await.unwrap();
    let f = s.fake.lock().unwrap();
    assert!(!f
        .links
        .iter()
        .any(|l| l.url == "https://news.ycombinator.com"));
    let other = f.colls.iter().find(|c| c.name == "other").unwrap().id;
    let lwn = f.links.iter().find(|l| l.url == "https://lwn.net").unwrap();
    assert_eq!(lwn.collection, other);
}

#[tokio::test]
async fn a_wrong_token_or_a_deleted_collection_changes_nothing() {
    let s = setup(Mode::Both).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    let n = titles(&s).len();
    // collection deleted in Linkwarden: stop, keep the bookmarks
    s.fake.lock().unwrap().colls.clear();
    s.fake.lock().unwrap().links.clear();
    let err = sync_now(&s.state, &s.id, false).await.unwrap_err();
    assert!(err.contains("is gone"), "{err}");
    assert_eq!(titles(&s).len(), n);
    // wrong token
    s.state.lock().unwrap().linkwarden[0].token = "nope".into();
    let err = sync_now(&s.state, &s.id, false).await.unwrap_err();
    assert!(err.contains("401"), "{err}");
    assert_eq!(titles(&s).len(), n);
    let st = s.state.lock().unwrap();
    assert!(st.linkwarden_status[&s.id].failed);
}

#[tokio::test]
async fn most_of_it_vanishing_pauses_the_run_until_forced() {
    let s = setup(Mode::Both).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    {
        let mut st = s.state.lock().unwrap();
        let other = folder(&st, &s.profile, "other");
        for i in 0..6 {
            add_mark(
                &mut st,
                &s.profile,
                &other,
                &format!("More {i}"),
                &format!("https://more{i}.example"),
            );
        }
    }
    sync_now(&s.state, &s.id, false).await.unwrap();
    // everything known on each side is wiped in the bound collection (which
    // stays itself)
    {
        let mut f = s.fake.lock().unwrap();
        let root = f.colls.iter().find(|c| c.name == "Bookmoor").unwrap().id;
        f.colls.retain(|c| c.id == root);
        f.links.clear();
    }
    // ... and delete nothing locally: remote delete + local removal planned
    let err = sync_now(&s.state, &s.id, false).await.unwrap_err();
    assert!(err.starts_with("Paused"), "{err}");
    assert_eq!(count(&s, "DELETE /links"), 0);
    sync_now(&s.state, &s.id, true).await.unwrap();
    // the tree was emptied on purpose and can be restored; the browser's own
    // folders stay
    let st = s.state.lock().unwrap();
    assert!(st.backups.contains_key(&s.profile));
    drop(st);
    let t = titles(&s);
    assert!(
        t.contains(&"toolbar".to_string()) && !t.contains(&"More 1".to_string()),
        "{t:?}"
    );
}

#[tokio::test]
async fn push_leaves_linkwarden_only_items_alone() {
    let s = setup(Mode::Push).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    {
        let mut f = s.fake.lock().unwrap();
        let root = f.colls.iter().find(|c| c.name == "Bookmoor").unwrap().id;
        f.next += 1;
        let id = f.next;
        f.links.push(Link {
            id,
            name: "Only there".into(),
            url: "https://only.example".into(),
            collection: root,
            tags: vec![],
        });
    }
    let before = titles(&s);
    sync_now(&s.state, &s.id, false).await.unwrap();
    assert_eq!(titles(&s), before);
    assert!(s
        .fake
        .lock()
        .unwrap()
        .links
        .iter()
        .any(|l| l.url == "https://only.example"));
}

#[tokio::test]
async fn bookmarks_with_odd_addresses_are_left_out() {
    let s = setup(Mode::Both).await;
    {
        let mut st = s.state.lock().unwrap();
        let toolbar = folder(&st, &s.profile, "toolbar");
        add_mark(
            &mut st,
            &s.profile,
            &toolbar,
            "Settings",
            "chrome://settings",
        );
        // an address with a space cannot go to Linkwarden
        add_mark(
            &mut st,
            &s.profile,
            &toolbar,
            "Odd",
            "javascript:alert(1) x",
        );
    }
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    // chrome:// is a valid URL for Linkwarden's zod check: it goes; the odd one is left out
    assert!(msg.contains("1 bookmark left out"), "{msg}");
}
