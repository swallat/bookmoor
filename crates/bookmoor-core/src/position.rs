//! Fractional indexes for sibling order.
//!
//! A position is a non-empty string over the base-62 alphabet
//! `0-9A-Za-z` (ASCII order equals digit order). Positions compare
//! lexicographically, and a valid position never ends in `0`, so there is
//! always room between any two of them. Inserting between two siblings
//! creates a single new key and never rewrites the neighbours.
//!
//! The same algorithm is implemented in the extension; both run the shared
//! vectors in `testvectors/position.json`.

use std::cmp::Ordering;
use std::fmt;

const DIGITS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE: usize = DIGITS.len();

/// Why a position could not be generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PositionError {
    /// Empty, trailing `0`, or a character outside the alphabet.
    Invalid(String),
    /// `before` is not strictly smaller than `after`.
    NotOrdered,
}

impl fmt::Display for PositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PositionError::Invalid(p) => write!(f, "invalid position {p:?}"),
            PositionError::NotOrdered => write!(f, "lower bound is not smaller than upper bound"),
        }
    }
}

impl std::error::Error for PositionError {}

/// Whether `p` is a well-formed position.
pub fn is_valid(p: &str) -> bool {
    !p.is_empty() && !p.ends_with('0') && p.bytes().all(|c| digit(c).is_some())
}

fn digit(c: u8) -> Option<usize> {
    DIGITS.iter().position(|d| *d == c)
}

/// A position strictly between `before` and `after`.
///
/// `None` means "no neighbour on this side": `(None, None)` yields the first
/// key for an empty folder, `(Some(a), None)` appends, `(None, Some(b))`
/// prepends.
pub fn between(before: Option<&str>, after: Option<&str>) -> Result<String, PositionError> {
    for p in [before, after].into_iter().flatten() {
        if !is_valid(p) {
            return Err(PositionError::Invalid(p.to_string()));
        }
    }
    if let (Some(a), Some(b)) = (before, after) {
        if a >= b {
            return Err(PositionError::NotOrdered);
        }
    }
    Ok(midpoint(
        before.unwrap_or("").as_bytes(),
        after.map(str::as_bytes),
    ))
}

/// `a` may be empty (meaning "0"), `b` of `None` means infinity. Requires
/// `a < b` and neither ending in `0`.
fn midpoint(a: &[u8], b: Option<&[u8]>) -> String {
    if let Some(b) = b {
        // Skip the common prefix, treating `a` as zero-padded.
        let mut n = 0;
        while n < b.len() && a.get(n).copied().unwrap_or(DIGITS[0]) == b[n] {
            n += 1;
        }
        if n > 0 {
            let rest_a = if n < a.len() { &a[n..] } else { &[][..] };
            let mut out = String::from_utf8(b[..n].to_vec()).expect("ascii");
            out.push_str(&midpoint(rest_a, Some(&b[n..])));
            return out;
        }
    }
    let da = a.first().map_or(0, |c| digit(*c).expect("validated"));
    let db = b.map_or(BASE, |b| digit(b[0]).expect("validated"));
    if db - da > 1 {
        let mid = (da + db).div_ceil(2);
        return (DIGITS[mid] as char).to_string();
    }
    // Adjacent digits: either keep b's first digit (if b is longer) or
    // extend a's first digit.
    if let Some(b) = b {
        if b.len() > 1 {
            return (b[0] as char).to_string();
        }
    }
    let rest = if a.len() > 1 { &a[1..] } else { &[][..] };
    let mut out = (DIGITS[da] as char).to_string();
    out.push_str(&midpoint(rest, None));
    out
}

/// Total order for siblings: by position, ties broken by id.
pub fn cmp_siblings(a: (&str, &str), b: (&str, &str)) -> Ordering {
    a.0.cmp(b.0).then_with(|| a.1.cmp(b.1))
}

/// `n` evenly usable, strictly increasing positions for bootstrapping a
/// folder (e.g. when importing an existing bookmark list).
pub fn sequence(n: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(n);
    for _ in 0..n {
        let next = between(out.last().map(String::as_str), None).expect("valid");
        out.push(next);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Vectors {
        between: Vec<Case>,
        invalid: Vec<Invalid>,
    }
    #[derive(Deserialize)]
    struct Case {
        before: Option<String>,
        after: Option<String>,
        expected: String,
    }
    #[derive(Deserialize)]
    struct Invalid {
        before: Option<String>,
        after: Option<String>,
    }

    fn vectors() -> Vectors {
        serde_json::from_str(include_str!("../../../testvectors/position.json")).unwrap()
    }

    #[test]
    fn shared_vectors() {
        for c in vectors().between {
            let got = between(c.before.as_deref(), c.after.as_deref()).unwrap();
            assert_eq!(got, c.expected, "between({:?}, {:?})", c.before, c.after);
        }
    }

    #[test]
    fn shared_invalid_vectors() {
        for c in vectors().invalid {
            assert!(
                between(c.before.as_deref(), c.after.as_deref()).is_err(),
                "between({:?}, {:?}) should fail",
                c.before,
                c.after
            );
        }
    }

    #[test]
    fn repeated_append_and_prepend_stay_ordered() {
        let mut keys = vec![between(None, None).unwrap()];
        for _ in 0..200 {
            let k = between(keys.last().map(String::as_str), None).unwrap();
            keys.push(k);
        }
        for _ in 0..200 {
            let k = between(None, keys.first().map(String::as_str)).unwrap();
            keys.insert(0, k);
        }
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        assert!(keys.iter().all(|k| is_valid(k)));
    }

    #[test]
    fn repeated_insertion_at_same_gap_stays_ordered() {
        let (a, mut b) = ("1".to_string(), "2".to_string());
        for _ in 0..300 {
            let m = between(Some(&a), Some(&b)).unwrap();
            assert!(a < m && m < b && is_valid(&m));
            b = m;
        }
    }

    #[test]
    fn pseudo_random_insertions_keep_order() {
        let mut keys: Vec<String> = Vec::new();
        let mut seed = 0x2545F4914F6CDD1Du64;
        for _ in 0..2000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let i = (seed % (keys.len() as u64 + 1)) as usize;
            let before = i.checked_sub(1).map(|j| keys[j].as_str());
            let after = keys.get(i).map(String::as_str);
            let k = between(before, after).unwrap();
            keys.insert(i, k);
        }
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn sequence_is_increasing() {
        let s = sequence(500);
        assert!(s.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn ties_break_by_id() {
        assert_eq!(cmp_siblings(("a1", "x"), ("a1", "y")), Ordering::Less);
        assert_eq!(cmp_siblings(("a2", "a"), ("a1", "z")), Ordering::Greater);
    }
}
