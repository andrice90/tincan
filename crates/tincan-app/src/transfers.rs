//! Attachments: import on send, auto-fetch on arrival, `transfer` / `download` /
//! `open_file` / `file_url`, resume after restart, and the blob URL backing.
//!
//! The hash-only commands need to know where a blob came from. The attachment index in
//! `Core::atts` is filled from every message the app sees; an entry is also persisted
//! (`app/att/<hash>` in kv) when a download is requested or a file is sent, so a partial
//! download can find its conversation again after a restart.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tincan_core::event::{Attachment, Body, StoredEvent};
use tincan_core::storage::Storage;
use tincan_core::wire::AddrHint;
use tincan_core::{BlobHash, ConvId, UserId};
use tincan_files::{BlobKind, TransferInfo, TransferState as FilesState};
use tincan_store::DownloadRecord;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::app::{AttRef, Core};
use crate::dto::{self, TransferState};
use crate::emit::Ev;
use crate::pump::REQUERY_EVERY;
use crate::settings::MAX_ATTACHMENT_BYTES;
use crate::shell;

/// Work for the background fetch worker (one at a time, bounded queue).
#[derive(Debug)]
pub(crate) enum FetchReq {
    /// Auto-fetch an attachment that just arrived.
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

/// Persisted form of an [`AttRef`].
#[derive(Serialize, Deserialize)]
struct AttRecord {
    conv: String,
    author: String,
    name: String,
    size: u64,
    mime: String,
}

fn att_key(hash: &BlobHash) -> String { format!("app/att/{}", hash.to_hex()) }
fn export_key(hash: &BlobHash) -> String { format!("app/export/{}", hash.to_hex()) }

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
        FilesState::Failed => TransferState::Failed,
    }
}

/// The background worker: auto-fetches, holder lookups, parked-download retries and
/// resumes, one at a time so `who_has` traffic stays bounded.
pub(crate) async fn fetch_worker(core: Arc<Core>, mut rx: mpsc::Receiver<FetchReq>) {
    let mut seen: HashSet<BlobHash> = HashSet::new();
    let mut requeried: HashMap<BlobHash, Instant> = HashMap::new();
    while let Some(req) = rx.recv().await {
        if core.is_closed() {
            break;
        }
        match req {
            FetchReq::Auto { hash } => {
                if seen.len() > 10_000 {
                    seen.clear();
                }
                if !seen.insert(hash) || core.files.is_complete(&hash) || core.files.transfer(hash).is_some() {
                    continue;
                }
                if let Some(att) = core.att_ref(&hash) {
                    core.start_download(hash, &att, BlobKind::Attachment).await;
                }
            }
            FetchReq::Peers { hash } => {
                if let Some(att) = core.att_ref(&hash) {
                    let holders = core.net.who_has(&att.conv, hash).await;
                    core.note_holders(hash, &holders);
                    core.events.emit(Ev::TransferUpdated, core.transfer_dto(&hash));
                }
            }
            FetchReq::Requery { hash } => {
                let now = Instant::now();
                requeried.retain(|_, t| now.duration_since(*t) < REQUERY_EVERY);
                if requeried.contains_key(&hash) {
                    continue;
                }
                requeried.insert(hash, now);
                if let Some(att) = core.att_ref(&hash) {
                    let holders = core.net.who_has(&att.conv, hash).await;
                    core.note_holders(hash, &holders);
                    let providers = core.providers(&att, holders);
                    if !providers.is_empty() {
                        core.files.add_providers(hash, providers);
                    }
                }
            }
            FetchReq::Resume { hash, kind } => {
                if core.files.is_complete(&hash) || core.files.transfer(hash).is_some() {
                    continue;
                }
                match core.att_ref(&hash) {
                    Some(att) => core.start_download(hash, &att, kind).await,
                    None => {
                        // Origin unknown (a clip, or an index entry lost): ask every server.
                        let mut providers: Vec<(UserId, AddrHint)> = Vec::new();
                        for conv in core.net.convs() {
                            for (u, a) in core.net.who_has(&conv, hash).await {
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
    /// Where `hash` came from: the in-memory index, then the persisted record.
    pub(crate) fn att_ref(&self, hash: &BlobHash) -> Option<AttRef> {
        if let Some(a) = self.atts.lock().get(hash) {
            return Some(a.clone());
        }
        let raw = self.storage.get_kv(&att_key(hash)).ok().flatten()?;
        let rec: AttRecord = serde_json::from_slice(&raw).ok()?;
        let a = AttRef {
            conv: ConvId::from_hex(&rec.conv)?,
            author: UserId::from_hex(&rec.author)?,
            name: rec.name,
            size: rec.size,
            mime: rec.mime,
        };
        self.atts.lock().insert(*hash, a.clone());
        Some(a)
    }

    /// Persist the index entry of `hash`.
    fn persist_att(&self, hash: &BlobHash, a: &AttRef) {
        let rec = AttRecord { conv: a.conv.to_hex(), author: a.author.to_hex(), name: a.name.clone(), size: a.size, mime: a.mime.clone() };
        if let Ok(json) = serde_json::to_vec(&rec)
            && let Err(e) = self.storage.set_kv(&att_key(hash), &json)
        {
            tracing::warn!(error = %e, "saving an attachment record failed");
        }
    }

    /// Remember the holders from a `who_has` answer.
    pub(crate) fn note_holders(&self, hash: BlobHash, holders: &[(UserId, AddrHint)]) {
        let mut h = self.holders.lock();
        if h.len() > 10_000 {
            h.clear();
        }
        h.insert(hash, holders.iter().map(|(u, _)| *u).collect());
    }

    /// Online holders of `hash` (the author counts when online), never me.
    fn online_holders(&self, hash: &BlobHash, att: Option<&AttRef>) -> u32 {
        let mut users: Vec<UserId> = self.holders.lock().get(hash).cloned().unwrap_or_default();
        if let Some(a) = att
            && !users.contains(&a.author)
        {
            users.push(a.author);
        }
        let n = users.iter().filter(|u| **u != self.me && self.net.peer_online(u)).count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Provider order for a download: the author when online, then the other holders, then
    /// an offline author last (a dead first provider costs the 4 s connect timeout).
    pub(crate) fn providers(&self, att: &AttRef, holders: Vec<(UserId, AddrHint)>) -> Vec<(UserId, AddrHint)> {
        let author = att.author;
        let author_addr = holders
            .iter()
            .find(|(u, _)| *u == author)
            .map(|(_, a)| a.clone())
            .or_else(|| self.net.roster(&att.conv).and_then(|r| r.online.iter().find(|e| e.user == author).map(|e| e.addr.clone())))
            .unwrap_or_default();
        let author_online = author != self.me && self.net.peer_online(&author);
        let mut out: Vec<(UserId, AddrHint)> = Vec::new();
        if author_online {
            out.push((author, author_addr.clone()));
        }
        for (u, a) in holders {
            if u != author && u != self.me && !out.iter().any(|(x, _)| *x == u) {
                out.push((u, a));
            }
        }
        if !author_online && author != self.me {
            out.push((author, author_addr));
        }
        out
    }

    /// Record, look up holders and queue the download of `hash`.
    pub(crate) async fn start_download(&self, hash: BlobHash, att: &AttRef, kind: BlobKind) {
        self.persist_att(&hash, att);
        if kind == BlobKind::Attachment
            && let Err(e) = self.storage.upsert_download(&DownloadRecord {
                hash,
                name: att.name.clone(),
                size: att.size,
                state: TransferState::Queued.as_str().to_string(),
            })
        {
            tracing::warn!(error = %e, "saving a download record failed");
        }
        let holders = self.net.who_has(&att.conv, hash).await;
        self.note_holders(hash, &holders);
        let providers = self.providers(att, holders);
        tracing::debug!(hash = %hash.short(), providers = providers.len(), "download requested");
        self.files.download(hash, kind, providers);
    }

    /// Queue auto-fetches for the attachments of an incoming message (up to the
    /// `autoFetchLimit` setting) and index them.
    pub(crate) fn auto_fetch(&self, conv: ConvId, ev: &StoredEvent) {
        let Body::Message { attachments, .. } = &ev.event.body else { return };
        if attachments.is_empty() {
            return;
        }
        self.index_attachments(conv, ev.event.author, attachments);
        if ev.event.author == self.me {
            return;
        }
        let limit = self.settings.read().auto_fetch_limit;
        if limit == 0 {
            return;
        }
        for a in attachments {
            if a.size <= limit
                && !self.files.is_complete(&a.hash)
                && self.files.transfer(a.hash).is_none()
                && self.fetch_tx.try_send(FetchReq::Auto { hash: a.hash }).is_err()
            {
                tracing::debug!(hash = %a.hash.short(), "fetch queue full; the file can still be downloaded on demand");
            }
        }
    }

    /// Ask for holders again for every parked download (rosters changed).
    pub(crate) fn requery_parked(&self) {
        let parked: Vec<BlobHash> =
            self.transfer_states.lock().iter().filter(|(_, s)| **s == FilesState::Unavailable).map(|(h, _)| *h).collect();
        for hash in parked {
            let _ = self.fetch_tx.try_send(FetchReq::Requery { hash });
        }
    }

    /// A `FileEvent::Transfer`: emit `transfer-updated`, persist state changes.
    pub(crate) fn on_transfer(&self, info: &TransferInfo) {
        let changed = {
            let mut st = self.transfer_states.lock();
            if info.state == FilesState::Complete {
                st.remove(&info.hash) != Some(FilesState::Complete)
            } else {
                st.insert(info.hash, info.state) != Some(info.state)
            }
        };
        let att = self.att_ref(&info.hash);
        if changed && info.kind == BlobKind::Attachment {
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
            total: info.total.or(att.as_ref().map(|a| a.size)).unwrap_or(0),
            peers: self.online_holders(&info.hash, att.as_ref()),
        };
        self.events.emit(Ev::TransferUpdated, t);
    }

    /// `Transfer` of `hash` right now (`idle` when there is no request and it is not held).
    pub(crate) fn transfer_dto(&self, hash: &BlobHash) -> dto::Transfer {
        let att = self.att_ref(hash);
        let peers = self.online_holders(hash, att.as_ref());
        let size = att.as_ref().map(|a| a.size).unwrap_or(0);
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
        if unknown_holders && t.state != TransferState::Complete && self.att_ref(&hash).is_some() {
            let _ = self.fetch_tx.try_send(FetchReq::Peers { hash });
        }
        t
    }

    /// `download` command.
    pub(crate) async fn cmd_download(&self, hash: BlobHash) -> Result<dto::Transfer, String> {
        if self.files.is_complete(&hash) {
            return Ok(self.transfer_dto(&hash));
        }
        let att = self.att_ref(&hash).ok_or_else(|| "This file isn't available on this device.".to_string())?;
        match self.files.transfer(hash).map(|i| i.state) {
            Some(FilesState::Queued | FilesState::Downloading) => {}
            _ => self.start_download(hash, &att, BlobKind::Attachment).await,
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
        let hint = self.att_ref(&hash).map(|a| a.mime);
        let head = &bytes[..bytes.len().min(16)];
        let mime = shell::serve_mime(hint.as_deref(), head);
        Some((bytes.to_vec(), mime))
    }

    /// `open_file` command: export to the download folder (once per blob) and open it.
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
        if self.cfg.test.no_devices {
            // Headless and test runs never launch other programs.
            return Ok(());
        }
        shell::open_with_default_app(&path).map_err(|e| {
            tracing::warn!(path = %path.display(), error = %e, "opening the file failed");
            "Couldn't open the file. You can find it in your download folder.".to_string()
        })
    }

    /// A previous export of `hash` that still exists with the right size.
    async fn previous_export(&self, hash: &BlobHash) -> Option<PathBuf> {
        let raw = self.storage.get_kv(&export_key(hash)).ok().flatten()?;
        let path = PathBuf::from(String::from_utf8(raw).ok()?);
        let size = self.files.status(*hash).await.ok()?.total?;
        let meta = tokio::fs::metadata(&path).await.ok()?;
        (meta.is_file() && meta.len() == size).then_some(path)
    }

    /// Import the files of an outgoing message. Errors are user-facing.
    pub(crate) async fn import_attachments(&self, conv: ConvId, paths: &[String]) -> Result<Vec<Attachment>, String> {
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
            let r = AttRef { conv, author: self.me, name: a.name.clone(), size: a.size, mime: a.mime.clone() };
            self.persist_att(&a.hash, &r);
            self.atts.lock().insert(a.hash, r);
            out.push(a);
        }
        Ok(out)
    }
}
