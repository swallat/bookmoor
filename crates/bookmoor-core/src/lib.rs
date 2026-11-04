//! Core types for bookmoor: the canonical bookmark tree, the adapter interface,
//! identity strategies and the diff used by the three-way merge.

pub mod adapter;
pub mod diff;
pub mod dynacat;
pub mod identity;
pub mod merge;
pub mod model;
pub mod position;
pub mod wire;

pub use adapter::{Adapter, Capabilities, Direction, Error, Result};
pub use diff::{diff, Change};
pub use identity::{fingerprint, IdentityStrategy};
pub use merge::{merge, Merged, Reason, Rejected, Side};
pub use model::{Bookmark, Folder, Kind, Node, NodeId, Tree};
