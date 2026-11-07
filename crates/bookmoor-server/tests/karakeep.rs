//! Karakeep sync against a small fake Karakeep, over real HTTP.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, put};
use axum::{Json, Router};
use bookmoor_core::linkwarden::Mode;
use bookmoor_core::model::Kind;
use bookmoor_server::api::{AppState, Shared};
use bookmoor_server::engine::UiEdit;
use bookmoor_server::linkwarden::{sync_now, Linkwarden, Service};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const KEY: &str = "kk-key";

#[derive(Clone)]
struct List {
    id: String,
    name: String,
    parent: Option<String>,
}

#[derive(Clone)]
struct Mark {
    id: String,
    title: Option<String>,
    crawled: String,
    url: String,
    lists: Vec<String>,
    note: String,
}

#[derive(Default)]
struct Fake {
    lists: Vec<List>,
    marks: Vec<Mark>,
    next: u32,
    log: Vec<String>,
}

type F = Arc<Mutex<Fake>>;

fn authed(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {KEY}"))
}

fn mark_json(m: &Mark) -> Value {
    json!({
        "id": m.id, "title": m.title, "note": m.note, "tags": [],
        "content": {"type": "link", "url": m.url, "title": m.crawled},
    })
}

async fn lists(State(f): State<F>, h: HeaderMap) -> Result<Json<Value>, StatusCode> {
    if !authed(&h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let f = f.lock().unwrap();
    Ok(Json(json!({"lists": f.lists.iter().map(|l| json!({
        "id": l.id, "name": l.name, "parentId": l.parent, "icon": "x",
        "type": "manual", "public": false, "userRole": "owner", "hasCollaborators": false,
    })).collect::<Vec<_>>()})))
}

async fn new_list(State(f): State<F>, Json(b): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut f = f.lock().unwrap();
    assert!(b["icon"].is_string(), "icon missing: {b}");
    f.next += 1;
    let id = format!("l{}", f.next);
    f.log.push("POST /lists".into());
    f.lists.push(List {
        id: id.clone(),
        name: b["name"].as_str().unwrap().into(),
        parent: b["parentId"].as_str().map(String::from),
    });
    (StatusCode::CREATED, Json(json!({"id": id})))
}

async fn change_list(
    State(f): State<F>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> StatusCode {
    let mut f = f.lock().unwrap();
    f.log.push("PATCH /lists".into());
    match f.lists.iter_mut().find(|l| l.id == id) {
        Some(l) => {
            if let Some(n) = b["name"].as_str() {
                l.name = n.into();
            }
            if let Some(p) = b["parentId"].as_str() {
                l.parent = Some(p.into());
            }
            StatusCode::OK
        }
        None => StatusCode::NOT_FOUND,
    }
}

async fn drop_list(State(f): State<F>, Path(id): Path<String>) -> StatusCode {
    let mut f = f.lock().unwrap();
    f.log.push("DELETE /lists".into());
    let mut gone = vec![id];
    let mut i = 0;
    while i < gone.len() {
        let p = gone[i].clone();
        gone.extend(
            f.lists
                .iter()
                .filter(|c| c.parent.as_deref() == Some(p.as_str()))
                .map(|c| c.id.clone()),
        );
        i += 1;
    }
    f.lists.retain(|c| !gone.contains(&c.id));
    for m in &mut f.marks {
        m.lists.retain(|l| !gone.contains(l));
    }
    StatusCode::NO_CONTENT
}

/// Two bookmarks per page, so paging is exercised.
async fn in_list(
    State(f): State<F>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let f = f.lock().unwrap();
    assert_eq!(q.get("limit").map(String::as_str), Some("100"));
    let after = q.get("cursor");
    let mut ms: Vec<&Mark> = f.marks.iter().filter(|m| m.lists.contains(&id)).collect();
    ms.sort_by(|a, b| a.id.cmp(&b.id));
    let page: Vec<&Mark> = ms
        .into_iter()
        .filter(|m| after.is_none_or(|c| m.id.as_str() > c.as_str()))
        .take(2)
        .collect();
    let next = (page.len() == 2).then(|| page[1].id.clone());
    Json(
        json!({"bookmarks": page.iter().map(|m| mark_json(m)).collect::<Vec<_>>(), "nextCursor": next}),
    )
}

async fn new_mark(State(f): State<F>, Json(b): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut f = f.lock().unwrap();
    f.log.push("POST /bookmarks".into());
    let url = b["url"].as_str().unwrap();
    if let Some(m) = f.marks.iter().find(|m| m.url == url) {
        let mut j = mark_json(m);
        j["alreadyExists"] = json!(true);
        return (StatusCode::OK, Json(j));
    }
    f.next += 1;
    let id = format!("b{:03}", f.next);
    let m = Mark {
        id,
        title: b["title"].as_str().map(String::from),
        crawled: String::new(),
        url: url.into(),
        lists: vec![],
        note: String::new(),
    };
    let j = mark_json(&m);
    f.marks.push(m);
    (StatusCode::CREATED, Json(j))
}

async fn change_mark(
    State(f): State<F>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> StatusCode {
    let mut f = f.lock().unwrap();
    f.log.push("PATCH /bookmarks".into());
    match f.marks.iter_mut().find(|m| m.id == id) {
        Some(m) => {
            if let Some(t) = b["title"].as_str() {
                m.title = Some(t.into());
            }
            if let Some(u) = b["url"].as_str() {
                m.url = u.into();
            }
            StatusCode::OK
        }
        None => StatusCode::NOT_FOUND,
    }
}

async fn add_to_list(State(f): State<F>, Path((l, b)): Path<(String, String)>) -> StatusCode {
    let mut f = f.lock().unwrap();
    f.log.push("PUT list".into());
    match f.marks.iter_mut().find(|m| m.id == b) {
        Some(m) if !m.lists.contains(&l) => {
            m.lists.push(l);
            StatusCode::NO_CONTENT
        }
        Some(_) => StatusCode::BAD_REQUEST,
        None => StatusCode::NOT_FOUND,
    }
}

async fn remove_from_list(State(f): State<F>, Path((l, b)): Path<(String, String)>) -> StatusCode {
    let mut f = f.lock().unwrap();
    f.log.push("DELETE list".into());
    if let Some(m) = f.marks.iter_mut().find(|m| m.id == b) {
        m.lists.retain(|x| *x != l);
    }
    StatusCode::NO_CONTENT
}

async fn serve() -> (String, F) {
    let fake: F = Arc::new(Mutex::new(Fake {
        next: 100,
        ..Default::default()
    }));
    let app = Router::new()
        .route("/api/v1/lists", get(lists).post(new_list))
        .route(
            "/api/v1/lists/{id}",
            axum::routing::patch(change_list).delete(drop_list),
        )
        .route("/api/v1/lists/{id}/bookmarks", get(in_list))
        .route(
            "/api/v1/lists/{l}/bookmarks/{b}",
            put(add_to_list).delete(remove_from_list),
        )
        .route("/api/v1/bookmarks", axum::routing::post(new_mark))
        .route("/api/v1/bookmarks/{id}", axum::routing::patch(change_mark))
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
    let id = "kk_test".to_string();
    st.linkwarden.push(Linkwarden {
        id: id.clone(),
        service: Service::Karakeep,
        profile_id: profile.clone(),
        folder: vec![],
        url,
        token: KEY.into(),
        collection: "Bookmoor".into(),
        collection_id: None,
        mode,
        interval: 60,
        pairs: vec![],
        ids: Default::default(),
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

fn list_id(f: &Fake, name: &str) -> String {
    f.lists.iter().find(|l| l.name == name).unwrap().id.clone()
}

#[tokio::test]
async fn first_run_sends_the_tree_and_a_second_run_changes_nothing() {
    let s = setup(Mode::Both).await;
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    assert!(msg.contains("to Karakeep") && msg.contains("new"), "{msg}");
    {
        let f = s.fake.lock().unwrap();
        let names: Vec<&str> = f.lists.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"Bookmoor") && names.contains(&"toolbar") && names.contains(&"News"),
            "{names:?}"
        );
        assert_eq!(f.marks.len(), 4);
        let news = list_id(&f, "News");
        assert_eq!(
            f.marks.iter().filter(|m| m.lists == [news.clone()]).count(),
            3
        );
        let root = list_id(&f, "Bookmoor");
        let toolbar = f.lists.iter().find(|l| l.name == "toolbar").unwrap();
        assert_eq!(toolbar.parent.as_deref(), Some(root.as_str()));
    }
    // The ids were kept, so the next run recognises everything.
    assert!(!s.state.lock().unwrap().linkwarden[0].ids.is_empty());
    let before = s.fake.lock().unwrap().log.len();
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    assert_eq!(msg, "in sync");
    assert_eq!(s.fake.lock().unwrap().log.len(), before, "no writes");
}

#[tokio::test]
async fn changes_in_karakeep_come_back_and_notes_survive_edits() {
    let s = setup(Mode::Both).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    {
        let mut f = s.fake.lock().unwrap();
        let news = list_id(&f, "News");
        // a new bookmark with a crawled title only, and a new sub-list
        f.marks.push(Mark {
            id: "b900".into(),
            title: None,
            crawled: "Crawled title".into(),
            url: "https://added.example".into(),
            lists: vec![news.clone()],
            note: "my note".into(),
        });
        f.lists.push(List {
            id: "l900".into(),
            name: "Deep".into(),
            parent: Some(news),
        });
        f.marks.push(Mark {
            id: "b901".into(),
            title: Some("Nested".into()),
            crawled: String::new(),
            url: "https://nested.example".into(),
            lists: vec!["l900".into()],
            note: String::new(),
        });
        // a note and a bookmark in no list are not touched or taken
        f.marks.push(Mark {
            id: "b902".into(),
            title: Some("Unlisted".into()),
            crawled: String::new(),
            url: "https://unlisted.example".into(),
            lists: vec![],
            note: String::new(),
        });
        let rust = f
            .marks
            .iter_mut()
            .find(|m| m.url == "https://rust-lang.org")
            .unwrap();
        rust.title = Some("Rust (renamed there)".into());
    }
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    assert!(msg.contains("from Karakeep: 3 new, 1 changed"), "{msg}");
    let t = titles(&s);
    for want in ["Crawled title", "Deep", "Nested", "Rust (renamed there)"] {
        assert!(t.contains(&want.to_string()), "{want} in {t:?}");
    }
    assert!(!t.contains(&"Unlisted".to_string()));
    // Rename the note-carrying bookmark here: the note stays.
    {
        let mut st = s.state.lock().unwrap();
        let id = st.engines[&s.profile]
            .tree()
            .nodes
            .iter()
            .find(|n| n.title == "Crawled title")
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
    sync_now(&s.state, &s.id, false).await.unwrap();
    let f = s.fake.lock().unwrap();
    let m = f.marks.iter().find(|m| m.id == "b900").unwrap();
    assert_eq!(m.title.as_deref(), Some("Renamed here"));
    assert_eq!(m.note, "my note");
}

#[tokio::test]
async fn moves_and_deletes_travel_and_deleting_keeps_the_bookmark() {
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
    // LWN was also put in a second synced list over there: the move takes it out.
    {
        let mut f = s.fake.lock().unwrap();
        let toolbar = list_id(&f, "toolbar");
        f.marks
            .iter_mut()
            .find(|m| m.url == "https://lwn.net")
            .unwrap()
            .lists
            .push(toolbar);
    }
    sync_now(&s.state, &s.id, false).await.unwrap();
    let f = s.fake.lock().unwrap();
    let hn = f
        .marks
        .iter()
        .find(|m| m.url == "https://news.ycombinator.com")
        .unwrap();
    assert!(hn.lists.is_empty(), "left the lists, not deleted");
    let other = list_id(&f, "other");
    let lwn = f.marks.iter().find(|m| m.url == "https://lwn.net").unwrap();
    assert_eq!(lwn.lists, vec![other]);
}

#[tokio::test]
async fn a_bookmark_in_two_lists_is_not_doubled_and_stays_put() {
    let s = setup(Mode::Both).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    {
        let mut f = s.fake.lock().unwrap();
        let toolbar = list_id(&f, "toolbar");
        f.marks
            .iter_mut()
            .find(|m| m.url == "https://lwn.net")
            .unwrap()
            .lists
            .push(toolbar);
    }
    let n = titles(&s).len();
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    assert_eq!(msg, "in sync");
    assert_eq!(titles(&s).len(), n);
}

#[tokio::test]
async fn a_wrong_key_or_a_deleted_list_changes_nothing() {
    let s = setup(Mode::Both).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    let n = titles(&s).len();
    {
        let mut f = s.fake.lock().unwrap();
        f.lists.clear();
        f.marks.clear();
    }
    let err = sync_now(&s.state, &s.id, false).await.unwrap_err();
    assert!(err.contains("is gone"), "{err}");
    assert_eq!(titles(&s).len(), n);
    s.state.lock().unwrap().linkwarden[0].token = "nope".into();
    let err = sync_now(&s.state, &s.id, false).await.unwrap_err();
    assert!(err.contains("401"), "{err}");
    assert_eq!(titles(&s).len(), n);
}

#[tokio::test]
async fn an_emptied_list_pauses_the_run_until_forced() {
    let s = setup(Mode::Both).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    let n = titles(&s).len();
    {
        // Everything below the bound list is gone (or was not read).
        let mut f = s.fake.lock().unwrap();
        let root = list_id(&f, "Bookmoor");
        f.lists.retain(|l| l.id == root);
        f.marks.clear();
    }
    let err = sync_now(&s.state, &s.id, false).await.unwrap_err();
    assert!(err.starts_with("Paused"), "{err}");
    assert_eq!(titles(&s).len(), n);
}

#[tokio::test]
async fn push_leaves_karakeep_only_items_alone() {
    let s = setup(Mode::Push).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    {
        let mut f = s.fake.lock().unwrap();
        let news = list_id(&f, "News");
        f.marks.push(Mark {
            id: "b950".into(),
            title: Some("Only there".into()),
            crawled: String::new(),
            url: "https://only.example".into(),
            lists: vec![news],
            note: String::new(),
        });
    }
    let n = titles(&s).len();
    sync_now(&s.state, &s.id, false).await.unwrap();
    assert_eq!(titles(&s).len(), n);
    assert_eq!(s.fake.lock().unwrap().marks.len(), 5);
}

#[tokio::test]
async fn odd_and_repeated_addresses_are_left_out() {
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
        add_mark(
            &mut st,
            &s.profile,
            &toolbar,
            "Rust again",
            "https://rust-lang.org",
        );
    }
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    assert!(msg.contains("2 bookmarks left out"), "{msg}");
    assert_eq!(s.fake.lock().unwrap().marks.len(), 4);
    // The next run still has nothing to do (and still counts them).
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    assert!(
        msg.starts_with("2 bookmarks left out") || msg.contains("in sync"),
        "{msg}"
    );
}

#[tokio::test]
async fn an_address_karakeep_has_elsewhere_is_left_out_without_failing() {
    let s = setup(Mode::Both).await;
    sync_now(&s.state, &s.id, false).await.unwrap();
    // A copy of a synced address in another folder, added here.
    {
        let mut st = s.state.lock().unwrap();
        let other = folder(&st, &s.profile, "other");
        add_mark(&mut st, &s.profile, &other, "AAA copy", "https://lwn.net");
    }
    let msg = sync_now(&s.state, &s.id, false).await.unwrap();
    assert!(msg.contains("1 bookmark left out"), "{msg}");
    assert!(!msg.contains("failed"), "{msg}");
    let st = s.state.lock().unwrap();
    assert!(!st.linkwarden_status[&s.id].failed);
    // The paired copy still is the one in Karakeep.
    let f = s.fake.lock().unwrap();
    assert_eq!(
        f.marks
            .iter()
            .filter(|m| m.url == "https://lwn.net")
            .count(),
        1
    );
}
