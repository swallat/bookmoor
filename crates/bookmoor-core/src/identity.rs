/// How an adapter recognises an entry again after the target system (or its
/// edit UI) has rewritten the file. Strategies are tried in the order the
/// user configures; see docs/architecture.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdentityStrategy {
    /// A dedicated data field on the entry, e.g. `id: bk_a1b2`.
    Field,
    /// A trailing same-line comment, e.g. `# id:bk_a1b2`.
    /// Only trailing comments travel with their entry; comments on the
    /// line above are orphaned when an entry is removed.
    Comment,
    /// Derived from group path + title + URL. Always available, but cannot
    /// follow renames or moves.
    Fingerprint,
}

impl IdentityStrategy {
    /// Default preference order: field, then comment, then fingerprint.
    pub const DEFAULT_ORDER: [IdentityStrategy; 3] =
        [IdentityStrategy::Field, IdentityStrategy::Comment, IdentityStrategy::Fingerprint];
}

/// Fingerprint of an entry: normalised group path, title and URL joined by
/// a separator that cannot appear in the parts after normalisation.
pub fn fingerprint(group_path: &[&str], title: &str, url: &str) -> String {
    let norm = |s: &str| s.trim().to_lowercase().replace('\u{1f}', " ");
    let mut parts: Vec<String> = group_path.iter().map(|s| norm(s)).collect();
    parts.push(norm(title));
    parts.push(url.trim().to_string());
    parts.join("\u{1f}")
}
