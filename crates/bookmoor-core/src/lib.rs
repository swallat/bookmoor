//! Core types for bookmoor: the canonical bookmark tree, the adapter interface,
//! identity strategies and the diff used by the three-way merge.

pub mod adapter;
pub mod diff;
pub mod identity;
pub mod model;

pub use adapter::{Adapter, Capabilities, Direction, Error, Result};
pub use diff::{diff, Change};
pub use identity::{fingerprint, IdentityStrategy};
pub use model::{Bookmark, Folder, Kind, Node, NodeId, Tree};
