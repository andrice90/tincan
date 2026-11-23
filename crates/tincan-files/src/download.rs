//! Download manager: one task per requested hash.
//!
//! A task walks the request's providers one at a time (each asked only for the ranges
//! still missing), finishes when the blob is complete, and parks on a [`Notify`] when every
//! provider has been tried. [`add_providers`] marks holders as worth trying again and wakes
//! it. Cancellation aborts the task; whatever was written stays in the store.
//!
//! Size caps act only on a proven size. A provider's size header is a claim: BLAKE3 commits
//! to the size through the tree shape, so a provider can claim any size in the same
//! power-of-two bucket and still have the first chunks verify. The size is proven once the
//! last chunk verified (a partial blob that has it, the end of a download, or a last-chunk
//! request). When a provider claims more than the largest cap, it is asked for the last
//! chunk first: proven over the cap, kinds whose [`BlobKind::max_size`] it exceeds are
//! dropped and their tags deleted, and with no kind left the request turns
//! [`TransferState::TooLarge`] and ends (for good: the hash commits to the size). A provider
//! that cannot prove its claim fails only its own attempt. Verified bytes past the largest
//! cap end the request too.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use iroh::EndpointId;
use iroh_blobs::{
    api::remote::GetProgressItem,
    get::GetError,
    protocol::{ChunkRanges, ChunkRangesExt, GetRequest},
};
use n0_future::StreamExt;
use tincan_core::{BlobHash, UserId, wire::AddrHint};
use tokio::{sync::Notify, task::JoinHandle};
use tracing::{debug, info, warn};

use crate::{
    BlobKind, FileEvent, TransferInfo, TransferState,
    addr::{blobs_hash, endpoint_addr, endpoint_id, user_id},
    files::{Inner, Kinds, complete_info, mark_held, set_tag},
};

/// Minimum spacing of progress events per download.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// One download request.
pub(crate) struct Download {
    /// Distinguishes this request from a later one for the same hash.
    id: u64,
    /// Kind of the first request; reported in events.
    kind: BlobKind,
    /// Every kind requested; all are tagged on completion.
    pub(crate) kinds: Kinds,
    state: TransferState,
    have: u64,
    total: Option<u64>,
    /// Providers in the order they were first named.
    providers: Vec<Provider>,
    /// Provider being fetched from right now.
    current: Option<EndpointId>,
    /// The provider that last delivered verified bytes.
    last_from: Option<EndpointId>,
    wake: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

struct Provider {
    id: EndpointId,
    /// Not yet tried in this round (or named again since).
    pending: bool,
}

/// What is known of a blob's size, measured against the smallest cap of a request.
#[derive(Debug, PartialEq, Eq)]
enum SizeVerdict {
    /// Nothing proven over the cap: keep going.
    Ok,
    /// Only the provider's unproven claim is over the cap: ask it for the last chunk.
    NeedProof,
    /// Proven over the cap (the last chunk verified, or verified bytes past it): apply the
    /// caps to this size ([`Download::admit`] drops kinds or refuses).
    Apply(u64),
}

/// `claimed`: a provider's size header (unproven until the last chunk verifies);
/// `validated`: the size proven by the last chunk; `have`: verified bytes held; `cap`: the
/// smallest cap of the request's kinds. Only a proven size or verified bytes are applied to
/// the caps (and can drop kinds or refuse, for good); a claim alone asks for proof.
fn size_verdict(claimed: Option<u64>, validated: Option<u64>, have: u64, cap: u64) -> SizeVerdict {
    if let Some(v) = validated.filter(|v| *v > cap) {
        return SizeVerdict::Apply(v);
    }
    if have > cap {
        return SizeVerdict::Apply(have);
    }
    if validated.is_none() && claimed.is_some_and(|c| c > cap) {
        return SizeVerdict::NeedProof;
    }
    SizeVerdict::Ok
}

/// What the size cap says about a request once its size is proven.
#[derive(Debug, PartialEq, Eq)]
enum Admit {
    /// Every requested kind allows it.
    Ok,
    /// These kinds were dropped from the request; at least one kind is left.
    Dropped(Kinds),
    /// No requested kind allows it: the request is now `TooLarge`. These were its kinds.
    Refused(Kinds),
}

impl Download {
    fn new(id: u64, kind: BlobKind, wake: Arc<Notify>) -> Self {
        Download {
            id,
            kind,
            kinds: Kinds::of(kind),
            state: TransferState::Queued,
            have: 0,
            total: None,
            providers: Vec::new(),
            current: None,
            last_from: None,
            wake,
            task: None,
        }
    }

    pub(crate) fn info(&self, hash: BlobHash) -> TransferInfo {
        let pending = self.providers.iter().filter(|p| p.pending).count();
        let peers = pending + usize::from(self.current.is_some());
        TransferInfo {
            hash,
            kind: self.kind,
            state: self.state,
            have: self.have,
            total: self.total,
            peers: u32::try_from(peers).unwrap_or(u32::MAX),
        }
    }

    /// Aborts the task (it stops at its next await point).
    pub(crate) fn abort(&self) {
        if let Some(t) = &self.task {
            t.abort();
        }
    }

    pub(crate) fn take_task(&mut self) -> Option<JoinHandle<()>> {
        self.task.take()
    }

    /// The largest size any requested kind accepts.
    fn cap(&self) -> u64 {
        self.kinds.iter().map(BlobKind::max_size).max().unwrap_or(0)
    }

    /// The smallest size every requested kind accepts.
    fn min_cap(&self) -> u64 {
        self.kinds.iter().map(BlobKind::max_size).min().unwrap_or(0)
    }

    /// Applies the size caps for a blob of `size` bytes (a proven size: see
    /// [`size_verdict`]).
    fn admit(&mut self, size: u64) -> Admit {
        let mut over = Kinds::default();
        for k in self.kinds.iter().filter(|k| size > k.max_size()) {
            over.insert(k);
        }
        if over.is_empty() {
            return Admit::Ok;
        }
        if over == self.kinds {
            self.state = TransferState::TooLarge;
            self.total = Some(size);
            self.current = None;
            self.task = None;
            for p in &mut self.providers {
                p.pending = false;
            }
            return Admit::Refused(over);
        }
        for k in over.iter() {
            self.kinds.remove(k);
        }
        if !self.kinds.has(self.kind)
            && let Some(k) = self.kinds.iter().next()
        {
            self.kind = k;
        }
        Admit::Dropped(over)
    }

    /// Merges providers; returns true if any became pending.
    fn merge(&mut self, ids: &[EndpointId]) -> bool {
        let mut woke = false;
        for id in ids {
            match self.providers.iter_mut().find(|p| p.id == *id) {
                Some(p) => {
                    woke |= !p.pending;
                    p.pending = true;
                }
                None => {
                    self.providers.push(Provider { id: *id, pending: true });
                    woke = true;
                }
            }
        }
        woke
    }

    fn has_pending(&self) -> bool {
        self.providers.iter().any(|p| p.pending)
    }

    /// Takes the first pending provider and makes it current.
    fn next_provider(&mut self) -> Option<EndpointId> {
        let p = self.providers.iter_mut().find(|p| p.pending)?;
        p.pending = false;
        self.current = Some(p.id);
        Some(p.id)
    }

    /// Starts a fresh task for this request (after a local failure or a refusal).
    fn restart(&mut self, inner: &Arc<Inner>, hash: BlobHash) {
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.id = id;
        self.state = TransferState::Queued;
        self.wake = Arc::new(Notify::new());
        self.task = Some(inner.rt.spawn(run(inner.clone(), hash, id, self.wake.clone())));
    }
}

/// Resolves providers to endpoint ids, feeding their address hints to the lookup. Skips
/// invalid keys and ourselves.
fn resolve(inner: &Inner, providers: Vec<(UserId, AddrHint)>) -> Vec<EndpointId> {
    let me = inner.endpoint.id();
    let mut out = Vec::with_capacity(providers.len());
    for (user, hint) in providers {
        let Some(id) = endpoint_id(&user) else {
            warn!(user = %user.short(), "ignoring provider with an invalid key");
            continue;
        };
        if id == me {
            continue;
        }
        if let Some(addr) = endpoint_addr(id, &hint) {
            inner.lookup.add_endpoint_info(addr);
        }
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// A `TooLarge` snapshot for a request that never started.
fn too_large_info(hash: BlobHash, kind: BlobKind, size: u64) -> TransferInfo {
    TransferInfo { hash, kind, state: TransferState::TooLarge, have: 0, total: Some(size), peers: 0 }
}

/// Implements [`crate::Files::download`].
pub(crate) fn request(inner: &Arc<Inner>, hash: BlobHash, kind: BlobKind, providers: Vec<(UserId, AddrHint)>) {
    let ids = resolve(inner, providers);
    let mut st = inner.state.lock();
    if let Some(h) = st.held.get(&hash).copied() {
        if h.kinds.has(kind) {
            inner.emit(FileEvent::Transfer(complete_info(hash, kind, h.size)));
            return;
        }
        if h.size > kind.max_size() {
            // Held as another kind, and too large for this one.
            inner.emit(FileEvent::Transfer(too_large_info(hash, kind, h.size)));
            return;
        }
    }
    if let Some(d) = st.downloads.get_mut(&hash) {
        if d.state == TransferState::TooLarge {
            let size = d.total.unwrap_or(u64::MAX);
            if size > kind.max_size() {
                inner.emit(FileEvent::Transfer(d.info(hash)));
                return;
            }
            // Asked again as a kind whose cap allows it: start over as that kind.
            d.kind = kind;
            d.kinds = Kinds::of(kind);
            d.merge(&ids);
            d.restart(inner, hash);
            inner.emit(FileEvent::Transfer(d.info(hash)));
            return;
        }
        d.kinds.insert(kind);
        d.merge(&ids);
        if d.state == TransferState::Failed {
            d.restart(inner, hash);
            inner.emit(FileEvent::Transfer(d.info(hash)));
        } else {
            d.wake.notify_one();
        }
        return;
    }
    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    let wake = Arc::new(Notify::new());
    let mut d = Download::new(id, kind, wake.clone());
    d.merge(&ids);
    d.task = Some(inner.rt.spawn(run(inner.clone(), hash, id, wake)));
    inner.emit(FileEvent::Transfer(d.info(hash)));
    st.downloads.insert(hash, d);
    debug!(hash = %hash.short(), ?kind, providers = ids.len(), "download queued");
}

/// Implements [`crate::Files::add_providers`].
pub(crate) fn add_providers(inner: &Inner, hash: BlobHash, providers: Vec<(UserId, AddrHint)>) {
    let ids = resolve(inner, providers);
    let mut st = inner.state.lock();
    if let Some(d) = st.downloads.get_mut(&hash)
        && d.state != TransferState::TooLarge
        && d.merge(&ids)
    {
        d.wake.notify_one();
    }
}

/// How one provider attempt ended.
enum Attempt {
    /// The blob is complete locally.
    Complete,
    /// This provider could not (fully) serve it; try the next.
    ProviderFailed,
    /// The blob is larger than the request allows; the request is already `TooLarge`.
    Refused,
    /// The local store failed; stop.
    LocalError(String),
}

/// The download task. Holds no lock across awaits; every state update first checks that
/// the request is still the one this task serves (`id`).
async fn run(inner: Arc<Inner>, hash: BlobHash, id: u64, wake: Arc<Notify>) {
    let bh = blobs_hash(&hash);
    let Some(kind) = with_entry(&inner, hash, id, |d| d.kind) else {
        return;
    };
    // Tag first, so garbage collection keeps the partial data.
    if let Err(e) = set_tag(&inner.store, hash, kind).await {
        fail(&inner, hash, id, &e.to_string());
        return;
    }
    loop {
        match inner.store.remote().local(bh).await {
            Ok(local) if local.is_complete() => {
                finish(&inner, hash, id).await;
                return;
            }
            Ok(local) => {
                let have = local.local_bytes();
                with_entry(&inner, hash, id, |d| d.have = have);
            }
            Err(e) => {
                fail(&inner, hash, id, &e.to_string());
                return;
            }
        }
        // A partial blob already knows its size: check it before asking anyone.
        if let Some(size) = known_size(&inner, &hash).await
            && !check_size(&inner, hash, id, size).await
        {
            return;
        }
        let Some(has_pending) = with_entry(&inner, hash, id, |d| d.has_pending()) else {
            return;
        };
        if !has_pending {
            set_state(&inner, hash, id, TransferState::Unavailable);
            wake.notified().await;
            continue;
        }
        set_state(&inner, hash, id, TransferState::Queued);
        let Ok(_permit) = inner.slots.clone().acquire_owned().await else {
            return;
        };
        loop {
            let next = with_entry(&inner, hash, id, |d| {
                let p = d.next_provider();
                if p.is_some() {
                    d.state = TransferState::Downloading;
                }
                (p, d.info(hash))
            });
            let Some((provider, info)) = next else {
                return;
            };
            let Some(provider) = provider else {
                break;
            };
            inner.emit(FileEvent::Transfer(info));
            let outcome = attempt(&inner, hash, id, provider).await;
            with_entry(&inner, hash, id, |d| d.current = None);
            match outcome {
                Attempt::Complete => {
                    finish(&inner, hash, id).await;
                    return;
                }
                Attempt::ProviderFailed => {
                    debug!(hash = %hash.short(), provider = %provider.fmt_short(), "provider failed");
                }
                Attempt::Refused => return,
                Attempt::LocalError(e) => {
                    fail(&inner, hash, id, &e);
                    return;
                }
            }
        }
        // Round over without success: back to the top, which parks if nobody new came.
    }
}

/// Fetches whatever is still missing from one provider.
async fn attempt(inner: &Inner, hash: BlobHash, id: u64, provider: EndpointId) -> Attempt {
    let bh = blobs_hash(&hash);
    let conn = match inner.pool.get_or_connect(provider).await {
        Ok(c) => c,
        Err(e) => {
            debug!(hash = %hash.short(), provider = %provider.fmt_short(), "connect failed: {e}");
            return Attempt::ProviderFailed;
        }
    };
    let local = match inner.store.remote().local(bh).await {
        Ok(l) => l,
        Err(e) => return Attempt::LocalError(e.to_string()),
    };
    if local.is_complete() {
        return Attempt::Complete;
    }
    let base = local.local_bytes();
    let connection: iroh::endpoint::Connection = (*conn).clone();
    let mut stream = Box::pin(inner.store.remote().execute_get(connection.clone(), local.missing()).stream());
    let mut last_emit: Option<Instant> = None;
    let Some(mut cap) = with_entry(inner, hash, id, |d| d.cap()) else {
        return Attempt::Refused;
    };
    let mut size_known = false;
    while let Some(item) = stream.next().await {
        match item {
            GetProgressItem::Progress(n) => {
                let have = base + n;
                if !size_known && n > 0 {
                    // The first verified chunk carries the provider's size claim.
                    if let Ok(bf) = inner.store.blobs().observe(bh).await
                        && bf.size() > 0
                    {
                        size_known = true;
                        let validated = bf.validated_size();
                        // Shown while downloading; the caps act only on a proven size.
                        with_entry(inner, hash, id, |d| d.total = Some(validated.unwrap_or(bf.size())));
                        let min_cap = with_entry(inner, hash, id, |d| d.min_cap()).unwrap_or(0);
                        match size_verdict(Some(bf.size()), validated, have, min_cap) {
                            SizeVerdict::NeedProof => {
                                drop(stream);
                                return prove_size(inner, hash, id, provider, connection).await;
                            }
                            SizeVerdict::Apply(size) => {
                                if !check_size(inner, hash, id, size).await {
                                    return Attempt::Refused;
                                }
                                cap = with_entry(inner, hash, id, |d| d.cap()).unwrap_or(0);
                            }
                            SizeVerdict::Ok => {}
                        }
                    }
                }
                // Verified bytes past the cap end it whatever the claim.
                if have > cap && !check_size(inner, hash, id, have).await {
                    return Attempt::Refused;
                }
                let due = last_emit.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL);
                let info = with_entry(inner, hash, id, |d| {
                    d.have = have;
                    if n > 0 {
                        d.last_from = Some(provider);
                    }
                    due.then(|| d.info(hash))
                });
                if let Some(Some(info)) = info {
                    inner.emit(FileEvent::Transfer(info));
                    last_emit = Some(Instant::now());
                }
            }
            GetProgressItem::Done(_) => {
                drop(conn);
                return match inner.store.remote().local(bh).await {
                    Ok(l) if l.is_complete() => Attempt::Complete,
                    // Served everything it had, which was not everything.
                    Ok(_) => Attempt::ProviderFailed,
                    Err(e) => Attempt::LocalError(e.to_string()),
                };
            }
            GetProgressItem::Error(e) => {
                return if is_local(&e) { Attempt::LocalError(e.to_string()) } else { Attempt::ProviderFailed };
            }
        }
    }
    Attempt::ProviderFailed
}

fn is_local(e: &GetError) -> bool {
    matches!(e, GetError::LocalFailure { .. }) || e.local_write().is_some()
}

/// The size the store has proven for `hash` (its last chunk verified), if any. A partial
/// blob's other size is only what a provider claimed.
async fn known_size(inner: &Inner, hash: &BlobHash) -> Option<u64> {
    let bf = inner.store.blobs().observe(blobs_hash(hash)).await.ok()?;
    bf.validated_size().filter(|s| *s > 0)
}

/// The size claim was over the cap: ask `provider` for the last chunk, which proves the
/// real size (the hash commits to it). Proven over the cap, the request is refused for
/// good. A provider that cannot prove the claim fails this attempt only. Proven under the
/// cap (the claim came from someone else's partial, or was a lie), the provider is tried
/// again at once: the size is settled now.
async fn prove_size(inner: &Inner, hash: BlobHash, id: u64, provider: EndpointId, conn: iroh::endpoint::Connection) -> Attempt {
    let bh = blobs_hash(&hash);
    let request = GetRequest::blob_ranges(bh, ChunkRanges::last_chunk());
    let mut stream = Box::pin(inner.store.remote().execute_get(conn, request).stream());
    while let Some(item) = stream.next().await {
        match item {
            GetProgressItem::Progress(_) => {}
            GetProgressItem::Done(_) => break,
            GetProgressItem::Error(e) => {
                debug!(hash = %hash.short(), "size proof failed: {e}");
                return if is_local(&e) { Attempt::LocalError(e.to_string()) } else { Attempt::ProviderFailed };
            }
        }
    }
    let validated = match inner.store.blobs().observe(bh).await {
        Ok(bf) => bf.validated_size(),
        Err(e) => return Attempt::LocalError(e.to_string()),
    };
    let Some(size) = validated else {
        debug!(hash = %hash.short(), "the provider did not prove its size claim");
        return Attempt::ProviderFailed;
    };
    with_entry(inner, hash, id, |d| d.total = Some(size));
    if !check_size(inner, hash, id, size).await {
        return Attempt::Refused;
    }
    debug!(hash = %hash.short(), size, "size proven under the cap; fetching the rest");
    with_entry(inner, hash, id, |d| d.merge(&[provider]));
    Attempt::ProviderFailed
}

/// Applies the size caps once `size` is known. False when the request was refused: the
/// task must end (its state is already `TooLarge` and reported).
async fn check_size(inner: &Inner, hash: BlobHash, id: u64, size: u64) -> bool {
    let Some((verdict, info)) = with_entry(inner, hash, id, |d| (d.admit(size), d.info(hash))) else {
        return false;
    };
    match verdict {
        Admit::Ok => true,
        Admit::Dropped(kinds) => {
            debug!(hash = %hash.short(), size, ?kinds, "kinds dropped: the blob is larger than their limit");
            drop_tags(inner, hash, kinds).await;
            // The task tagged only the kind it started with. If that kind was dropped, tag
            // the one it continues as, or garbage collection could take the partial data.
            if let Some(kind) = with_entry(inner, hash, id, |d| d.kind)
                && let Err(e) = set_tag(&inner.store, hash, kind).await
            {
                warn!(hash = %hash.short(), "tagging a download after a size check: {e}");
            }
            true
        }
        Admit::Refused(kinds) => {
            warn!(hash = %hash.short(), size, "download refused: the blob is larger than its size limit");
            drop_tags(inner, hash, kinds).await;
            inner.emit(FileEvent::Transfer(info));
            false
        }
    }
}

/// Deletes the tags of `kinds` for `hash`, except kinds it is held as, so garbage collection
/// frees refused partial data and `partial_downloads` never lists it.
async fn drop_tags(inner: &Inner, hash: BlobHash, kinds: Kinds) {
    let held = inner.state.lock().held.get(&hash).map(|h| h.kinds).unwrap_or_default();
    for k in kinds.iter().filter(|k| !held.has(*k)) {
        if let Err(e) = inner.store.tags().delete(k.tag_name(&hash)).await {
            warn!(hash = %hash.short(), "dropping the tag of a refused download: {e}");
        }
    }
}

/// Runs `f` on the request if it is still the one with `id`.
fn with_entry<R>(inner: &Inner, hash: BlobHash, id: u64, f: impl FnOnce(&mut crate::download::Download) -> R) -> Option<R> {
    let mut st = inner.state.lock();
    match st.downloads.get_mut(&hash) {
        Some(d) if d.id == id => Some(f(d)),
        _ => None,
    }
}

/// Sets the state and emits it if it changed.
fn set_state(inner: &Inner, hash: BlobHash, id: u64, state: TransferState) {
    let info = with_entry(inner, hash, id, |d| {
        if d.state == state {
            return None;
        }
        d.state = state;
        if state == TransferState::Unavailable {
            d.current = None;
        }
        Some(d.info(hash))
    });
    if let Some(Some(info)) = info {
        if state == TransferState::Unavailable {
            info!(hash = %hash.short(), have = info.have, "no provider could serve the blob; waiting for holders");
        }
        inner.emit(FileEvent::Transfer(info));
    }
}

/// Marks the request failed (it stays listed so the UI can show it) and ends the task.
fn fail(inner: &Inner, hash: BlobHash, id: u64, reason: &str) {
    warn!(hash = %hash.short(), "download failed: {reason}");
    let info = with_entry(inner, hash, id, |d| {
        d.state = TransferState::Failed;
        d.current = None;
        d.task = None;
        d.info(hash)
    });
    if let Some(info) = info {
        inner.emit(FileEvent::Transfer(info));
    }
}

/// Checks the final size, tags every requested kind, moves the hash to the held set and
/// reports completion.
async fn finish(inner: &Inner, hash: BlobHash, id: u64) {
    let size = match inner.store.blobs().observe(blobs_hash(&hash)).await {
        Ok(bf) => bf.size(),
        Err(e) => {
            fail(inner, hash, id, &e.to_string());
            return;
        }
    };
    if !check_size(inner, hash, id, size).await {
        return;
    }
    let Some((kinds, from)) = with_entry(inner, hash, id, |d| (d.kinds, d.last_from.map(|p| user_id(&p)))) else {
        return;
    };
    for k in kinds.iter() {
        if let Err(e) = set_tag(&inner.store, hash, k).await {
            fail(inner, hash, id, &e.to_string());
            return;
        }
    }
    let done = {
        let mut st = inner.state.lock();
        match st.downloads.get(&hash) {
            Some(d) if d.id == id => {
                let kind = d.kind;
                st.downloads.remove(&hash);
                Some((kind, mark_held(&mut st, hash, kinds, size, from)))
            }
            // Cancelled or removed while finishing: leave the held set alone.
            _ => None,
        }
    };
    if let Some((kind, added)) = done {
        info!(hash = %hash.short(), size, from = ?from.map(|u| u.short()), "download complete");
        inner.emit(FileEvent::Transfer(complete_info(hash, kind, size)));
        for k in added {
            inner.emit(FileEvent::Completed { hash, kind: k, from });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both() -> Download {
        let mut d = Download::new(1, BlobKind::Clip, Arc::new(Notify::new()));
        d.kinds.insert(BlobKind::Attachment);
        d
    }

    #[test]
    fn caps_drop_kinds_then_refuse() {
        let mut d = both();
        assert_eq!(d.cap(), crate::MAX_ATTACHMENT_BYTES);
        assert_eq!(d.admit(crate::MAX_CLIP_BYTES), Admit::Ok);
        // Too large for a clip, fine for an attachment: only the clip kind goes.
        assert_eq!(d.admit(crate::MAX_CLIP_BYTES + 1), Admit::Dropped(Kinds::of(BlobKind::Clip)));
        assert_eq!(d.kinds, Kinds::of(BlobKind::Attachment));
        assert_eq!(d.kind, BlobKind::Attachment, "the reported kind follows what is left");
        assert_eq!(d.state, TransferState::Queued);
        // Past every cap: refused.
        assert_eq!(d.admit(crate::MAX_ATTACHMENT_BYTES + 1), Admit::Refused(Kinds::of(BlobKind::Attachment)));
        assert_eq!(d.state, TransferState::TooLarge);
        assert_eq!(d.total, Some(crate::MAX_ATTACHMENT_BYTES + 1));
        assert!(!d.has_pending());
    }

    /// APP-SECURITY-1: a provider's size claim over the cap never refuses or drops a kind (a
    /// refusal is permanent); only a proven size or verified bytes past the cap do.
    #[test]
    fn only_a_proven_size_refuses() {
        let cap = crate::MAX_ATTACHMENT_BYTES;
        for claim in [cap + 1, 30 * 1024 * 1024, u64::MAX] {
            for have in [0, 1024, cap] {
                assert_eq!(size_verdict(Some(claim), None, have, cap), SizeVerdict::NeedProof, "an unproven claim of {claim}");
            }
        }
        assert_eq!(size_verdict(Some(cap), None, 1024, cap), SizeVerdict::Ok);
        assert_eq!(size_verdict(None, None, 0, cap), SizeVerdict::Ok);
        assert_eq!(size_verdict(Some(u64::MAX), Some(cap + 1), 0, cap), SizeVerdict::Apply(cap + 1), "proven over the cap");
        assert_eq!(size_verdict(Some(cap + 5), Some(cap), 0, cap), SizeVerdict::Ok, "a proven size wins over the claim");
        assert_eq!(size_verdict(Some(cap), None, cap + 1, cap), SizeVerdict::Apply(cap + 1), "verified bytes past the cap");
    }

    #[test]
    fn a_single_clip_request_is_refused_past_five_mib() {
        let mut d = Download::new(1, BlobKind::Clip, Arc::new(Notify::new()));
        assert_eq!(d.admit(crate::MAX_CLIP_BYTES + 1), Admit::Refused(Kinds::of(BlobKind::Clip)));
        assert_eq!(d.info(BlobHash::from_bytes([0; 32])).state, TransferState::TooLarge);
    }

    /// An avatar or icon download is refused past 64 KiB, so naming a large blob as an image
    /// cannot make everyone fetch it.
    #[test]
    fn an_image_request_is_refused_past_64_kib() {
        assert_eq!(BlobKind::Image.max_size(), 64 * 1024);
        assert_eq!(BlobKind::Image.tag_name(&BlobHash::from_bytes([0xab; 32])), format!("img-{}", "ab".repeat(32)));
        let mut d = Download::new(1, BlobKind::Image, Arc::new(Notify::new()));
        assert_eq!(d.admit(crate::MAX_IMAGE_BYTES), Admit::Ok);
        assert_eq!(d.admit(crate::MAX_IMAGE_BYTES + 1), Admit::Refused(Kinds::of(BlobKind::Image)));
        assert_eq!(d.state, TransferState::TooLarge);
        // Merged with an attachment request, only the image kind is dropped.
        let mut d = Download::new(1, BlobKind::Image, Arc::new(Notify::new()));
        d.kinds.insert(BlobKind::Attachment);
        assert_eq!(d.admit(crate::MAX_IMAGE_BYTES + 1), Admit::Dropped(Kinds::of(BlobKind::Image)));
        let k = Kinds::of(BlobKind::Image);
        assert!(k.has(BlobKind::Image) && !k.is_empty());
        assert_eq!(k.iter().collect::<Vec<_>>(), vec![BlobKind::Image]);
    }
}
