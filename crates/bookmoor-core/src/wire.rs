//! Wire types of the sync protocol (`docs/sync-architecture.md`, section 3.2).
//!
//! These types are the single source of truth: the JSON Schema in
//! `schema/wire.schema.json` is generated from them (see the
//! `schema_is_up_to_date` test) and the extension's TypeScript types are
//! generated from that schema.

use crate::model::Kind;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};

/// A node of the canonical tree as sent over the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WireNode {
    pub id: String,
    /// `null` only for the tree root.
    pub parent: Option<String>,
    pub position: String,
    pub kind: Kind,
    pub title: String,
    /// Required for bookmarks, absent for folders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// One client operation. `op_id` makes retried requests idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Op {
    pub op_id: String,
    #[serde(flatten)]
    pub body: OpBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OpBody {
    Add {
        id: String,
        parent: String,
        position: String,
        kind: Kind,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// Partial update: only the fields that are present change.
    Update {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    Move {
        id: String,
        parent: String,
        position: String,
    },
    Remove {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncRequest {
    pub endpoint_id: String,
    /// Last server revision this client has applied.
    pub since_rev: u64,
    pub ops: Vec<Op>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    ConflictLost,
    Invalid,
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RejectedOp {
    pub op_id: String,
    pub reason: RejectReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncResponse {
    /// New cursor.
    pub rev: u64,
    /// Canonical changes in `(since_rev, rev]`, excluding the client's own.
    pub ops: Vec<Op>,
    pub rejected: Vec<RejectedOp>,
    /// `true`: discard the local base and call `GET /v1/tree`.
    pub resync: bool,
}

/// `GET /v1/tree`: the full canonical tree, flattened, parent-first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TreeResponse {
    pub rev: u64,
    pub nodes: Vec<WireNode>,
}

/// All wire types in one schema document (`$defs`), so one generation step
/// covers the whole protocol.
#[derive(JsonSchema)]
#[allow(dead_code)]
struct Protocol {
    sync_request: SyncRequest,
    sync_response: SyncResponse,
    tree_response: TreeResponse,
}

/// The JSON Schema of the protocol, pretty-printed with a trailing newline.
pub fn json_schema() -> String {
    let mut s = serde_json::to_string_pretty(&schema_for!(Protocol)).expect("serialisable");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../schema/wire.schema.json");

    #[test]
    fn schema_is_up_to_date() {
        let fresh = json_schema();
        if std::env::var_os("BOOKMOOR_UPDATE_SCHEMA").is_some() {
            std::fs::create_dir_all(std::path::Path::new(SCHEMA_PATH).parent().unwrap()).unwrap();
            std::fs::write(SCHEMA_PATH, &fresh).unwrap();
        }
        // Git on Windows may check the file out with CRLF line endings.
        let on_disk = std::fs::read_to_string(SCHEMA_PATH)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert_eq!(
            on_disk, fresh,
            "schema/wire.schema.json is stale; run: BOOKMOOR_UPDATE_SCHEMA=1 cargo test -p bookmoor-core schema_is_up_to_date"
        );
    }

    #[test]
    fn request_matches_the_documented_shape() {
        let json = r#"{
          "endpoint_id": "ep_1",
          "since_rev": 4211,
          "ops": [
            { "op_id": "o1", "type": "add", "id": "bk_new", "parent": "bk_f1",
              "position": "V", "kind": "bookmark", "title": "HN",
              "url": "https://news.ycombinator.com" },
            { "op_id": "o2", "type": "update", "id": "bk_b1", "title": "GH" },
            { "op_id": "o3", "type": "move", "id": "bk_b2", "parent": "bk_f2", "position": "W" },
            { "op_id": "o4", "type": "remove", "id": "bk_b3" }
          ]
        }"#;
        let req: SyncRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.ops.len(), 4);
        assert!(matches!(req.ops[1].body, OpBody::Update { url: None, .. }));
        // Round trip keeps the exact structure.
        let again: SyncRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(req, again);
    }

    #[test]
    fn unknown_op_type_is_rejected() {
        let json = r#"{"op_id":"o","type":"explode","id":"x"}"#;
        assert!(serde_json::from_str::<Op>(json).is_err());
    }

    #[test]
    fn response_serialises_reasons_in_snake_case() {
        let r = SyncResponse {
            rev: 5,
            ops: vec![],
            rejected: vec![RejectedOp {
                op_id: "o".into(),
                reason: RejectReason::ConflictLost,
            }],
            resync: false,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["rejected"][0]["reason"], "conflict_lost");
    }
}
