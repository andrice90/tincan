//! History sync by per-author heads ("version vector").
//!
//! Because only contiguous events are stored, a peer's knowledge of a
//! conversation is fully described by `author -> highest contiguous seq`.
//!
//! Send order: per author in ascending seq (receivers only apply contiguous runs). To keep
//! the receiver's `state::PendingBuffer` small, send authors in dependency order: the Genesis
//! owner first, then each admitting host before the members it admitted
//! (`state::ConvState::sync_author_order`). Any order converges, it just buffers more.

use std::collections::HashMap;

use crate::UserId;
use serde::{Deserialize, Serialize};

/// `(author, highest contiguous seq)` pairs, sorted by author when built with `from_map`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heads(pub Vec<(UserId, u64)>);

impl Heads {
    /// From a map (sorted by author, so equal maps give equal `Heads`).
    pub fn from_map(m: &HashMap<UserId, u64>) -> Self {
        let mut v: Vec<(UserId, u64)> = m.iter().map(|(u, s)| (*u, *s)).collect();
        v.sort();
        Self(v)
    }
    /// To a map (a duplicated author keeps the last value).
    pub fn to_map(&self) -> HashMap<UserId, u64> { self.0.iter().copied().collect() }
    /// For each author where `self` is ahead of `theirs`: (author, first_missing_seq, last_seq).
    pub fn missing_in(&self, theirs: &Heads) -> Vec<(UserId, u64, u64)> {
        let t = theirs.to_map();
        let mut out = Vec::new();
        for (u, mine) in &self.0 {
            let have = t.get(u).copied().unwrap_or(0);
            if *mine > have {
                out.push((*u, have + 1, *mine));
            }
        }
        out
    }
    /// BLAKE3 digest of the sorted heads, for cheap periodic comparison (VvDigest).
    /// Authors with head 0 are ignored, so "no events" and "absent" compare equal.
    pub fn digest(&self) -> [u8; 32] {
        let mut v: Vec<(UserId, u64)> = self.0.iter().copied().filter(|(_, s)| *s > 0).collect();
        v.sort();
        v.dedup_by_key(|(u, _)| *u);
        let bytes = postcard::to_stdvec(&v).unwrap_or_default();
        *blake3::hash(&bytes).as_bytes()
    }
}

/// Maximum bytes of events per `Ctl::Events` page. Events are sent per author in
/// ascending seq order (the receiver only stores contiguous runs), authors in dependency
/// order (see module docs).
pub const SYNC_PAGE_BYTES: usize = 256 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    fn u(b: u8) -> UserId { UserId([b; 32]) }

    #[test]
    fn missing_in_reports_only_what_they_lack() {
        let mine = Heads(vec![(u(1), 5), (u(2), 3), (u(3), 7)]);
        let theirs = Heads(vec![(u(1), 5), (u(2), 1), (u(4), 9)]);
        let mut m = mine.missing_in(&theirs);
        m.sort();
        assert_eq!(m, vec![(u(2), 2, 3), (u(3), 1, 7)]);
        let mut back = theirs.missing_in(&mine);
        back.sort();
        assert_eq!(back, vec![(u(4), 1, 9)]);
        assert!(mine.missing_in(&mine).is_empty());
        assert!(Heads::default().missing_in(&mine).is_empty());
        assert_eq!(mine.missing_in(&Heads::default()).len(), 3);
    }

    #[test]
    fn map_round_trip_is_sorted() {
        let mut m = HashMap::new();
        m.insert(u(3), 1);
        m.insert(u(1), 2);
        m.insert(u(2), 3);
        let h = Heads::from_map(&m);
        assert_eq!(h.0, vec![(u(1), 2), (u(2), 3), (u(3), 1)]);
        assert_eq!(h.to_map(), m);
    }

    #[test]
    fn digest_ignores_order_and_zero_heads() {
        let a = Heads(vec![(u(1), 5), (u(2), 3)]);
        let b = Heads(vec![(u(2), 3), (u(1), 5), (u(9), 0)]);
        assert_eq!(a.digest(), b.digest());
        let c = Heads(vec![(u(1), 5), (u(2), 4)]);
        assert_ne!(a.digest(), c.digest());
        assert_eq!(Heads::default().digest(), Heads(vec![(u(1), 0)]).digest());
    }

    #[test]
    fn heads_are_compact_on_the_wire() {
        let h = Heads(vec![(u(1), 1000)]);
        let bytes = postcard::to_stdvec(&h).unwrap();
        assert!(bytes.len() <= 40, "{} bytes per author", bytes.len());
    }
}
