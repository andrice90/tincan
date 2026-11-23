//! Avatars and server icons.
//!
//! - **Upload:** `set_avatar` and `set_server_icon` take the image bytes from the UI and
//!   re-check them (`tincan_core::image::inspect`: magic bytes, 64 KiB, header dimensions).
//!   The blob is imported as `BlobKind::Image`. My avatar is published as `ProfileImage` in
//!   every server (again on each connection, like the Profile name) and announced to DM
//!   contacts (`Net::set_avatar`, `Ctl::DmAvatar`). A server icon is published as
//!   `ServerIcon` and shared with that server.
//! - **Fetch:** a DTO builder that meets an image it does not hold yet indexes the image as a
//!   reference (the conversation and the author that named it) and queues
//!   `FetchReq::Image`. The worker downloads it as `BlobKind::Image` (refused past 64 KiB)
//!   from the author and the holders that conversation's host lists. Only the current image
//!   of each member and server is ever fetched: history sync replays every old one, but
//!   DTOs are built from the state.
//! - **Access:** the same rules as attachments (`index.rs`). A reference grants nothing; a
//!   downloaded image is shared with the conversations that named the peer who delivered it,
//!   so other members can fetch it from me while its author is offline. My own current
//!   avatar is the one exception: any peer allowed to connect may fetch it
//!   (`Access::set_own_avatar`), since it is published to all of them anyway.
//! - **Shown only once checked:** a held blob gets a URL only after its bytes were read
//!   back and passed `inspect` (`ImgState::Ok`). Holding the hash is not enough: it may be a
//!   large attachment or clip someone named as their avatar.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tincan_core::event::{Body, ImageRef};
use tincan_core::image::{self, MAX_IMAGE_BYTES};
use tincan_core::{BlobHash, ConvId, UserId};
use tincan_files::BlobKind;

use crate::app::{Core, net_err};
use crate::dto;
use crate::index::AttRef;
use crate::transfers::FetchReq;
use crate::views::Refresh;

/// kv key of my avatar (JSON [`AvatarRecord`]).
pub(crate) const AVATAR_KEY: &str = "app/avatar";
/// Image hashes whose state is remembered.
pub(crate) const IMAGE_STATES: usize = 4096;

/// Where an avatar or icon blob stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImgState {
    /// Download or check queued.
    Pending,
    /// Held and checked: a PNG or WebP of at most 64 KiB.
    Ok,
    /// Held but not a valid image, or refused for its size: never shown.
    Bad,
}

#[derive(Serialize, Deserialize)]
struct AvatarRecord {
    hash: String,
    size: u64,
    mime: String,
}

/// Read my avatar from kv.
pub(crate) fn load_avatar(storage: &tincan_store::SqliteStorage) -> Option<ImageRef> {
    use tincan_core::storage::Storage;
    let raw = storage.get_kv(AVATAR_KEY).ok().flatten()?;
    let rec: AvatarRecord = serde_json::from_slice(&raw).ok()?;
    let img = ImageRef { hash: BlobHash::from_hex(&rec.hash)?, size: rec.size, mime: rec.mime };
    image::validate_image_ref(&img).ok().map(|()| img)
}

/// The file name an image reference is indexed under (the type decides how it is served).
pub(crate) fn ref_name(img: &ImageRef) -> &'static str { if img.mime == "image/png" { "image.png" } else { "image.webp" } }

impl Core {
    /// The URL of `img` once it is held and checked. Otherwise None, after indexing it as
    /// named by `author` in `conv` and queueing its download or check (once per session).
    pub(crate) fn image_url(&self, conv: ConvId, dm: bool, author: UserId, img: &ImageRef) -> Option<String> {
        let state = self.images.lock().get(&img.hash).copied();
        match state {
            Some(ImgState::Ok) => return Some(self.blob_url(&img.hash)),
            Some(ImgState::Bad) => return None,
            Some(ImgState::Pending) => {
                // Index this naming too, so a later download can use this author.
                self.index_image(conv, dm, author, img);
                return None;
            }
            None => {}
        }
        self.index_image(conv, dm, author, img);
        if self.fetch_tx.try_send(FetchReq::Image { hash: img.hash }).is_ok() {
            self.images.lock().insert(img.hash, ImgState::Pending);
        }
        None
    }

    fn blob_url(&self, hash: &BlobHash) -> String { format!("{}{}", self.cfg.file_url_base, hash.to_hex()) }

    /// Record that `author` named `img` in `conv` (a reference, like a message attachment:
    /// it routes downloads and grants nothing).
    pub(crate) fn index_image(&self, conv: ConvId, dm: bool, author: UserId, img: &ImageRef) {
        let r = AttRef { conv, dm, author, name: ref_name(img).to_string(), size: img.size, mime: img.mime.clone() };
        self.update_info(img.hash, |i| i.add_ref(r));
    }

    /// Worker: download `hash` as an image, or check it if it is held already.
    pub(crate) async fn fetch_image(self: &Arc<Self>, hash: BlobHash) {
        if self.files.is_complete(&hash) {
            self.check_image(hash).await;
            return;
        }
        if self.files.transfer(hash).is_some() {
            return;
        }
        match self.att_info(&hash).filter(|i| !i.refs.is_empty()) {
            Some(info) if info.declared_size().is_some_and(|s| s <= MAX_IMAGE_BYTES) => self.start_download(hash, &info, BlobKind::Image).await,
            _ => {
                self.images.lock().insert(hash, ImgState::Bad);
            }
        }
    }

    /// Worker: read a held image back and check it; refresh what shows it.
    pub(crate) async fn check_image(self: &Arc<Self>, hash: BlobHash) {
        let ok = match self.files.held_size(&hash) {
            Some(size) if size <= MAX_IMAGE_BYTES => match self.files.read_to_bytes(hash, MAX_IMAGE_BYTES).await {
                Ok(bytes) => image::inspect(&bytes).is_ok(),
                Err(e) => {
                    tracing::debug!(hash = %hash.short(), error = %e, "reading an image failed");
                    false
                }
            },
            _ => false,
        };
        if !ok {
            tracing::info!(hash = %hash.short(), "an avatar or icon is not a small PNG or WebP; not showing it");
        }
        self.images.lock().insert(hash, if ok { ImgState::Ok } else { ImgState::Bad });
        if ok {
            self.refresh_image(hash);
        }
    }

    /// An image download ended without the blob (too large, or failed): stop showing it as
    /// pending. A failure is tried again the next time a DTO names it.
    pub(crate) fn on_image_transfer(&self, info: &tincan_files::TransferInfo) {
        use tincan_files::TransferState as S;
        match info.state {
            S::TooLarge => {
                self.images.lock().insert(info.hash, ImgState::Bad);
            }
            S::Failed => {
                self.images.lock().remove(&info.hash);
            }
            S::Queued | S::Downloading | S::Unavailable | S::Complete => {}
        }
    }

    /// Recompute the DTOs that may show `hash`: every conversation that references it, and
    /// `Me` when it is my avatar.
    fn refresh_image(self: &Arc<Self>, hash: BlobHash) {
        if let Some(info) = self.att_info(&hash) {
            for conv in info.convs() {
                self.refresh(&conv, Refresh::ALL);
            }
        }
        // A DM can show a contact's avatar from a shared server, and voice tiles show them too.
        self.refresh_dms();
        self.media.on_name_changed(self, UserId::default());
        if self.avatar.read().as_ref().is_some_and(|a| a.hash == hash) {
            self.emit_snapshot();
        }
    }

    /// My avatar's URL (it is imported, so it is checked at once).
    pub(crate) fn my_avatar_url(&self) -> Option<String> {
        let img = self.avatar.read().clone()?;
        (self.images.lock().get(&img.hash) == Some(&ImgState::Ok)).then(|| self.blob_url(&img.hash))
    }

    /// Check and import image bytes from the UI. Errors are user-facing.
    async fn import_image(&self, bytes: Vec<u8>) -> Result<ImageRef, String> {
        let info = image::inspect(&bytes)?;
        let imported = self.files.import_bytes(bytes.into(), BlobKind::Image).await.map_err(|e| {
            tracing::warn!(error = %e, "importing an image failed");
            "Couldn't save that image. Try again.".to_string()
        })?;
        self.images.lock().insert(imported.hash, ImgState::Ok);
        Ok(ImageRef { hash: imported.hash, size: imported.size, mime: info.mime.to_string() })
    }

    /// `set_avatar`: check, store and publish my avatar (`None` removes it).
    pub(crate) async fn cmd_set_avatar(self: &Arc<Self>, bytes: Option<Vec<u8>>) -> Result<dto::Me, String> {
        use tincan_core::storage::Storage;
        self.require_name()?;
        let img = match bytes {
            Some(b) => Some(self.import_image(b).await?),
            None => None,
        };
        let saved = match &img {
            Some(i) => {
                let rec = AvatarRecord { hash: i.hash.to_hex(), size: i.size, mime: i.mime.clone() };
                self.storage.set_kv(AVATAR_KEY, &serde_json::to_vec(&rec).unwrap_or_default())
            }
            None => self.storage.set_kv(AVATAR_KEY, b""),
        };
        saved.map_err(|e| {
            tracing::warn!(error = %e, "saving the avatar failed");
            "Couldn't save your avatar. Try again.".to_string()
        })?;
        *self.avatar.write() = img.clone();
        self.access.write().set_own_avatar(img.as_ref().map(|i| i.hash));
        self.net.set_avatar(img);
        for conv in self.net.convs() {
            self.ensure_avatar(conv);
        }
        self.emit_snapshot();
        self.me_dto().ok_or_else(|| "Pick a display name first.".to_string())
    }

    /// Publish my `ProfileImage` in `server` if the avatar there differs from mine.
    pub(crate) fn ensure_avatar(self: &Arc<Self>, server: ConvId) {
        let mine = self.avatar.read().clone();
        let Some(view) = self.view(&server) else { return };
        if view.is_dm || !view.members.contains(&self.me) || view.avatars.get(&self.me) == mine.as_ref() {
            return;
        }
        if !self.avatar_inflight.lock().insert(server) {
            return;
        }
        let core = self.clone();
        tokio::spawn(async move {
            let mut image = mine;
            loop {
                if let Err(e) = core.net.publish(server, Body::ProfileImage { image: image.clone() }).await {
                    tracing::info!(server = %server.short(), error = %e, "publishing my avatar failed; will retry on reconnect");
                    core.avatar_inflight.lock().remove(&server);
                    return;
                }
                let current = core.avatar.read().clone();
                if current == image {
                    break;
                }
                image = current;
            }
            core.avatar_inflight.lock().remove(&server);
            if *core.avatar.read() != image {
                core.ensure_avatar(server);
            }
        });
    }

    /// `set_server_icon`: owner only; check, store, share with the server and publish.
    pub(crate) async fn cmd_set_server_icon(&self, server: ConvId, bytes: Option<Vec<u8>>) -> Result<dto::ServerSummary, String> {
        let view = self.server_view(&server)?;
        crate::commands::owner_only(&view, &self.me)?;
        let img = match bytes {
            Some(b) => {
                let img = self.import_image(b).await?;
                self.index_image(server, false, self.me, &img);
                self.share_blob(img.hash, server);
                Some(img)
            }
            None => None,
        };
        if view.icon != img {
            self.net.publish(server, Body::ServerIcon { image: img }).await.map_err(net_err)?;
            self.invalidate_view(&server);
        }
        self.server_summary(&server).ok_or_else(|| "That server isn't on this device anymore.".to_string())
    }

    /// The URL of the avatar of `user` in a server (mine when it is me).
    pub(crate) fn member_avatar_url(&self, view: &crate::views::ConvView, user: &UserId) -> Option<String> {
        if *user == self.me {
            return self.my_avatar_url();
        }
        let img = view.avatars.get(user)?;
        self.image_url(view.conv, view.is_dm, *user, img)
    }

    /// The URL of a server's icon.
    pub(crate) fn icon_url(&self, view: &crate::views::ConvView) -> Option<String> {
        let img = view.icon.as_ref()?;
        let owner = view.owner?;
        self.image_url(view.conv, false, owner, img)
    }

    /// The URL of the avatar of `user` for a DM or call: the one they announced over our DM,
    /// else their avatar in any server I am in.
    pub(crate) fn dm_avatar_url(&self, user: &UserId) -> Option<String> {
        if *user == self.me {
            return self.my_avatar_url();
        }
        let conv = ConvId::dm(&self.me, user);
        if let Some(img) = self.net.dm_peer_avatar(&conv) {
            return self.image_url(conv, true, *user, &img);
        }
        // A contact who never announced one over the DM: a shared server's copy.
        self.net.convs().iter().filter_map(|c| self.view(c)).filter(|v| !v.is_dm).find_map(|v| {
            let img = v.avatars.get(user)?;
            self.image_url(v.conv, false, *user, img)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_names_follow_the_type() {
        let mut img = ImageRef { hash: BlobHash([1; 32]), size: 10, mime: "image/png".into() };
        assert_eq!(ref_name(&img), "image.png");
        img.mime = "image/webp".into();
        assert_eq!(ref_name(&img), "image.webp");
    }
}
