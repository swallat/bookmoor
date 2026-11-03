use crate::identity::IdentityStrategy;
use crate::model::Tree;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The adapter does not implement this operation (see `Capabilities`).
    Unsupported(&'static str),
    /// The target could not be read or written.
    Io(String),
    /// The target content could not be parsed or does not match the expected shape.
    Format(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::Io(m) => write!(f, "io error: {m}"),
            Error::Format(m) => write!(f, "format error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Sync direction configured per endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// canonical tree -> endpoint only
    Push,
    /// endpoint -> canonical tree only
    Pull,
    Both,
}

/// What an adapter can do and which parts of the model it can represent.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub read: bool,
    pub write: bool,
    pub watch: bool,
    /// Maximum folder nesting the target can represent (`None` = unlimited).
    pub max_depth: Option<usize>,
    /// Identity strategies the target survives, determined by the adapter's
    /// requirements analysis (docs/adapters/TEMPLATE.md).
    pub identity: Vec<IdentityStrategy>,
}

/// A bidirectional endpoint (a dashboard, a file, another browser's bridge).
///
/// Adapters must never destroy data they cannot represent: the engine only
/// passes them diffs against the last synced state.
pub trait Adapter {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn read(&mut self) -> Result<Tree>;
    fn write(&mut self, tree: &Tree) -> Result<()>;
}
