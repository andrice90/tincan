//! Download manager: one task per requested hash.
//!
//! A task walks the request's providers one at a time (each asked only for the ranges
//! still missing), finishes when the blob is complete, and parks on a [`Notify`] when every
//! provider has been tried. [`add_providers`] marks holders as worth trying again and wakes
//! it. Cancellation aborts the task; whatever was written stays in the store.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use iroh::EndpointId;
use iroh_blobs::{api::remote::GetProgressItem, get::GetError};
use n0_future::StreamExt;
use tincan_core::{BlobHash, UserId, wire::AddrHint};
use tokio::{sync::Notify, task::JoinHandle};
use tracing::{debug, info, warn};

use crate::{
    BlobKind, FileEvent, TransferInfo, TransferState,
    addr::{blobs_hash, endpoint_addr, endpoint_id},
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
    wake: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

struct Provider {
    id: EndpointId,
    /// Not yet tried in this round (or named again since).
    pending: bool,
}

impl Download {
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

/// Implements [`crate::Files::download`].
pub(crate) fn request(inner: &Arc<Inner>, hash: BlobHash, kind: BlobKind, providers: Vec<(UserId, AddrHint)>) {
    let ids = resolve(inner, providers);
    let mut st = inner.state.lock();
    if let Some(h) = st.held.get(&hash).copied()
        && h.kinds.has(kind)
    {
        inner.emit(FileEvent::Transfer(complete_info(hash, kind, h.size)));
        return;
    }
    if let Some(d) = st.downloads.get_mut(&hash) {
        d.kinds.insert(kind);
        d.merge(&ids);
        if d.state == TransferState::Failed {
            let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
            d.id = id;
            d.state = TransferState::Queued;
            d.wake = Arc::new(Notify::new());
            d.task = Some(inner.rt.spawn(run(inner.clone(), hash, id, d.wake.clone())));
            inner.emit(FileEvent::Transfer(d.info(hash)));
        } else {
            d.wake.notify_one();
        }
        return;
    }
    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    let wake = Arc::new(Notify::new());
    let mut d = Download {
        id,
        kind,
        kinds: Kinds::of(kind),
        state: TransferState::Queued,
        have: 0,
        total: None,
        providers: Vec::new(),
        current: None,
        wake: wake.clone(),
        task: None,
    };
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
    let stream = inner.store.remote().execute_get(connection, local.missing()).stream();
    tokio::pin!(stream);
    let mut last_emit: Option<Instant> = None;
    let mut size_known = with_entry(inner, hash, id, |d| d.total.is_some()).unwrap_or(true);
    while let Some(item) = stream.next().await {
        match item {
            GetProgressItem::Progress(n) => {
                let have = base + n;
                if !size_known && n > 0 {
                    // The first verified chunk carries the size.
                    if let Ok(bf) = inner.store.blobs().observe(bh).await
                        && bf.size() > 0
                    {
                        let size = bf.size();
                        with_entry(inner, hash, id, |d| d.total = Some(size));
                        size_known = true;
                    }
                }
                let due = last_emit.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL);
                let info = with_entry(inner, hash, id, |d| {
                    d.have = have;
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

/// Tags every requested kind, moves the hash to the held set and reports completion.
async fn finish(inner: &Inner, hash: BlobHash, id: u64) {
    let Some(kinds) = with_entry(inner, hash, id, |d| d.kinds) else {
        return;
    };
    for k in kinds.iter() {
        if let Err(e) = set_tag(&inner.store, hash, k).await {
            fail(inner, hash, id, &e.to_string());
            return;
        }
    }
    let size = match inner.store.blobs().observe(blobs_hash(&hash)).await {
        Ok(bf) => bf.size(),
        Err(e) => {
            fail(inner, hash, id, &e.to_string());
            return;
        }
    };
    let done = {
        let mut st = inner.state.lock();
        match st.downloads.get(&hash) {
            Some(d) if d.id == id => {
                let kind = d.kind;
                st.downloads.remove(&hash);
                Some((kind, mark_held(&mut st, hash, kinds, size)))
            }
            // Cancelled or removed while finishing: leave the held set alone.
            _ => None,
        }
    };
    if let Some((kind, added)) = done {
        info!(hash = %hash.short(), size, "download complete");
        inner.emit(FileEvent::Transfer(complete_info(hash, kind, size)));
        for k in added {
            inner.emit(FileEvent::Completed { hash, kind: k });
        }
    }
}
