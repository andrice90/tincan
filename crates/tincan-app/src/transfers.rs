//! Attachments: import on send, auto-fetch on arrival, `transfer` / `download` /
//! `open_file` / `file_url`, resume after restart, the blob URL backing, and the hash index
//! bookkeeping (references, shares and download nominations; the rules are in `index.rs`).
//!
//! Every reference a message makes to a hash is indexed (`update_info`, persisted in kv
//! `app/att/<hash>`). A download asks `who_has` in every conversation that references the
//! hash and tries every author. When it completes, the blob is shared with the
//! conversations that named the peer who delivered it.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tincan_core::event::{Attachment, Body, StoredEvent};
use tincan_core::storage::Storage;
use tincan_core::wire::AddrHint;
use tincan_core::{BlobHash, ConvId, UserId};
use tincan_files::{BlobKind, TransferInfo, TransferState as FilesState};
use tincan_store::DownloadRecord;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::app::Core;
use crate::dto::{self, TransferState};
use crate::emit::Ev;
use crate::index::{self, AttRef, Bounded, HashInfo};
use crate::pump::REQUERY_EVERY;
use crate::settings::MAX_ATTACHMENT_BYTES;
use crate::shell;

/// Nominations kept per hash.
const MAX_NOMINATIONS: usize = 64;
/// Hashes the worker remembers having auto-fetched.
const SEEN_AUTO: usize = 10_000;
/// The refusal of an attachment over the size limit.
pub(crate) const TOO_LARGE: &str = "This file is larger than 25 MB, so Tincan won't download it.";

/// Work for the background fetch worker (one at a time). Holder lookups, retries and
/// resumes go through a bounded channel and are served before auto-fetches.
#[derive(Debug)]
pub(crate) enum FetchReq {
    /// Auto-fetch an attachment that just arrived (from [`AutoQueue`]).
    Auto {
        /// The blob.
        hash: BlobHash,
    },
    /// Look up holders for `Transfer.peers`.
    Peers {
        /// The blob.
        hash: BlobHash,
    },
    /// A parked (unavailable) download: look for holders again.
    Requery {
        /// The blob.
        hash: BlobHash,
    },
    /// A partial download from before the restart.
    Resume {
        /// The blob.
        hash: BlobHash,
        /// Its kind.
        kind: BlobKind,
    },
}

/// Auto-fetch candidates. Newest first: a history sync streams the oldest messages first,
/// and the newest are the ones on screen and the likeliest to lose their holders. When full,
/// the oldest candidate is dropped (it can still be downloaded on demand).
pub(crate) struct AutoQueue {
    items: parking_lot::Mutex<VecDeque<BlobHash>>,
    cap: usize,
    /// Wakes the worker after a push.
    pub(crate) notify: Notify,
}

impl AutoQueue {
    /// Empty, holding at most `cap` hashes.
    pub(crate) fn new(cap: usize) -> Self { Self { items: parking_lot::Mutex::new(VecDeque::new()), cap: cap.max(1), notify: Notify::new() } }

    /// Add a candidate (moved to the front if already queued).
    pub(crate) fn push(&self, hash: BlobHash) {
        {
            let mut q = self.items.lock();
            q.retain(|h| *h != hash);
            if q.len() >= self.cap
                && let Some(old) = q.pop_front()
            {
                tracing::debug!(hash = %old.short(), "auto-fetch queue full; the oldest can still be downloaded on demand");
            }
            q.push_back(hash);
        }
        self.notify.notify_one();
    }

    /// The newest candidate.
    pub(crate) fn pop(&self) -> Option<BlobHash> { self.items.lock().pop_back() }
}

fn export_key(hash: &BlobHash) -> String { format!("app/export/{}", hash.to_hex()) }
fn refused_key(hash: &BlobHash) -> String { format!("app/refused/{}", hash.to_hex()) }

/// Parse a blob hash argument.
pub(crate) fn parse_hash(s: &str) -> Result<BlobHash, String> {
    BlobHash::from_hex(s).ok_or_else(|| "That file reference is not valid.".to_string())
}

/// The UI state of a tincan-files download state.
pub(crate) fn map_state(s: FilesState) -> TransferState {
    match s {
        FilesState::Queued => TransferState::Queued,
        FilesState::Downloading => TransferState::Downloading,
        FilesState::Complete => TransferState::Complete,
        FilesState::Unavailable => TransferState::Unavailable,
        FilesState::Failed | FilesState::TooLarge => TransferState::Failed,
    }
}

/// May an attachment that declares `size` bytes be fetched automatically under the
/// `autoFetchLimit` setting `limit`? Never past the 25 MiB attachment limit, whatever the
/// setting says. (The download also checks the real size.)
pub(crate) fn auto_fetch_ok(size: u64, limit: u64) -> bool { limit > 0 && size <= limit.min(MAX_ATTACHMENT_BYTES) }

/// Is a parked download due for another holder lookup? At most once every `every` per
/// hash, decided before it is queued, so a burst of roster updates queues each hash once.
pub(crate) fn requery_due(last: Option<Instant>, now: Instant, every: Duration) -> bool {
    last.is_none_or(|t| now.saturating_duration_since(t) >= every)
}

/// The providers worth waking a parked download for: the peers online now. An offline
/// author would only cost a download slot and a 4 s connect timeout.
pub(crate) fn online_only(providers: Vec<(UserId, AddrHint)>, online: impl Fn(&UserId) -> bool) -> Vec<(UserId, AddrHint)> {
    providers.into_iter().filter(|(u, _)| online(u)).collect()
}

/// The background worker: holder lookups, parked-download retries and resumes first, then
/// auto-fetches newest first, one at a time so `who_has` traffic stays bounded.
pub(crate) async fn fetch_worker(core: Arc<Core>, mut rx: mpsc::Receiver<FetchReq>) {
    let mut seen: Bounded<BlobHash, ()> = Bounded::new(SEEN_AUTO);
    loop {
        if core.is_closed() {
            break;
        }
        let req = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::error::TryRecvError::Disconnected) => break,
            Err(mpsc::error::TryRecvError::Empty) => match core.auto.pop() {
                Some(hash) => FetchReq::Auto { hash },
                None => {
                    tokio::select! {
                        biased;
                        r = rx.recv() => match r {
                            Some(r) => r,
                            None => break,
                        },
                        () = core.auto.notify.notified() => continue,
                    }
                }
            },
        };
        match req {
            FetchReq::Auto { hash } => {
                if seen.contains_key(&hash) || core.files.is_complete(&hash) || core.files.transfer(hash).is_some() || core.is_refused(&hash) {
                    continue;
                }
                seen.insert(hash, ());
                if let Some(info) = core.att_info(&hash).filter(|i| !i.refs.is_empty()) {
                    core.start_download(hash, &info, BlobKind::Attachment).await;
                }
            }
            FetchReq::Peers { hash } => {
                if let Some(info) = core.att_info(&hash).filter(|i| !i.refs.is_empty()) {
                    core.lookup_holders(hash, &info).await;
                    core.events.emit(Ev::TransferUpdated, core.transfer_dto(&hash));
                }
            }
            FetchReq::Requery { hash } => {
                if let Some(info) = core.att_info(&hash).filter(|i| !i.refs.is_empty()) {
                    let holders = core.lookup_holders(hash, &info).await;
                    // Only peers online now: re-adding an offline author would wake the
                    // download just to spend a slot on a dead dial.
                    let providers = online_only(core.providers(&info, holders), |u| core.net.peer_online(u));
                    if !providers.is_empty() {
                        core.files.add_providers(hash, providers);
                    }
                }
            }
            FetchReq::Resume { hash, kind } => {
                if core.files.is_complete(&hash) || core.files.transfer(hash).is_some() {
                    continue;
                }
                match core.att_info(&hash).filter(|i| !i.refs.is_empty()) {
                    Some(info) if kind == BlobKind::Attachment && info.declared_size().is_some_and(|s| s > MAX_ATTACHMENT_BYTES) => {
                        tracing::debug!(hash = %hash.short(), "not resuming an attachment declared over the size limit");
                    }
                    Some(info) => core.start_download(hash, &info, kind).await,
                    None => {
                        // Origin unknown (a clip, or an index entry lost): ask every
                        // conversation. A DM's who_has names the contact.
                        let mut providers: Vec<(UserId, AddrHint)> = Vec::new();
                        for conv in core.net.convs() {
                            let holders = core.net.who_has(&conv, hash).await;
                            core.nominate(hash, conv, holders.iter().map(|(u, _)| *u));
                            for (u, a) in holders {
                                if !providers.iter().any(|(x, _)| *x == u) {
                                    providers.push((u, a));
                                }
                            }
                        }
                        core.note_holders(hash, &providers);
                        core.files.download(hash, kind, providers);
                    }
                }
            }
        }
    }
}

impl Core {
    // ------------------------------------------------------------------ the hash index

    fn load_info(&self, hash: &BlobHash) -> Option<HashInfo> {
        let raw = self.storage.get_kv(&index::key(hash)).ok().flatten()?;
        index::decode(&raw)
    }

    /// What the index knows about `hash`: the cache, then kv.
    pub(crate) fn att_info(&self, hash: &BlobHash) -> Option<HashInfo> {
        let mut cache = self.atts.lock();
        if let Some(i) = cache.get(hash) {
            return Some(i.clone());
        }
        let info = self.load_info(hash)?;
        cache.insert(*hash, info.clone());
        Some(info)
    }

    /// The first reference of `hash` (name, size and type for hash-only commands).
    pub(crate) fn att_ref(&self, hash: &BlobHash) -> Option<AttRef> { self.att_info(hash)?.primary().cloned() }

    /// Change the index entry of `hash` with `f` (which returns true when it changed it).
    /// A change is cached and persisted; a change to the shares also updates the access
    /// state and the announcements. True if it changed.
    pub(crate) fn update_info(&self, hash: BlobHash, f: impl FnOnce(&mut HashInfo) -> bool) -> bool {
        let shared = {
            let mut cache = self.atts.lock();
            let mut info = match cache.get(&hash) {
                Some(i) => i.clone(),
                None => self.load_info(&hash).unwrap_or_default(),
            };
            let before = info.shared.clone();
            if !f(&mut info) {
                cache.insert(hash, info);
                return false;
            }
            // Persisted under the lock, so two updates of one hash cannot reorder in kv.
            if let Err(e) = self.storage.set_kv(&index::key(&hash), &index::encode(&info)) {
                tracing::warn!(error = %e, "saving the attachment index failed");
            }
            let shared = (info.shared != before).then(|| info.shared.clone());
            cache.insert(hash, info);
            shared
        };
        if let Some(shared) = shared {
            self.access.write().set_shared(hash, &shared);
            self.announce_changed(hash);
        }
        true
    }

    /// The shares of `hash` changed: update what I announce. A held attachment that is now
    /// shared with a server is one `have_add`; anything else (it stopped being announced, or
    /// it is a clip, whose library entry is announced too) recomputes both lists.
    fn announce_changed(&self, hash: BlobHash) {
        if !self.files.is_complete(&hash) {
            return;
        }
        let announces = self.access.read().announces(&hash);
        let clip = matches!(self.storage.get_clip(&hash), Ok(Some(_)));
        if announces && !clip { self.net.have_add(hash) } else { self.announce() }
    }

    /// Share `hash` with `conv`: I sent it there, or played the clip there.
    pub(crate) fn share_blob(&self, hash: BlobHash, conv: ConvId) { self.update_info(hash, |i| i.share(conv)); }

    /// Record where the attachments of a message can be fetched.
    ///
    /// A reference alone shares nothing (anyone who can post can name any hash). One
    /// exception: when its author may already fetch the blob from me (it is shared with a
    /// conversation the author is in), the author could pass the bytes on anyway, so I
    /// serve the conversation it was reposted in too.
    ///
    /// A new reference also wakes a parked download of the hash (`requery_on_new_ref`).
    pub(crate) fn index_attachments(&self, conv: ConvId, dm: bool, author: UserId, attachments: &[Attachment]) {
        for a in attachments {
            let reshare = author != self.me && self.files.is_complete(&a.hash) && self.access.read().allow_blob(&author, &a.hash);
            let r = AttRef { conv, dm, author, name: a.name.clone(), size: a.size, mime: a.mime.clone() };
            let mut added = false;
            self.update_info(a.hash, |i| {
                added = i.add_ref(r);
                // Both run: an earlier reference may predate the blob being shared.
                added | (reshare && i.share(conv))
            });
            if added {
                self.requery_on_new_ref(a.hash, dm);
            }
        }
    }

    /// Remember that `conv` named `users` as providers of `hash`.
    pub(crate) fn nominate(&self, hash: BlobHash, conv: ConvId, users: impl IntoIterator<Item = UserId>) {
        let mut n = self.nominations.lock();
        let fresh: Vec<(UserId, ConvId)> = users.into_iter().filter(|u| *u != self.me).map(|u| (u, conv)).collect();
        if fresh.is_empty() {
            return;
        }
        match n.get_mut(&hash) {
            Some(list) => {
                for e in fresh {
                    if list.len() < MAX_NOMINATIONS && !list.contains(&e) {
                        list.push(e);
                    }
                }
            }
            None => {
                let mut list = fresh;
                list.dedup();
                list.truncate(MAX_NOMINATIONS);
                n.insert(hash, list);
            }
        }
    }

    /// A download of `hash` completed with the last bytes from `from`: share it with the
    /// conversations that named that peer (it posted the file there, or that conversation's
    /// host listed it as a holder). A message that only references the hash shares nothing.
    pub(crate) fn share_with_nominators(&self, hash: BlobHash, from: UserId) {
        let mut convs: Vec<ConvId> = self
            .nominations
            .lock()
            .get(&hash)
            .map(|l| l.iter().filter(|(u, _)| *u == from).map(|(_, c)| *c).collect())
            .unwrap_or_default();
        if let Some(info) = self.att_info(&hash) {
            for c in info.authored_by(&from) {
                if !convs.contains(&c) {
                    convs.push(c);
                }
            }
        }
        if convs.is_empty() {
            tracing::debug!(hash = %hash.short(), from = %from.short(), "no conversation named the provider; the blob stays unshared");
            return;
        }
        self.update_info(hash, |i| {
            let mut changed = false;
            for c in &convs {
                changed |= i.share(*c);
            }
            changed
        });
    }

    /// `FileEvent::Completed`: share by nomination, hand the shares to the access check
    /// (the record may already have listed them, for example a resumed download), then
    /// announce the hash if it is shared with a server.
    pub(crate) fn on_completed(&self, hash: BlobHash, from: Option<UserId>) {
        if let Some(from) = from {
            self.share_with_nominators(hash, from);
        }
        self.nominations.lock().remove(&hash);
        self.load_access(hash);
    }

    /// Copy the persisted shares of a held `hash` into the access state and announce it
    /// when it is shared with a server I am in.
    pub(crate) fn load_access(&self, hash: BlobHash) {
        let shared = self.att_info(&hash).map(|i| i.shared).unwrap_or_default();
        let announces = {
            let mut a = self.access.write();
            a.set_shared(hash, &shared);
            a.announces(&hash)
        };
        if announces && self.files.is_complete(&hash) {
            self.net.have_add(hash);
        }
    }

    /// The file event stream lagged: completions and state changes may be lost. Recover
    /// the shares of downloads that finished meanwhile, re-announce, and re-read every
    /// transfer the app tracks (which also fixes the download records and the UI).
    pub(crate) fn on_files_lagged(self: &Arc<Self>) {
        let finished: Vec<BlobHash> = {
            let n = self.nominations.lock();
            n.keys().copied().filter(|h| self.files.is_complete(h)).collect()
        };
        for hash in finished {
            self.on_completed(hash, self.files.provided_by(&hash));
        }
        self.announce();
        let mut hashes: HashSet<BlobHash> = self.transfer_states.lock().keys().copied().collect();
        match self.storage.list_downloads() {
            Ok(list) => {
                let open = [TransferState::Queued, TransferState::Downloading, TransferState::Unavailable].map(|s| s.as_str());
                hashes.extend(list.into_iter().filter(|r| open.contains(&r.state.as_str())).map(|r| r.hash));
            }
            Err(e) => tracing::warn!(error = %e, "reading download records failed"),
        }
        for hash in hashes {
            if let Some(info) = self.files.transfer(hash) {
                self.on_transfer(&info);
            }
        }
        self.media.on_files_lagged(self);
    }

    // ------------------------------------------------------------------ holders

    /// Remember the holders from a `who_has` round.
    pub(crate) fn note_holders(&self, hash: BlobHash, holders: &[(UserId, AddrHint)]) {
        self.holders.lock().insert(hash, holders.iter().map(|(u, _)| *u).collect());
    }

    /// `who_has` in every conversation that references `hash`. Records the holders and who
    /// named whom (holders and authors). Returns the holders, each once.
    pub(crate) async fn lookup_holders(&self, hash: BlobHash, info: &HashInfo) -> Vec<(UserId, AddrHint)> {
        let mut all: Vec<(UserId, AddrHint)> = Vec::new();
        for conv in info.convs() {
            let holders = self.net.who_has(&conv, hash).await;
            self.nominate(hash, conv, holders.iter().map(|(u, _)| *u));
            for (u, a) in holders {
                if !all.iter().any(|(x, _)| *x == u) {
                    all.push((u, a));
                }
            }
        }
        for r in &info.refs {
            self.nominate(hash, r.conv, [r.author]);
        }
        self.note_holders(hash, &all);
        all
    }

    /// Online holders of `hash` (authors count when online), never me.
    fn online_holders(&self, hash: &BlobHash, info: Option<&HashInfo>) -> u32 {
        let mut users: Vec<UserId> = self.holders.lock().get(hash).cloned().unwrap_or_default();
        for r in info.map(|i| i.refs.as_slice()).unwrap_or_default() {
            if !users.contains(&r.author) {
                users.push(r.author);
            }
        }
        let n = users.iter().filter(|u| **u != self.me && self.net.peer_online(u)).count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Provider order for a download: online authors, then the other holders, then offline
    /// authors last (a dead first provider costs the 4 s connect timeout).
    pub(crate) fn providers(&self, info: &HashInfo, holders: Vec<(UserId, AddrHint)>) -> Vec<(UserId, AddrHint)> {
        let mut authors: Vec<(UserId, ConvId)> = Vec::new();
        for r in &info.refs {
            if r.author != self.me && !authors.iter().any(|(u, _)| *u == r.author) {
                authors.push((r.author, r.conv));
            }
        }
        let addr_of = |user: &UserId, conv: &ConvId| -> AddrHint {
            holders
                .iter()
                .find(|(u, _)| u == user)
                .map(|(_, a)| a.clone())
                .or_else(|| self.net.roster(conv).and_then(|r| r.online.iter().find(|e| e.user == *user).map(|e| e.addr.clone())))
                .unwrap_or_default()
        };
        let (online, offline): (Vec<_>, Vec<_>) = authors.into_iter().partition(|(u, _)| self.net.peer_online(u));
        let mut out: Vec<(UserId, AddrHint)> = online.iter().map(|(u, c)| (*u, addr_of(u, c))).collect();
        for (u, a) in &holders {
            if *u != self.me && !out.iter().any(|(x, _)| x == u) {
                out.push((*u, a.clone()));
            }
        }
        for (u, c) in &offline {
            if !out.iter().any(|(x, _)| x == u) {
                out.push((*u, addr_of(u, c)));
            }
        }
        out
    }

    // ------------------------------------------------------------------ downloads

    /// Record, look up holders and queue the download of `hash`.
    pub(crate) async fn start_download(&self, hash: BlobHash, info: &HashInfo, kind: BlobKind) {
        let Some(primary) = info.primary() else { return };
        if kind == BlobKind::Attachment
            && let Err(e) = self.storage.upsert_download(&DownloadRecord {
                hash,
                name: shell::clean_text(&primary.name),
                size: primary.size,
                state: TransferState::Queued.as_str().to_string(),
            })
        {
            tracing::warn!(error = %e, "saving a download record failed");
        }
        let holders = self.lookup_holders(hash, info).await;
        let providers = self.providers(info, holders);
        tracing::debug!(hash = %hash.short(), providers = providers.len(), "download requested");
        self.files.download(hash, kind, providers);
    }

    /// Index the attachments of an incoming message and queue auto-fetches for those that
    /// declare at most `autoFetchLimit` (and never past 25 MiB).
    pub(crate) fn auto_fetch(&self, conv: ConvId, ev: &StoredEvent) {
        let Body::Message { attachments, .. } = &ev.event.body else { return };
        if attachments.is_empty() {
            return;
        }
        let dm = self.view(&conv).is_some_and(|v| v.is_dm);
        self.index_attachments(conv, dm, ev.event.author, attachments);
        if ev.event.author == self.me {
            return;
        }
        let limit = self.settings.read().auto_fetch_limit;
        for a in attachments {
            if auto_fetch_ok(a.size, limit) && !self.files.is_complete(&a.hash) && self.files.transfer(a.hash).is_none() {
                self.auto.push(a.hash);
            }
        }
    }

    /// Ask for holders again for every parked download (rosters changed, or a DM link came
    /// up). Each hash at most every [`REQUERY_EVERY`], checked before it is queued.
    pub(crate) fn requery_parked(&self) {
        let parked: Vec<BlobHash> =
            self.transfer_states.lock().iter().filter(|(_, s)| **s == FilesState::Unavailable).map(|(h, _)| *h).collect();
        let now = Instant::now();
        let mut done = self.requeried.lock();
        done.retain(|_, t| !requery_due(Some(*t), now, REQUERY_EVERY));
        for hash in parked {
            if !requery_due(done.get(&hash).copied(), now, REQUERY_EVERY) {
                continue;
            }
            if self.fetch_tx.try_send(FetchReq::Requery { hash }).is_ok() {
                done.insert(hash, now);
            }
        }
    }

    /// A message added a reference to `hash` in `conv`: if its download is parked, look for
    /// holders again, now that one more conversation can name them. A server reference does
    /// this at once (joining a server takes an invite); a DM reference waits its turn like
    /// a roster change (anyone can open a DM).
    fn requery_on_new_ref(&self, hash: BlobHash, dm: bool) {
        if self.files.transfer(hash).is_none_or(|t| t.state != FilesState::Unavailable) {
            return;
        }
        let now = Instant::now();
        let mut done = self.requeried.lock();
        if dm && !requery_due(done.get(&hash).copied(), now, REQUERY_EVERY) {
            return;
        }
        if self.fetch_tx.try_send(FetchReq::Requery { hash }).is_ok() {
            tracing::debug!(hash = %hash.short(), "a new reference; looking for holders of a parked download");
            done.insert(hash, now);
        }
    }

    /// Was `hash` refused for its size before (kv `app/refused/<hash>`)?
    pub(crate) fn is_refused(&self, hash: &BlobHash) -> bool { matches!(self.storage.get_kv(&refused_key(hash)), Ok(Some(_))) }

    /// A `FileEvent::Transfer`: emit `transfer-updated`, persist state changes, remember a
    /// size refusal.
    pub(crate) fn on_transfer(&self, info: &TransferInfo) {
        let changed = {
            let mut st = self.transfer_states.lock();
            if info.state == FilesState::Complete {
                st.remove(&info.hash) != Some(FilesState::Complete)
            } else {
                st.insert(info.hash, info.state) != Some(info.state)
            }
        };
        let att = self.att_info(&info.hash);
        if changed && info.kind == BlobKind::Attachment {
            if info.state == FilesState::TooLarge {
                tracing::warn!(hash = %info.hash.short(), total = ?info.total, "an attachment is larger than 25 MB; not downloading it");
                let size = info.total.unwrap_or(0).to_string();
                if let Err(e) = self.storage.set_kv(&refused_key(&info.hash), size.as_bytes()) {
                    tracing::warn!(error = %e, "saving a size refusal failed");
                }
            }
            let state = map_state(info.state).as_str().to_string();
            match self.storage.get_download(&info.hash) {
                Ok(Some(mut rec)) if rec.state != state => {
                    rec.state = state;
                    if let Err(e) = self.storage.upsert_download(&rec) {
                        tracing::warn!(error = %e, "updating a download record failed");
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "reading a download record failed"),
            }
        }
        let t = dto::Transfer {
            hash: info.hash.to_hex(),
            state: map_state(info.state),
            have: info.have,
            total: info.total.or(att.as_ref().and_then(|a| a.primary()).map(|a| a.size)).unwrap_or(0),
            peers: self.online_holders(&info.hash, att.as_ref()),
        };
        self.events.emit(Ev::TransferUpdated, t);
    }

    /// `Transfer` of `hash` right now (`idle` when there is no request and it is not held).
    pub(crate) fn transfer_dto(&self, hash: &BlobHash) -> dto::Transfer {
        let att = self.att_info(hash);
        let peers = self.online_holders(hash, att.as_ref());
        let size = att.as_ref().and_then(|a| a.primary()).map(|a| a.size).unwrap_or(0);
        match self.files.transfer(*hash) {
            Some(info) => dto::Transfer {
                hash: hash.to_hex(),
                state: map_state(info.state),
                have: info.have,
                total: info.total.unwrap_or(size),
                peers,
            },
            None => dto::Transfer { hash: hash.to_hex(), state: TransferState::Idle, have: 0, total: size, peers },
        }
    }

    /// `transfer` command.
    pub(crate) fn cmd_transfer(&self, hash: BlobHash) -> dto::Transfer {
        let t = self.transfer_dto(&hash);
        let unknown_holders = !self.holders.lock().contains_key(&hash);
        if unknown_holders && t.state != TransferState::Complete && self.att_info(&hash).is_some_and(|i| !i.refs.is_empty()) {
            let _ = self.fetch_tx.try_send(FetchReq::Peers { hash });
        }
        t
    }

    /// `download` command.
    pub(crate) async fn cmd_download(&self, hash: BlobHash) -> Result<dto::Transfer, String> {
        if self.files.is_complete(&hash) {
            return Ok(self.transfer_dto(&hash));
        }
        if self.is_refused(&hash) {
            return Err(TOO_LARGE.into());
        }
        let info =
            self.att_info(&hash).filter(|i| !i.refs.is_empty()).ok_or_else(|| "This file isn't available on this device.".to_string())?;
        if info.declared_size().is_some_and(|s| s > MAX_ATTACHMENT_BYTES) {
            return Err(TOO_LARGE.into());
        }
        match self.files.transfer(hash).map(|i| i.state) {
            Some(FilesState::Queued | FilesState::Downloading) => {}
            Some(FilesState::TooLarge) => return Err(TOO_LARGE.into()),
            _ => self.start_download(hash, &info, BlobKind::Attachment).await,
        }
        Ok(self.transfer_dto(&hash))
    }

    /// `file_url` command.
    pub(crate) fn cmd_file_url(&self, hash: BlobHash) -> Option<String> {
        self.files.is_complete(&hash).then(|| format!("{}{}", self.cfg.file_url_base, hash.to_hex()))
    }

    /// `App::file_for_url`.
    pub(crate) async fn file_for_url(&self, hash_hex: &str) -> Option<(Vec<u8>, String)> {
        let hash = BlobHash::from_hex(hash_hex)?;
        if !self.files.is_complete(&hash) {
            return None;
        }
        let bytes = match self.files.read_to_bytes(hash, MAX_ATTACHMENT_BYTES).await {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(hash = %hash.short(), error = %e, "blob not served");
                return None;
            }
        };
        // The type of the name it will be saved under, never the peer's declared type.
        let hint = self.att_ref(&hash).map(|a| shell::mime_for_name(&shell::safe_file_name(&a.name)));
        let head = &bytes[..bytes.len().min(16)];
        let mime = shell::serve_mime(hint, head);
        // Takes over the store's buffer when it is the only owner, instead of copying it.
        Some((Vec::from(bytes), mime))
    }

    /// `open_file` command: export to the download folder (once per blob), mark it as
    /// downloaded from the Internet, then open it if it is an inert document or media type
    /// and reveal it in Explorer otherwise (see `shell::open_action`).
    pub(crate) async fn cmd_open_file(&self, hash: BlobHash, name: &str) -> Result<(), String> {
        if !self.files.is_complete(&hash) {
            return Err("Download the file before opening it.".into());
        }
        let path = match self.previous_export(&hash).await {
            Some(p) => p,
            None => {
                let dir = PathBuf::from(self.settings.read().download_dir.clone());
                tokio::fs::create_dir_all(&dir).await.map_err(|e| {
                    tracing::warn!(dir = %dir.display(), error = %e, "creating the download folder failed");
                    "Couldn't create the download folder. Pick another one in Settings.".to_string()
                })?;
                let path = shell::unique_path(&dir, &shell::safe_file_name(name));
                self.files.export(hash, &path).await.map_err(|e| {
                    tracing::warn!(path = %path.display(), error = %e, "export failed");
                    "Couldn't save the file to the download folder. Check the folder in Settings.".to_string()
                })?;
                if let Err(e) = self.storage.set_kv(&export_key(&hash), path.to_string_lossy().as_bytes()) {
                    tracing::warn!(error = %e, "saving the export path failed");
                }
                path
            }
        };
        // Written on every open: an earlier export may predate it, or a copy may have lost it.
        let marked = match shell::mark_of_the_web(&path).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "writing the Mark of the Web failed; revealing instead of opening");
                false
            }
        };
        let head = read_head(&path).await;
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let action = shell::open_action(&file_name, &head, marked);
        tracing::debug!(path = %path.display(), ?action, "open_file");
        if self.cfg.test.no_devices {
            // Headless and test runs never launch other programs.
            return Ok(());
        }
        match action {
            shell::OpenAction::Open => shell::open_with_default_app(&path).map_err(|e| {
                tracing::warn!(path = %path.display(), error = %e, "opening the file failed");
                "Couldn't open the file. You can find it in your download folder.".to_string()
            }),
            shell::OpenAction::Reveal => shell::reveal_in_folder(&path).map_err(|e| {
                tracing::warn!(path = %path.display(), error = %e, "showing the file failed");
                "Couldn't show the file. You can find it in your download folder.".to_string()
            }),
        }
    }

    /// A previous export of `hash` that still exists with the right size.
    async fn previous_export(&self, hash: &BlobHash) -> Option<PathBuf> {
        let raw = self.storage.get_kv(&export_key(hash)).ok().flatten()?;
        let path = PathBuf::from(String::from_utf8(raw).ok()?);
        let size = self.files.status(*hash).await.ok()?.total?;
        let meta = tokio::fs::metadata(&path).await.ok()?;
        (meta.is_file() && meta.len() == size).then_some(path)
    }

    /// Import the files of an outgoing message into `conv` (a DM when `dm`). Each one is
    /// indexed with me as the author and shared with `conv`. Errors are user-facing.
    pub(crate) async fn import_attachments(&self, conv: ConvId, dm: bool, paths: &[String]) -> Result<Vec<Attachment>, String> {
        if paths.len() > tincan_core::state::MAX_ATTACHMENTS {
            return Err(format!("You can attach up to {} files to one message.", tincan_core::state::MAX_ATTACHMENTS));
        }
        let mut out = Vec::with_capacity(paths.len());
        for raw in paths {
            let path = Path::new(raw);
            let name: String = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| raw.clone())
                .chars()
                .take(tincan_core::state::MAX_ATTACHMENT_NAME_CHARS)
                .collect();
            let unreadable = || format!("Couldn't read {name}. Check that the file still exists.");
            let meta = tokio::fs::metadata(path).await.map_err(|_| unreadable())?;
            if meta.is_dir() {
                return Err(format!("{name} is a folder. Pick a file instead."));
            }
            if meta.len() > MAX_ATTACHMENT_BYTES {
                return Err(format!("{name} is larger than 25 MB. Send a smaller file."));
            }
            let imported = self.files.import(path, BlobKind::Attachment).await.map_err(|e| {
                tracing::warn!(path = %path.display(), error = %e, "import failed");
                unreadable()
            })?;
            let a = Attachment { hash: imported.hash, size: imported.size, name: name.clone(), mime: shell::mime_for_name(&name).to_string() };
            let r = AttRef { conv, dm, author: self.me, name: a.name.clone(), size: a.size, mime: a.mime.clone() };
            self.update_info(a.hash, |i| {
                let added = i.add_ref(r);
                i.share(conv) || added
            });
            out.push(a);
        }
        Ok(out)
    }
}

impl Core {
    /// Backs `App::test_fetch_from`.
    pub(crate) async fn test_fetch_from(&self, hash_hex: &str, user_id: &str, addrs: &[std::net::SocketAddr], secs: u64) -> bool {
        let (Ok(hash), Ok(user)) = (parse_hash(hash_hex), crate::commands::parse_user(user_id)) else { return false };
        if self.files.is_complete(&hash) {
            return true;
        }
        let mut rx = self.files.subscribe();
        self.files.download(hash, BlobKind::Attachment, vec![(user, AddrHint { relay: None, direct: addrs.to_vec() })]);
        let deadline = Instant::now() + std::time::Duration::from_secs(secs);
        let got = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(tincan_files::FileEvent::Transfer(t))) if t.hash == hash => match t.state {
                    FilesState::Complete => break true,
                    FilesState::Unavailable | FilesState::Failed | FilesState::TooLarge => break false,
                    FilesState::Queued | FilesState::Downloading => {}
                },
                Ok(Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => break false,
            }
        };
        if !got {
            self.files.cancel(hash);
        }
        got && self.files.is_complete(&hash)
    }

    /// Backs `App::test_post_attachment_ref`.
    pub(crate) async fn test_post_attachment_ref(&self, conv_id: &str, channel_id: Option<&str>, hash_hex: &str, name: &str, size: u64) -> Result<(), String> {
        let conv = crate::commands::parse_conv(conv_id)?;
        let view = self.view(&conv).ok_or_else(|| "unknown conversation".to_string())?;
        let channel = if view.is_dm {
            tincan_core::state::dm_channel(&conv)
        } else {
            crate::commands::parse_channel(channel_id.ok_or_else(|| "a channel is needed".to_string())?)?
        };
        let a = Attachment { hash: parse_hash(hash_hex)?, size, name: name.to_string(), mime: shell::mime_for_name(name).to_string() };
        self.net.publish(conv, Body::Message { channel, text: String::new(), attachments: vec![a] }).await.map_err(crate::app::net_err)?;
        Ok(())
    }
}

/// The first bytes of a file (for the image check in `open_action`).
async fn read_head(path: &Path) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut head = Vec::with_capacity(16);
    if let Ok(f) = tokio::fs::File::open(path).await {
        let _ = f.take(16).read_to_end(&mut head).await;
    }
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_sizes_over_the_limit_are_never_auto_fetched() {
        assert!(auto_fetch_ok(1000, MAX_ATTACHMENT_BYTES));
        assert!(auto_fetch_ok(MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_BYTES));
        assert!(!auto_fetch_ok(MAX_ATTACHMENT_BYTES + 1, MAX_ATTACHMENT_BYTES));
        assert!(!auto_fetch_ok(MAX_ATTACHMENT_BYTES + 1, u64::MAX), "the setting cannot lift the cap");
        assert!(!auto_fetch_ok(2000, 1000));
        assert!(!auto_fetch_ok(0, 0), "0 turns auto-fetch off");
    }

    #[test]
    fn parked_downloads_are_requeried_at_most_every_ten_seconds() {
        let t0 = Instant::now();
        assert!(requery_due(None, t0, REQUERY_EVERY), "never asked: due");
        assert!(!requery_due(Some(t0), t0 + Duration::from_secs(1), REQUERY_EVERY), "a roster burst queues it once");
        assert!(!requery_due(Some(t0), t0 + REQUERY_EVERY - Duration::from_millis(1), REQUERY_EVERY));
        assert!(requery_due(Some(t0), t0 + REQUERY_EVERY, REQUERY_EVERY));
        assert!(!requery_due(Some(t0 + Duration::from_secs(5)), t0, REQUERY_EVERY), "a clock step back is not due");
    }

    #[test]
    fn only_online_peers_wake_a_parked_download() {
        let (on, off) = (UserId::from_bytes([1; 32]), UserId::from_bytes([2; 32]));
        let providers = vec![(off, AddrHint::default()), (on, AddrHint::default())];
        let left = online_only(providers, |u| *u == on);
        assert_eq!(left.iter().map(|(u, _)| *u).collect::<Vec<_>>(), vec![on]);
        assert!(online_only(vec![(off, AddrHint::default())], |_| false).is_empty(), "nobody online: nothing to wake");
    }

    #[test]
    fn a_refusal_shows_as_failed() {
        assert_eq!(map_state(FilesState::TooLarge), TransferState::Failed);
    }

    #[test]
    fn the_auto_queue_serves_the_newest_and_drops_the_oldest() {
        let q = AutoQueue::new(3);
        for i in 0..5u8 {
            q.push(BlobHash([i; 32]));
        }
        q.push(BlobHash([3; 32]));
        assert_eq!(q.pop(), Some(BlobHash([3; 32])), "a re-pushed hash moves to the front");
        assert_eq!(q.pop(), Some(BlobHash([4; 32])));
        assert_eq!(q.pop(), Some(BlobHash([2; 32])));
        assert_eq!(q.pop(), None, "the two oldest were dropped");
    }
}
