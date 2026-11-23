//! Soundboard: the clip library (store `clips` table plus `BlobKind::Clip` blobs), clips
//! heard in this session, pulling a heard clip into the library, and announcing the library
//! to every host (`Net::set_clips`) so holder lookups list me as a source.
//!
//! Limits (architecture.md; the UI shows the same): 20 s decoded and 5 MB per file. Every
//! decode runs on a blocking thread under `catch_unwind` (see `engine::decode_clip`).
//!
//! A heard clip is not replayed locally: the player's engine mixes it into its outgoing
//! voice stream (`AF_SOUNDBOARD`), so every listener already hears it once.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tincan_audio::{AudioError, ClipPcm};
use tincan_core::storage::Storage;
use tincan_core::wire::{AddrHint, ClipInfo};
use tincan_core::{BlobHash, ConvId, UserId, now_ms};
use tincan_files::{BlobKind, FilesError, TransferInfo};
use tincan_store::ClipRecord;

use super::engine::{DecodeError, decode_clip};
use super::session::Kind;
use super::{MAX_CLIP_BYTES, MAX_CLIP_MS, MAX_CLIP_NAME_CHARS, Media, State};
use crate::app::{Core, net_err};
use crate::dto::{self, ToastKind};
use crate::emit::Ev;

/// Heard clips kept per session.
const MAX_HEARD: usize = 50;
/// Decoded clips kept in memory (a 20 s clip is about 3.8 MB of PCM).
const PCM_CACHE_CLIPS: usize = 4;
/// Decoded samples kept in memory (about 8 MB).
const PCM_CACHE_SAMPLES: usize = 2_000_000;
/// kv key of the pending pulls (JSON object: hash hex -> [`Fetch`]).
const FETCHES_KEY: &str = "app/clip_fetches";
/// A player's `SoundPlayed` announcements closer together than this are dropped (the net
/// layer forwards them without a limit; each one costs an event and a store read).
pub(super) const SOUND_EVERY: Duration = Duration::from_millis(250);
/// Players remembered by the throttle before old entries are pruned.
const SOUND_SEEN_MAX: usize = 256;

/// Per-player throttle of `SoundPlayed`: true when `by`'s announcement at `now` goes
/// through (and is remembered), false when it came within [`SOUND_EVERY`] of the last one
/// that did.
pub(super) fn sound_allowed(seen: &mut HashMap<UserId, Instant>, by: UserId, now: Instant) -> bool {
    if seen.get(&by).is_some_and(|t| now.duration_since(*t) < SOUND_EVERY) {
        return false;
    }
    if seen.len() >= SOUND_SEEN_MAX {
        seen.retain(|_, t| now.duration_since(*t) < SOUND_EVERY);
        if seen.len() >= SOUND_SEEN_MAX {
            // Only room members can announce, so this takes a huge room; start over.
            seen.clear();
        }
    }
    seen.insert(by, now);
    true
}

/// A clip someone played in my session.
#[derive(Clone, Debug)]
pub(super) struct HeardClip {
    /// As announced (name cleaned up).
    pub info: ClipInfo,
    /// Who played it.
    pub by: UserId,
    /// The session's conversation (holder lookups go there).
    pub conv: ConvId,
}

/// Clips heard in this session, newest last.
#[derive(Default)]
pub(super) struct Heard {
    list: VecDeque<HeardClip>,
}

impl Heard {
    pub(super) fn is_empty(&self) -> bool { self.list.is_empty() }
    pub(super) fn clear(&mut self) { self.list.clear(); }
    fn get(&self, hash: &BlobHash) -> Option<&HeardClip> { self.list.iter().find(|h| h.info.hash == *hash) }
    fn remove(&mut self, hash: &BlobHash) { self.list.retain(|h| h.info.hash != *hash); }
    fn put(&mut self, h: HeardClip) {
        self.remove(&h.info.hash);
        if self.list.len() >= MAX_HEARD {
            self.list.pop_front();
        }
        self.list.push_back(h);
    }
}

/// A heard clip being pulled into the library.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Fetch {
    /// Name to save it under.
    pub name: String,
    /// Who played it (z32), for logs.
    pub by: String,
}

/// Decoded clips, least recently used first.
#[derive(Default)]
pub(super) struct PcmCache {
    entries: VecDeque<(BlobHash, Arc<ClipPcm>)>,
    samples: usize,
}

impl PcmCache {
    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.samples = 0;
    }
    fn get(&mut self, hash: &BlobHash) -> Option<Arc<ClipPcm>> {
        let i = self.entries.iter().position(|(h, _)| h == hash)?;
        let e = self.entries.remove(i)?;
        let pcm = e.1.clone();
        self.entries.push_back(e);
        Some(pcm)
    }
    fn put(&mut self, hash: BlobHash, pcm: Arc<ClipPcm>) {
        self.remove(&hash);
        self.samples += pcm.len();
        self.entries.push_back((hash, pcm));
        while self.entries.len() > PCM_CACHE_CLIPS || (self.samples > PCM_CACHE_SAMPLES && self.entries.len() > 1) {
            if let Some((_, old)) = self.entries.pop_front() {
                self.samples -= old.len();
            }
        }
    }
    fn remove(&mut self, hash: &BlobHash) {
        if let Some(i) = self.entries.iter().position(|(h, _)| h == hash)
            && let Some((_, old)) = self.entries.remove(i)
        {
            self.samples -= old.len();
        }
    }
}

/// Pending pulls persisted by an earlier run (they resume with the partial downloads).
pub(super) fn load_fetches(core: &Core) -> HashMap<BlobHash, Fetch> {
    let Ok(Some(raw)) = core.storage.get_kv(FETCHES_KEY) else { return HashMap::new() };
    let map: HashMap<String, Fetch> = serde_json::from_slice(&raw).unwrap_or_default();
    map.into_iter().filter_map(|(k, v)| Some((BlobHash::from_hex(&k)?, v))).collect()
}

fn save_fetches(core: &Core, fetching: &HashMap<BlobHash, Fetch>) {
    let map: HashMap<String, &Fetch> = fetching.iter().map(|(h, f)| (h.to_hex(), f)).collect();
    if let Ok(json) = serde_json::to_vec(&map)
        && let Err(e) = core.storage.set_kv(FETCHES_KEY, &json)
    {
        tracing::warn!(error = %e, "saving pending clip pulls failed");
    }
}

/// A clip name from the user: trimmed, 1 to 32 characters, no control characters.
fn clip_name(raw: &str, fallback: &str) -> Result<String, String> {
    let name = match raw.trim() {
        "" => fallback.trim(),
        n => n,
    };
    if name.is_empty() {
        return Err("Enter a name for the sound.".into());
    }
    if name.chars().count() > MAX_CLIP_NAME_CHARS {
        return Err(format!("Sound names can be up to {MAX_CLIP_NAME_CHARS} characters."));
    }
    if name.chars().any(char::is_control) {
        return Err("Sound names can't contain control characters.".into());
    }
    Ok(name.to_string())
}

/// A clip name from a peer: control and hidden format characters removed, cut to the
/// limit instead of refused.
fn remote_name(raw: &str) -> String {
    let cleaned = crate::shell::clean_text(raw);
    let name: String = cleaned.chars().take(MAX_CLIP_NAME_CHARS).collect();
    if name.trim().is_empty() { "Sound".to_string() } else { name.trim().to_string() }
}

/// A library clip. Pulled clips were saved under a cleaned name; cleaning again covers
/// rows saved before names were cleaned of hidden format characters.
fn owned_dto(r: &ClipRecord) -> dto::Clip {
    dto::Clip { hash: r.hash.to_hex(), name: remote_name(&r.name), duration_ms: r.duration_ms, size: r.size, owned: true, heard_from: None, transfer: None }
}

fn clip_info(r: &ClipRecord) -> ClipInfo { ClipInfo { hash: r.hash, name: r.name.clone(), size: r.size, duration_ms: r.duration_ms } }

/// The download state of a clip being pulled.
fn transfer_of(core: &Core, hash: &BlobHash, size: u64) -> Option<dto::Transfer> {
    core.files.transfer(*hash).map(|t| dto::Transfer {
        hash: hash.to_hex(),
        state: crate::transfers::map_state(t.state),
        have: t.have,
        total: t.total.unwrap_or(size),
        peers: t.peers,
    })
}

impl Media {
    /// The clip library, oldest first.
    fn library(&self, core: &Core) -> Vec<ClipRecord> {
        core.storage.list_clips().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "reading the clip library failed");
            Vec::new()
        })
    }

    fn owned(&self, core: &Core, hash: &BlobHash) -> Option<ClipRecord> { core.storage.get_clip(hash).ok().flatten() }

    fn heard_dto(&self, core: &Core, h: &HeardClip) -> dto::Clip {
        dto::Clip {
            hash: h.info.hash.to_hex(),
            name: h.info.name.clone(),
            duration_ms: h.info.duration_ms,
            size: h.info.size,
            owned: false,
            heard_from: Some(h.by.to_z32()),
            transfer: transfer_of(core, &h.info.hash, h.info.size),
        }
    }

    /// `clips`: my library, then the heard clips I don't own.
    pub(super) fn clip_list(&self, core: &Core) -> Vec<dto::Clip> {
        let st = self.st.lock();
        self.clips_locked(core, &st)
    }

    fn clips_locked(&self, core: &Core, st: &State) -> Vec<dto::Clip> {
        let lib = self.library(core);
        let mut out: Vec<dto::Clip> = lib.iter().map(owned_dto).collect();
        out.extend(st.heard.list.iter().filter(|h| !lib.iter().any(|r| r.hash == h.info.hash)).map(|h| self.heard_dto(core, h)));
        out
    }

    /// Emit `clips-updated` (with `st` locked).
    pub(super) fn emit_clips_locked(&self, core: &Core, st: &State) { core.events.emit(Ev::ClipsUpdated, self.clips_locked(core, st)); }

    fn emit_clips(&self, core: &Core) {
        let st = self.st.lock();
        self.emit_clips_locked(core, &st);
    }

    /// Tell the hosts which clips I hold (they count as held blobs in holder lookups):
    /// each server's host only the clips shared with it, that is, played or pulled in one of
    /// its voice rooms (`Core::announce` does both lists).
    pub(crate) fn announce_clips(&self, core: &Core) { core.announce(); }

    /// The library clips whose blob is complete, as announced.
    pub(crate) fn complete_clips(&self, core: &Core) -> Vec<ClipInfo> {
        self.library(core).iter().filter(|r| core.files.is_complete(&r.hash)).map(clip_info).collect()
    }

    /// A clip blob was dropped: re-announce the held set and the library.
    fn reannounce_after_drop(&self, core: &Core) { core.announce(); }

    /// `clip_add`: decode-check, import, save, announce.
    pub(super) async fn clip_add(&self, core: &Arc<Core>, raw_path: &str, raw_name: &str) -> Result<dto::Clip, String> {
        let path = Path::new(raw_path);
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| raw_path.to_string());
        let stem = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let name = clip_name(raw_name, &stem)?;
        let unreadable = || format!("Couldn't read {file}. Check that the file still exists.");
        let too_big = || format!("{file} is larger than 5 MB. Pick a smaller sound.");
        let meta = tokio::fs::metadata(path).await.map_err(|_| unreadable())?;
        if meta.is_dir() {
            return Err(format!("{file} is a folder. Pick a sound file instead."));
        }
        if meta.len() > MAX_CLIP_BYTES {
            return Err(too_big());
        }
        let bytes = tokio::fs::read(path).await.map_err(|_| unreadable())?;
        if bytes.len() as u64 > MAX_CLIP_BYTES {
            return Err(too_big());
        }
        let bytes = Bytes::from(bytes);
        let pcm = decode_clip(bytes.clone()).await.map_err(|e| match e {
            DecodeError::TooLong => format!("{file} is longer than {} seconds. Pick a shorter sound.", MAX_CLIP_MS / 1000),
            DecodeError::Invalid => format!("{file} isn't a sound Tincan can play. Pick an MP3, WAV, OGG, FLAC or M4A file."),
        })?;
        // Copied into the store: the user may move or edit the original later.
        let imported = core.files.import_bytes(bytes, BlobKind::Clip).await.map_err(|e| {
            tracing::warn!(error = %e, "importing a clip failed");
            "Couldn't save the sound. Try again.".to_string()
        })?;
        let rec = ClipRecord {
            hash: imported.hash,
            name,
            duration_ms: u32::try_from(pcm.duration_ms()).unwrap_or(MAX_CLIP_MS),
            size: imported.size,
            added_ms: now_ms(),
        };
        let rec = match core.storage.add_clip(&rec) {
            Ok(true) => rec,
            Ok(false) => self.owned(core, &rec.hash).unwrap_or(rec),
            Err(e) => {
                tracing::warn!(error = %e, "saving a clip failed");
                return Err("Couldn't save the sound. Try again.".into());
            }
        };
        {
            let mut st = self.st.lock();
            st.heard.remove(&rec.hash);
            if st.fetching.remove(&rec.hash).is_some() {
                save_fetches(core, &st.fetching);
            }
            if st.attached.is_some() {
                st.pcm.put(rec.hash, pcm);
            }
        }
        self.announce_clips(core);
        self.emit_clips(core);
        Ok(owned_dto(&rec))
    }

    /// `clip_remove`: out of the library; the blob is dropped unless it is also an
    /// attachment.
    pub(super) async fn clip_remove(&self, core: &Arc<Core>, hash: BlobHash) {
        if let Err(e) = core.storage.remove_clip(&hash) {
            tracing::warn!(error = %e, "removing a clip failed");
        }
        {
            let mut st = self.st.lock();
            st.pcm.remove(&hash);
            if st.fetching.remove(&hash).is_some() {
                save_fetches(core, &st.fetching);
            }
        }
        if let Err(e) = core.files.remove_kind(hash, BlobKind::Clip).await {
            tracing::debug!(error = %e, "dropping a clip blob failed");
        }
        self.reannounce_after_drop(core);
        self.emit_clips(core);
    }

    /// `clip_rename`.
    pub(super) fn clip_rename(&self, core: &Arc<Core>, hash: BlobHash, raw: &str) -> Result<(), String> {
        let name = clip_name(raw, "")?;
        match core.storage.rename_clip(&hash, &name) {
            Ok(true) => {}
            Ok(false) => return Err("That sound was removed.".into()),
            Err(e) => {
                tracing::warn!(error = %e, "renaming a clip failed");
                return Err("Couldn't rename the sound. Try again.".into());
            }
        }
        self.announce_clips(core);
        self.emit_clips(core);
        Ok(())
    }

    /// Decoded samples of a library clip (cached while in voice).
    async fn pcm_of(&self, core: &Core, hash: BlobHash) -> Result<Arc<ClipPcm>, String> {
        if let Some(p) = self.st.lock().pcm.get(&hash) {
            return Ok(p);
        }
        let bytes = core.files.read_to_bytes(hash, MAX_CLIP_BYTES).await.map_err(|e| {
            tracing::warn!(hash = %hash.short(), error = %e, "reading a clip failed");
            "That sound's file is missing. Remove it and add it again.".to_string()
        })?;
        let pcm = decode_clip(bytes).await.map_err(|_| "That sound can't be played. Remove it and add it again.".to_string())?;
        let mut st = self.st.lock();
        if st.attached.is_some() {
            st.pcm.put(hash, pcm.clone());
        }
        Ok(pcm)
    }

    /// `clip_play`: into the call (and locally), then announce it to the session. Playing
    /// shares the clip with the session's conversation (the server of the room, or the DM
    /// of the call) before anyone hears of it, so listeners may pull it from me.
    pub(super) async fn clip_play(&self, core: &Arc<Core>, hash: BlobHash) -> Result<(), String> {
        self.check_can_play()?;
        let rec = self.owned(core, &hash).ok_or_else(|| "Save that sound before playing it.".to_string())?;
        let pcm = self.pcm_of(core, hash).await?;
        self.check_can_play()?;
        let engine = self.engine_handle().ok_or_else(|| "not-in-voice".to_string())?;
        engine.play_clip(pcm).map_err(|e| match e {
            AudioError::NotInCall => "not-in-voice".to_string(),
            AudioError::Deafened => "deafened".to_string(),
            other => {
                tracing::warn!(error = %other, "playing a clip failed");
                "Audio isn't working right now. Try again in a moment.".to_string()
            }
        })?;
        let conv = self.st.lock().attached.as_ref().map(|a| a.conv);
        if let Some(conv) = conv {
            core.share_blob(hash, conv);
        }
        core.net.sound_played(clip_info(&rec)).await.map_err(net_err)
    }

    fn check_can_play(&self) -> Result<(), String> {
        let st = self.st.lock();
        if st.attached.is_none() {
            return Err("not-in-voice".into());
        }
        if st.deafened {
            return Err("deafened".into());
        }
        Ok(())
    }

    /// Someone in my session played a clip: `sound-played`, and remember it as heard.
    pub(super) fn on_sound(&self, core: &Arc<Core>, conv: ConvId, by: UserId, clip: &ClipInfo) {
        if clip.size > MAX_CLIP_BYTES || clip.duration_ms > MAX_CLIP_MS + tincan_audio::CLIP_LENGTH_TOLERANCE_MS {
            tracing::debug!(by = %by.short(), size = clip.size, "ignoring an oversized clip announcement");
            return;
        }
        let mut st = self.st.lock();
        let Some(kind) = st.attached.as_ref().filter(|a| a.conv == conv).map(|a| a.kind) else { return };
        if !sound_allowed(&mut st.sound_seen, by, Instant::now()) {
            tracing::debug!(by = %by.short(), "dropping a sound announcement that came too soon");
            return;
        }
        let player = match kind {
            Kind::Room { server, .. } => core.view(&server).map(|v| core.author_name(&v, &by)).unwrap_or_else(|| core.user_name(&by)),
            Kind::Call { .. } => core.user_name(&by),
        };
        let heard = HeardClip { info: ClipInfo { name: remote_name(&clip.name), ..clip.clone() }, by, conv };
        let payload = match self.owned(core, &clip.hash) {
            Some(rec) => dto::Clip { heard_from: Some(by.to_z32()), ..owned_dto(&rec) },
            None => {
                let d = self.heard_dto(core, &heard);
                st.heard.put(heard);
                d
            }
        };
        core.events.emit(Ev::SoundPlayed, dto::SoundPlayed { user_id: by.to_z32(), name: player, clip: payload });
    }

    /// `clip_fetch`: pull a heard clip from the player and every holder the host knows.
    pub(super) async fn clip_fetch(&self, core: &Arc<Core>, hash: BlobHash) -> Result<dto::Clip, String> {
        if let Some(rec) = self.owned(core, &hash) {
            return Ok(owned_dto(&rec));
        }
        let heard = self.st.lock().heard.get(&hash).cloned().ok_or_else(|| "That sound is no longer available.".to_string())?;
        {
            let mut st = self.st.lock();
            st.fetching.insert(hash, Fetch { name: heard.info.name.clone(), by: heard.by.to_z32() });
            save_fetches(core, &st.fetching);
        }
        if core.files.is_complete(&hash) {
            // Already held (as an attachment, or as a clip no longer in the library): tag it
            // as a clip and finish the pull here. A `Completed` event may follow the tag;
            // `finish_fetch` runs once (the second sees the pull already gone).
            let fetch = Fetch { name: heard.info.name.clone(), by: heard.by.to_z32() };
            let tagged = match core.files.read_to_bytes(hash, MAX_CLIP_BYTES).await {
                Ok(bytes) => core.files.import_bytes(bytes, BlobKind::Clip).await.map(|_| ()).map_err(|e| {
                    tracing::warn!(error = %e, "tagging a held clip failed");
                    "Couldn't save the sound. Try again.".to_string()
                }),
                Err(FilesError::TooLarge { size, .. }) => {
                    // A peer announced a held attachment's hash with a small claimed size.
                    tracing::info!(hash = %hash.short(), size, "a held blob announced as a clip is too large");
                    self.st.lock().heard.remove(&hash);
                    Err("That sound is larger than 5 MB, so it wasn't saved.".to_string())
                }
                Err(e) => {
                    tracing::warn!(error = %e, "reading a held clip failed");
                    Err("Couldn't save the sound. Try again.".to_string())
                }
            };
            let finished = match tagged {
                Ok(()) => self.finish_fetch(core, hash, fetch).await,
                Err(e) => {
                    self.forget_fetch(core, &hash);
                    Err(e)
                }
            };
            if let Err(e) = finished {
                self.emit_clips(core);
                return Err(e);
            }
            if let Some(rec) = self.owned(core, &hash) {
                return Ok(owned_dto(&rec));
            }
        } else {
            let addr = self.addr_of(core, &heard);
            let mut providers: Vec<(UserId, AddrHint)> = vec![(heard.by, addr)];
            let holders = core.net.who_has(&heard.conv, hash).await;
            // The session's conversation named the player and these holders: a pull they
            // complete shares the clip with it (`Core::on_completed`).
            core.nominate(hash, heard.conv, std::iter::once(heard.by).chain(holders.iter().map(|(u, _)| *u)));
            for (u, a) in holders {
                if u != core.me && !providers.iter().any(|(x, _)| *x == u) {
                    providers.push((u, a));
                }
            }
            tracing::debug!(hash = %hash.short(), providers = providers.len(), "pulling a clip");
            core.files.download(hash, BlobKind::Clip, providers);
        }
        let st = self.st.lock();
        let d = st.heard.get(&hash).map(|h| self.heard_dto(core, h)).unwrap_or_else(|| self.heard_dto(core, &heard));
        self.emit_clips_locked(core, &st);
        Ok(d)
    }

    /// Where the player can be reached (roster address in rooms; DMs resolve by id).
    fn addr_of(&self, core: &Core, h: &HeardClip) -> AddrHint {
        core.net
            .roster(&h.conv)
            .and_then(|r| r.online.iter().find(|e| e.user == h.by).map(|e| e.addr.clone()))
            .unwrap_or_default()
    }

    /// Progress of a clip download: refuse oversized ones, refresh the heard tile.
    pub(super) fn on_clip_transfer(&self, core: &Arc<Core>, info: &TransferInfo) {
        let mut st = self.st.lock();
        if !st.fetching.contains_key(&info.hash) && st.heard.get(&info.hash).is_none() {
            return;
        }
        // tincan-files already stops a clip download past 5 MB (`TooLarge`); this tells
        // the user and forgets the pull.
        if info.state == tincan_files::TransferState::TooLarge || info.total.is_some_and(|t| t > MAX_CLIP_BYTES) {
            tracing::warn!(hash = %info.hash.short(), total = ?info.total, "a pulled clip is too large; cancelling");
            core.files.cancel(info.hash);
            let core2 = core.clone();
            let hash = info.hash;
            tokio::spawn(async move {
                let _ = core2.files.remove_kind(hash, BlobKind::Clip).await;
                core2.media.reannounce_after_drop(&core2);
            });
            if st.fetching.remove(&info.hash).is_some() {
                save_fetches(core, &st.fetching);
            }
            st.heard.remove(&info.hash);
            core.toast(ToastKind::Error, "That sound is larger than 5 MB, so it wasn't saved.");
        }
        self.emit_clips_locked(core, &st);
    }

    /// A clip blob became complete: if it was being pulled, check it and add it.
    pub(super) fn on_clip_completed(&self, core: &Arc<Core>, hash: BlobHash) {
        // Only pulls: `clip_add` imports fire this too, before their row exists.
        let Some(fetch) = self.st.lock().fetching.get(&hash).cloned() else { return };
        let core = core.clone();
        tokio::spawn(async move {
            if let Err(e) = core.media.finish_fetch(&core, hash, fetch).await {
                core.toast(ToastKind::Error, e);
            }
        });
    }

    /// Drop a pending pull (persisted).
    fn forget_fetch(&self, core: &Core, hash: &BlobHash) {
        let mut st = self.st.lock();
        if st.fetching.remove(hash).is_some() {
            save_fetches(core, &st.fetching);
        }
    }

    /// Check a pulled clip and add it to the library. Err is the notice for the user when
    /// it was not a playable sound (already handled: the pull is forgotten).
    async fn finish_fetch(&self, core: &Arc<Core>, hash: BlobHash, fetch: Fetch) -> Result<(), String> {
        let checked = match core.files.read_to_bytes(hash, MAX_CLIP_BYTES).await {
            Ok(bytes) => {
                let size = bytes.len() as u64;
                decode_clip(bytes).await.map(|pcm| (pcm, size)).map_err(|_| ())
            }
            Err(e) => {
                tracing::warn!(hash = %hash.short(), error = %e, "reading a pulled clip failed");
                Err(())
            }
        };
        {
            let mut st = self.st.lock();
            if st.fetching.remove(&hash).is_none() {
                // Removed meanwhile (clip_remove or a second completion).
                return Ok(());
            }
            save_fetches(core, &st.fetching);
        }
        match checked {
            Ok((pcm, size)) => {
                let rec = ClipRecord {
                    hash,
                    name: fetch.name,
                    duration_ms: u32::try_from(pcm.duration_ms()).unwrap_or(MAX_CLIP_MS),
                    size,
                    added_ms: now_ms(),
                };
                if let Err(e) = core.storage.add_clip(&rec) {
                    tracing::warn!(error = %e, "saving a pulled clip failed");
                }
                tracing::info!(hash = %hash.short(), from = %fetch.by, "clip pulled into the library");
                let mut st = self.st.lock();
                st.heard.remove(&hash);
                if st.attached.is_some() {
                    st.pcm.put(hash, pcm);
                }
            }
            Err(()) => {
                if let Err(e) = core.files.remove_kind(hash, BlobKind::Clip).await {
                    tracing::debug!(error = %e, "dropping a bad clip failed");
                }
                self.st.lock().heard.remove(&hash);
                core.announce();
                self.announce_clips(core);
                self.emit_clips(core);
                return Err(format!("{} couldn't be played, so it wasn't saved.", fetch.name));
            }
        }
        self.announce_clips(core);
        self.emit_clips(core);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(clip_name("  Airhorn ", "x").unwrap(), "Airhorn");
        assert_eq!(clip_name("", "file stem").unwrap(), "file stem");
        assert!(clip_name("", " ").is_err());
        assert!(clip_name(&"a".repeat(33), "").is_err());
        assert!(clip_name("bad\u{7}", "").is_err());
        assert_eq!(remote_name(" \u{1b}[31mred "), "[31mred");
        assert_eq!(remote_name(&"b".repeat(80)).chars().count(), MAX_CLIP_NAME_CHARS);
        assert_eq!(remote_name("\n\t"), "Sound");
        assert_eq!(remote_name("Air\u{202E}horn\u{200B}"), "Airhorn", "hidden format characters are dropped");
        assert_eq!(remote_name("\u{2066}\u{FEFF}"), "Sound");
    }

    #[test]
    fn pcm_cache_is_bounded() {
        let mut c = PcmCache::default();
        for i in 0..10u8 {
            c.put(BlobHash([i; 32]), Arc::new(ClipPcm::from_samples(vec![0.0; 960_000])));
        }
        assert!(c.entries.len() <= PCM_CACHE_CLIPS);
        assert!(c.samples <= PCM_CACHE_SAMPLES);
        assert!(c.get(&BlobHash([9; 32])).is_some());
        assert!(c.get(&BlobHash([0; 32])).is_none());
        c.remove(&BlobHash([9; 32]));
        assert!(c.get(&BlobHash([9; 32])).is_none());
        c.clear();
        assert_eq!(c.samples, 0);
    }

    /// APP-MEDIA-3: a player's announcements closer than 250 ms are dropped, other players
    /// are not affected, and the map stays bounded.
    #[test]
    fn sound_announcements_are_throttled_per_player() {
        let (a, b) = (tincan_core::keys::Identity::generate().user(), tincan_core::keys::Identity::generate().user());
        let mut seen = HashMap::new();
        let t0 = Instant::now();
        assert!(sound_allowed(&mut seen, a, t0));
        assert!(!sound_allowed(&mut seen, a, t0 + Duration::from_millis(100)), "too soon");
        assert!(sound_allowed(&mut seen, b, t0 + Duration::from_millis(100)), "another player");
        assert!(sound_allowed(&mut seen, a, t0 + SOUND_EVERY), "after the interval");
        // A flood of 1000 announcements in one second lets through 4.
        let mut through = 0;
        let t1 = t0 + Duration::from_secs(10);
        for i in 0..1000u64 {
            through += u32::from(sound_allowed(&mut seen, a, t1 + Duration::from_millis(i)));
        }
        assert_eq!(through, 4);
        for _ in 0..(SOUND_SEEN_MAX * 2) {
            sound_allowed(&mut seen, tincan_core::keys::Identity::generate().user(), t1 + Duration::from_secs(5));
        }
        assert!(seen.len() <= SOUND_SEEN_MAX + 1, "{} remembered", seen.len());
    }

    #[test]
    fn heard_is_bounded_and_moves_to_the_end() {
        let mut h = Heard::default();
        let me = tincan_core::keys::Identity::generate().user();
        let clip = |i: u8| HeardClip {
            info: ClipInfo { hash: BlobHash([i; 32]), name: format!("c{i}"), size: 1, duration_ms: 1 },
            by: me,
            conv: ConvId([0; 32]),
        };
        for i in 0..60u8 {
            h.put(clip(i));
        }
        assert_eq!(h.list.len(), MAX_HEARD);
        h.put(clip(20));
        assert_eq!(h.list.back().unwrap().info.hash, BlobHash([20; 32]));
        assert_eq!(h.list.len(), MAX_HEARD);
    }
}
