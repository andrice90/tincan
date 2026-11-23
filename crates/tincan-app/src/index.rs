//! The hash index and the blob access rules.
//!
//! For every attachment or clip hash the app knows about it keeps a [`HashInfo`]:
//!
//! - `refs`: the messages that reference it ([`AttRef`], one per conversation and author),
//!   used to route downloads (`who_has` in each of those conversations, every author as a
//!   provider). Anyone who can post can add one, so a reference never grants access.
//! - `shared`: the conversations whose members (servers) or peer (DMs) may fetch it from
//!   me. A conversation is only added when I sent the file there, played the clip in its
//!   voice room or call, or downloaded the bytes from a peer that conversation pointed me
//!   to (an author of a reference in it, or a holder its host listed).
//!
//! Both lists are bounded per hash. Every change is persisted in kv (`app/att/<hash>`), so
//! the in-memory copy is only a bounded cache that evicts its oldest entry.
//!
//! [`Access`] is what the blobs provider consults on every connection and request. It
//! must answer from memory: the provider loop also paces every uploaded chunk.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tincan_core::{BlobHash, ConvId, UserId};
use tincan_files::AccessPolicy;

/// References kept per hash.
pub(crate) const MAX_REFS: usize = 8;
/// References kept per hash and conversation (distinct authors).
pub(crate) const MAX_REFS_PER_CONV: usize = 2;
/// Conversations a hash can be shared with.
pub(crate) const MAX_SHARED: usize = 16;
/// Hashes cached in memory (the rest are read back from kv on use).
pub(crate) const CACHED_HASHES: usize = 10_000;

/// One message's reference to a blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttRef {
    /// The conversation it was posted in (who_has goes there).
    pub conv: ConvId,
    /// That conversation is a DM.
    pub dm: bool,
    /// Who posted it.
    pub author: UserId,
    /// File name, as the author sent it.
    pub name: String,
    /// Size the author declared (a claim; the download checks the real size).
    pub size: u64,
    /// Declared MIME type.
    pub mime: String,
}

/// Everything the app knows about one hash.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HashInfo {
    /// Messages that reference it, oldest first.
    pub refs: Vec<AttRef>,
    /// Conversations it is shared with, oldest first.
    pub shared: Vec<ConvId>,
}

impl HashInfo {
    /// Adds a reference unless that conversation already has it from this author (or from
    /// [`MAX_REFS_PER_CONV`] authors). Past [`MAX_REFS`] the oldest DM reference goes first
    /// (anyone can open a DM; joining a server needs an invite), else the oldest one.
    /// True if the list changed.
    pub fn add_ref(&mut self, r: AttRef) -> bool {
        if self.refs.iter().any(|x| x.conv == r.conv && x.author == r.author) {
            return false;
        }
        if self.refs.iter().filter(|x| x.conv == r.conv).count() >= MAX_REFS_PER_CONV {
            return false;
        }
        self.refs.push(r);
        if self.refs.len() > MAX_REFS {
            let victim = self.refs.iter().position(|x| x.dm).unwrap_or(0);
            self.refs.remove(victim);
            return victim != self.refs.len();
        }
        true
    }

    /// Shares the hash with `conv`. Past [`MAX_SHARED`] the oldest share goes. True if the
    /// list changed.
    pub fn share(&mut self, conv: ConvId) -> bool {
        if self.shared.contains(&conv) {
            return false;
        }
        self.shared.push(conv);
        if self.shared.len() > MAX_SHARED {
            self.shared.remove(0);
        }
        true
    }

    /// The first reference (name, size and MIME shown for hash-only commands).
    pub fn primary(&self) -> Option<&AttRef> { self.refs.first() }

    /// Distinct conversations that reference it, oldest first.
    pub fn convs(&self) -> Vec<ConvId> {
        let mut out: Vec<ConvId> = Vec::new();
        for r in &self.refs {
            if !out.contains(&r.conv) {
                out.push(r.conv);
            }
        }
        out
    }

    /// The smallest size any reference declares.
    pub fn declared_size(&self) -> Option<u64> { self.refs.iter().map(|r| r.size).min() }

    /// Conversations in which `user` authored a reference.
    pub fn authored_by(&self, user: &UserId) -> Vec<ConvId> {
        self.refs.iter().filter(|r| r.author == *user).map(|r| r.conv).collect()
    }
}

#[derive(Serialize, Deserialize)]
struct RefRecord {
    conv: String,
    #[serde(default)]
    dm: bool,
    author: String,
    name: String,
    size: u64,
    mime: String,
}

#[derive(Serialize, Deserialize)]
struct InfoRecord {
    refs: Vec<RefRecord>,
    #[serde(default)]
    shared: Vec<String>,
}

fn ref_from(r: RefRecord) -> Option<AttRef> {
    Some(AttRef { conv: ConvId::from_hex(&r.conv)?, dm: r.dm, author: UserId::from_hex(&r.author)?, name: r.name, size: r.size, mime: r.mime })
}

/// The kv key of a hash's record.
pub(crate) fn key(hash: &BlobHash) -> String { format!("app/att/{}", hash.to_hex()) }

/// The persisted form of `info`.
pub(crate) fn encode(info: &HashInfo) -> Vec<u8> {
    let rec = InfoRecord {
        refs: info
            .refs
            .iter()
            .map(|r| RefRecord { conv: r.conv.to_hex(), dm: r.dm, author: r.author.to_hex(), name: r.name.clone(), size: r.size, mime: r.mime.clone() })
            .collect(),
        shared: info.shared.iter().map(ConvId::to_hex).collect(),
    };
    serde_json::to_vec(&rec).unwrap_or_default()
}

/// Parses a persisted record. A record from before the index kept several references holds
/// one reference: the conversation the file was sent or first downloaded in, which keeps
/// the access it had.
pub(crate) fn decode(raw: &[u8]) -> Option<HashInfo> {
    if let Ok(rec) = serde_json::from_slice::<InfoRecord>(raw) {
        return Some(HashInfo {
            refs: rec.refs.into_iter().filter_map(ref_from).take(MAX_REFS).collect(),
            shared: rec.shared.iter().filter_map(|c| ConvId::from_hex(c)).take(MAX_SHARED).collect(),
        });
    }
    let old = ref_from(serde_json::from_slice::<RefRecord>(raw).ok()?)?;
    Some(HashInfo { shared: vec![old.conv], refs: vec![old] })
}

/// A map that keeps at most `cap` entries and evicts the oldest inserted key.
pub(crate) struct Bounded<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
    cap: usize,
}

impl<K: Eq + Hash + Copy, V> Bounded<K, V> {
    /// Empty, holding at most `cap` entries.
    pub fn new(cap: usize) -> Self { Self { map: HashMap::new(), order: VecDeque::new(), cap: cap.max(1) } }

    /// The value of `k`.
    pub fn get(&self, k: &K) -> Option<&V> { self.map.get(k) }

    /// The value of `k`, mutable.
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> { self.map.get_mut(k) }

    /// Is `k` present?
    pub fn contains_key(&self, k: &K) -> bool { self.map.contains_key(k) }

    /// Inserts or replaces; a new key may evict the oldest one.
    pub fn insert(&mut self, k: K, v: V) {
        if self.map.insert(k, v).is_none() {
            self.order.push_back(k);
            while self.map.len() > self.cap {
                let Some(old) = self.order.pop_front() else { break };
                self.map.remove(&old);
            }
        }
    }

    /// Removes `k` (linear in the size; for rare removals).
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let v = self.map.remove(k)?;
        self.order.retain(|x| x != k);
        Some(v)
    }

    /// Number of entries.
    #[cfg(test)]
    pub fn len(&self) -> usize { self.map.len() }

    /// Every key.
    pub fn keys(&self) -> impl Iterator<Item = &K> { self.map.keys() }
}

/// Who is in one conversation, for the access check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Members {
    /// A DM (`users` is the peer).
    pub dm: bool,
    /// Current members of a server, or the peer of a DM.
    pub users: HashSet<UserId>,
}

/// The access state the blobs provider reads.
#[derive(Debug, Default)]
pub(crate) struct Access {
    /// Everyone in any of my conversations (a blobs connection needs this).
    peers: HashSet<UserId>,
    /// Members of every conversation I am in.
    convs: HashMap<ConvId, Members>,
    /// For each blob shared anywhere: the conversations it is shared with.
    blobs: HashMap<BlobHash, Vec<ConvId>>,
}

impl Access {
    /// Replaces the conversations (from the current views); I am never a peer. True if the
    /// set of conversations (or which of them are DMs) changed, which is all that
    /// [`Access::announces`] depends on; a membership change alone returns false.
    pub fn set_convs(&mut self, convs: HashMap<ConvId, Members>, me: &UserId) -> bool {
        let same = convs.len() == self.convs.len() && convs.iter().all(|(c, m)| self.convs.get(c).is_some_and(|old| old.dm == m.dm));
        self.peers = convs.values().flat_map(|m| m.users.iter().copied()).filter(|u| u != me).collect();
        self.convs = convs;
        !same
    }

    /// A DM conversation appeared.
    pub fn add_dm(&mut self, conv: ConvId, peer: UserId) {
        self.peers.insert(peer);
        self.convs.insert(conv, Members { dm: true, users: HashSet::from([peer]) });
    }

    /// The conversations `hash` is shared with.
    pub fn set_shared(&mut self, hash: BlobHash, shared: &[ConvId]) {
        if shared.is_empty() {
            self.blobs.remove(&hash);
        } else {
            self.blobs.insert(hash, shared.to_vec());
        }
    }

    /// May `peer` open a blobs connection?
    pub fn allow_peer(&self, peer: &UserId) -> bool { self.peers.contains(peer) }

    /// May `peer` fetch `hash`? Only if the hash is shared with a conversation the peer is
    /// in right now: a current member of that server, or the peer of that DM.
    pub fn allow_blob(&self, peer: &UserId, hash: &BlobHash) -> bool {
        self.blobs
            .get(hash)
            .is_some_and(|convs| convs.iter().any(|c| self.convs.get(c).is_some_and(|m| m.users.contains(peer))))
    }

    /// Should `hash` be in my `Have` list? Only when it is shared with a server I am in:
    /// DM `who_has` always names the contact, so a DM-only blob needs no announcement.
    pub fn announces(&self, hash: &BlobHash) -> bool { !self.servers_sharing(hash).is_empty() }

    /// The servers I am in that `hash` is shared with: the hosts that may be told I hold it.
    pub fn servers_sharing(&self, hash: &BlobHash) -> Vec<ConvId> {
        self.blobs.get(hash).map(|convs| convs.iter().filter(|c| self.convs.get(c).is_some_and(|m| !m.dm)).copied().collect()).unwrap_or_default()
    }
}

/// The [`AccessPolicy`] given to tincan-files.
pub(crate) struct Policy(pub Arc<RwLock<Access>>);

impl AccessPolicy for Policy {
    fn allow(&self, peer: &UserId) -> bool { self.0.read().allow_peer(peer) }
    fn allow_blob(&self, peer: &UserId, hash: &BlobHash) -> bool { self.0.read().allow_blob(peer, hash) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(i: u8) -> UserId { UserId::from_bytes([i; 32]) }
    fn conv(i: u8) -> ConvId { ConvId([i; 32]) }
    fn r(c: u8, dm: bool, author: u8) -> AttRef {
        AttRef { conv: conv(c), dm, author: user(author), name: "f.png".into(), size: 10, mime: "image/png".into() }
    }

    #[test]
    fn references_are_kept_per_conversation_and_author() {
        let mut i = HashInfo::default();
        assert!(i.add_ref(r(1, false, 1)));
        assert!(!i.add_ref(r(1, false, 1)), "the same author again changes nothing");
        assert!(i.add_ref(r(1, false, 2)), "a second author in the same server");
        assert!(!i.add_ref(r(1, false, 3)), "at most two authors per conversation");
        assert!(i.add_ref(r(2, false, 3)), "another conversation is not first-writer-wins");
        assert_eq!(i.convs(), vec![conv(1), conv(2)]);
        assert_eq!(i.primary().map(|x| x.conv), Some(conv(1)));
        assert_eq!(i.authored_by(&user(3)), vec![conv(2)]);
    }

    #[test]
    fn dm_references_are_evicted_first() {
        let mut i = HashInfo::default();
        for c in 0..MAX_REFS as u8 {
            assert!(i.add_ref(r(c, false, 1)));
        }
        // A full list of server references: a new DM reference does not push one out.
        assert!(!i.add_ref(r(100, true, 9)));
        assert_eq!(i.refs.len(), MAX_REFS);
        assert!(i.refs.iter().all(|x| !x.dm));
        // A new server reference pushes out the oldest.
        assert!(i.add_ref(r(200, false, 1)));
        assert_eq!(i.refs.len(), MAX_REFS);
        assert!(!i.convs().contains(&conv(0)));

        let mut j = HashInfo::default();
        assert!(j.add_ref(r(1, true, 5)));
        for c in 10..(10 + MAX_REFS as u8 - 1) {
            assert!(j.add_ref(r(c, false, 1)));
        }
        assert!(j.add_ref(r(50, false, 1)));
        assert!(!j.convs().contains(&conv(1)), "the DM reference went first");
        assert!(j.convs().contains(&conv(10)));
    }

    #[test]
    fn shares_are_bounded() {
        let mut i = HashInfo::default();
        assert!(i.share(conv(1)));
        assert!(!i.share(conv(1)));
        for c in 2..=(MAX_SHARED as u8 + 1) {
            assert!(i.share(conv(c)));
        }
        assert_eq!(i.shared.len(), MAX_SHARED);
        assert!(!i.shared.contains(&conv(1)));
    }

    #[test]
    fn records_round_trip_and_read_the_old_format() {
        let mut i = HashInfo::default();
        i.add_ref(r(1, true, 1));
        i.add_ref(r(2, false, 2));
        i.share(conv(2));
        assert_eq!(decode(&encode(&i)), Some(i));

        let old = serde_json::json!({
            "conv": conv(3).to_hex(), "author": user(4).to_hex(), "name": "a.pdf", "size": 5, "mime": "application/pdf"
        });
        let got = decode(&serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(got.refs.len(), 1);
        assert_eq!(got.refs[0].author, user(4));
        assert_eq!(got.shared, vec![conv(3)], "an old record keeps the access it had");
        assert!(decode(b"not json").is_none());
    }

    #[test]
    fn bounded_map_evicts_the_oldest() {
        let mut m: Bounded<u32, u32> = Bounded::new(3);
        for k in 0..5 {
            m.insert(k, k);
        }
        assert_eq!(m.len(), 3);
        assert!(!m.contains_key(&0) && !m.contains_key(&1));
        m.insert(4, 40);
        assert_eq!(m.get(&4), Some(&40), "replacing keeps the entry");
        assert_eq!(m.remove(&3), Some(3));
        m.insert(5, 5);
        m.insert(6, 6);
        assert_eq!(m.len(), 3);
        assert!(m.contains_key(&4) && m.contains_key(&5) && m.contains_key(&6));
    }

    #[test]
    fn access_is_per_conversation_membership() {
        let me = user(0);
        let (alice, bob, carol, stranger) = (user(1), user(2), user(3), user(4));
        let (server, dm_ab, dm_stranger) = (conv(1), conv(2), conv(3));
        let mut a = Access::default();
        let mut convs = HashMap::new();
        convs.insert(server, Members { dm: false, users: HashSet::from([me, alice, carol]) });
        convs.insert(dm_ab, Members { dm: true, users: HashSet::from([bob]) });
        assert!(a.set_convs(convs.clone(), &me), "the first set changes the conversations");
        let mut more = convs.clone();
        more.get_mut(&server).unwrap().users.insert(user(9));
        assert!(!a.set_convs(more, &me), "a membership change alone does not");
        a.set_convs(convs, &me);
        a.add_dm(dm_stranger, stranger);
        assert!(a.allow_peer(&alice) && a.allow_peer(&bob) && a.allow_peer(&stranger));
        assert!(!a.allow_peer(&me));

        let server_file = BlobHash([10; 32]);
        let dm_file = BlobHash([11; 32]);
        a.set_shared(server_file, &[server]);
        a.set_shared(dm_file, &[dm_ab]);

        assert!(a.allow_blob(&carol, &server_file));
        assert!(!a.allow_blob(&carol, &dm_file), "a server member cannot fetch a DM-only file");
        assert!(a.allow_blob(&bob, &dm_file));
        assert!(!a.allow_blob(&bob, &server_file), "a DM contact cannot fetch server files");
        assert!(!a.allow_blob(&stranger, &server_file), "nor can a stranger who DMed me");
        assert!(!a.allow_blob(&alice, &BlobHash([12; 32])), "unshared blobs are never served");

        assert!(a.announces(&server_file));
        assert!(!a.announces(&dm_file), "DM-only blobs are not announced");
        assert_eq!(a.servers_sharing(&server_file), vec![server], "announced to that server's host only");
        assert!(a.servers_sharing(&dm_file).is_empty());

        // Leaving the server takes the access and the announcement away.
        let mut convs = HashMap::new();
        convs.insert(dm_ab, Members { dm: true, users: HashSet::from([bob]) });
        assert!(a.set_convs(convs, &me), "leaving a server changes the conversations");
        assert!(!a.allow_blob(&carol, &server_file));
        assert!(!a.announces(&server_file));
        assert!(!a.allow_peer(&carol));
    }
}
