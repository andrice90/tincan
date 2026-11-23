//! Non-media command implementations (servers, channels, messages, DMs, settings).
//! Every `Err` is a short user-facing sentence (api.md).

use std::sync::Arc;

use tincan_core::event::{Body, ChannelKind as CoreKind};
use tincan_core::invite::{Invite, parse_user_code};
use tincan_core::state::{MAX_TEXT_CHARS, dm_channel};
use tincan_core::storage::Storage;
use tincan_core::{ChannelId, ConvId, EventId, UserId, now_ms};
use tincan_store::ContactRecord;

use crate::app::{Core, net_err, validate_name};
use crate::dto::{self, ToastKind};
use crate::emit::Ev;
use crate::settings;
use crate::views::{ChannelInfo, ConvView, Refresh, short_name};

/// Most messages per page.
const MAX_PAGE: u32 = 200;
/// Most `message-updated` events per DM delivery ack.
const MAX_DELIVERED_UPDATES: u64 = 200;

/// Parse a server or DM conversation id.
pub(crate) fn parse_conv(s: &str) -> Result<ConvId, String> {
    ConvId::from_hex(s).ok_or_else(|| "That conversation id is not valid.".to_string())
}

/// Parse a channel id.
pub(crate) fn parse_channel(s: &str) -> Result<ChannelId, String> {
    EventId::from_hex(s).ok_or_else(|| "That channel id is not valid.".to_string())
}

/// Parse a z-base-32 user id.
pub(crate) fn parse_user(s: &str) -> Result<UserId, String> {
    UserId::from_z32(s).ok_or_else(|| "That user id is not valid.".to_string())
}

/// `<hlc>:<eventIdHex>`.
pub(crate) fn format_cursor(key: (u64, EventId)) -> String { format!("{}:{}", key.0, key.1.to_hex()) }

/// Inverse of [`format_cursor`].
pub(crate) fn parse_cursor(s: &str) -> Result<(u64, EventId), String> {
    let bad = || "That page cursor is not valid.".to_string();
    let (hlc, id) = s.split_once(':').ok_or_else(bad)?;
    Ok((hlc.parse().map_err(|_| bad())?, EventId::from_hex(id).ok_or_else(bad)?))
}

fn owner_only(view: &ConvView, me: &UserId) -> Result<(), String> {
    if view.is_owner(me) { Ok(()) } else { Err("Only the server owner can do that.".into()) }
}

fn live_channel<'a>(view: &'a ConvView, ch: &ChannelId) -> Result<&'a ChannelInfo, String> {
    view.all_channels.get(ch).filter(|c| !c.deleted).ok_or_else(|| "That channel doesn't exist anymore.".to_string())
}

impl Core {
    fn conv_view(&self, conv: &ConvId) -> Result<Arc<ConvView>, String> {
        self.view(conv).ok_or_else(|| "That conversation isn't on this device anymore.".to_string())
    }

    // ------------------------------------------------------------------ servers

    /// `create_server`.
    pub(crate) async fn cmd_create_server(&self, name: &str) -> Result<dto::ServerSummary, String> {
        self.require_name()?;
        let name = name.trim();
        validate_name(name, "Enter a server name")?;
        let server = self.net.create_server(name).await.map_err(net_err)?;
        self.invalidate_view(&server);
        self.update_policy();
        self.server_summary(&server).ok_or_else(|| "Couldn't create the server. Try again.".to_string())
    }

    /// `create_invite`.
    pub(crate) async fn cmd_create_invite(&self, server: ConvId) -> Result<String, String> {
        self.server_view(&server)?;
        self.net.create_invite(server).await.map_err(net_err)
    }

    /// `join_server`.
    pub(crate) async fn cmd_join_server(&self, invite: &str) -> Result<dto::ServerSummary, String> {
        self.require_name()?;
        let fresh = Invite::decode(invite).ok().filter(|inv| self.view(&inv.server).is_none()).map(|inv| inv.server);
        if let Some(server) = fresh {
            // The history sync follows the join at once: no per-message events for it.
            self.arm_bulk(server);
        }
        let server = self.net.join(invite).await.map_err(net_err)?;
        if fresh == Some(server) {
            self.arm_bulk(server);
        }
        self.invalidate_view(&server);
        self.update_policy();
        self.server_summary(&server).ok_or_else(|| "Couldn't join the server. Try again.".to_string())
    }

    /// `leave_server`.
    pub(crate) async fn cmd_leave_server(&self, server: ConvId) -> Result<(), String> {
        self.server_view(&server)?;
        self.net.leave(server).await.map_err(net_err)?;
        self.invalidate_view(&server);
        self.forget_sent(&server);
        self.update_policy();
        Ok(())
    }

    /// `rename_server`.
    pub(crate) async fn cmd_rename_server(&self, server: ConvId, name: &str) -> Result<(), String> {
        let view = self.server_view(&server)?;
        owner_only(&view, &self.me)?;
        let name = name.trim();
        validate_name(name, "Enter a server name")?;
        if name == view.name {
            return Ok(());
        }
        self.net.publish(server, Body::ServerUpdate { name: Some(name.to_string()) }).await.map_err(net_err)?;
        Ok(())
    }

    /// `channels`.
    pub(crate) fn cmd_channels(&self, server: ConvId) -> Result<Vec<dto::Channel>, String> {
        let view = self.server_view(&server)?;
        Ok(self.channels(&view))
    }

    /// `create_channel`.
    pub(crate) async fn cmd_create_channel(&self, server: ConvId, name: &str, kind: dto::ChannelKind) -> Result<dto::Channel, String> {
        let view = self.server_view(&server)?;
        owner_only(&view, &self.me)?;
        let name = name.trim();
        validate_name(name, "Enter a channel name")?;
        let kind = match kind {
            dto::ChannelKind::Text => CoreKind::Text,
            dto::ChannelKind::Voice => CoreKind::Voice,
        };
        let position = view.all_channels.values().filter(|c| c.kind == kind).map(|c| c.position.saturating_add(1)).max().unwrap_or(0);
        let ev = self
            .net
            .publish(server, Body::ChannelCreate { name: name.to_string(), kind, position })
            .await
            .map_err(net_err)?;
        let info = ChannelInfo { id: ev.id, name: name.to_string(), kind, position, deleted: false };
        Ok(self.channel_dto(&server, &info))
    }

    /// `rename_channel`.
    pub(crate) async fn cmd_rename_channel(&self, server: ConvId, channel: ChannelId, name: &str) -> Result<(), String> {
        let view = self.server_view(&server)?;
        owner_only(&view, &self.me)?;
        let current = live_channel(&view, &channel)?;
        let name = name.trim();
        validate_name(name, "Enter a channel name")?;
        if current.name == name {
            return Ok(());
        }
        let body = Body::ChannelUpdate { channel, name: Some(name.to_string()), position: None, deleted: None };
        self.net.publish(server, body).await.map_err(net_err)?;
        Ok(())
    }

    /// `delete_channel`.
    pub(crate) async fn cmd_delete_channel(&self, server: ConvId, channel: ChannelId) -> Result<(), String> {
        let view = self.server_view(&server)?;
        owner_only(&view, &self.me)?;
        live_channel(&view, &channel)?;
        let body = Body::ChannelUpdate { channel, name: None, position: None, deleted: Some(true) };
        self.net.publish(server, body).await.map_err(net_err)?;
        Ok(())
    }

    /// `members`.
    pub(crate) fn cmd_members(&self, server: ConvId) -> Result<Vec<dto::Member>, String> {
        let view = self.server_view(&server)?;
        Ok(self.members(&view))
    }

    // ------------------------------------------------------------------ messages

    /// The channel a message command targets: the DM channel, or a live text channel.
    fn target_channel(&self, view: &ConvView, channel: Option<&str>) -> Result<ChannelId, String> {
        if view.is_dm {
            return Ok(dm_channel(&view.conv));
        }
        let raw = channel.ok_or_else(|| "Pick a channel first.".to_string())?;
        let id = parse_channel(raw)?;
        let ch = live_channel(view, &id)?;
        if ch.kind != CoreKind::Text {
            return Err("Voice rooms have no messages.".into());
        }
        Ok(id)
    }

    /// `messages`: a page, oldest first.
    pub(crate) fn cmd_messages(&self, conv: ConvId, channel: Option<&str>, before: Option<&str>, limit: u32) -> Result<dto::MessagePage, String> {
        let view = self.conv_view(&conv)?;
        let channel = if view.is_dm {
            None
        } else {
            let id = parse_channel(channel.ok_or_else(|| "Pick a channel first.".to_string())?)?;
            if !view.all_channels.contains_key(&id) {
                return Err("That channel doesn't exist anymore.".into());
            }
            Some(id)
        };
        let before = before.map(parse_cursor).transpose()?;
        let limit = limit.clamp(1, MAX_PAGE) as usize;
        let mut evs = self.storage_messages(&conv, channel.as_ref(), before, limit + 1);
        let more = evs.len() > limit;
        evs.truncate(limit);
        let cursor = if more { evs.last().map(|e| format_cursor(e.order_key())) } else { None };
        evs.reverse();
        let delivered = if view.is_dm { self.net.dm_delivered(&conv) } else { 0 };
        let messages = evs.iter().filter_map(|e| self.message_dto(&view, e, delivered)).collect();
        Ok(dto::MessagePage { messages, cursor })
    }

    /// `send_message`.
    pub(crate) async fn cmd_send_message(&self, conv: ConvId, channel: Option<&str>, text: &str, paths: &[String]) -> Result<dto::Message, String> {
        let view = self.conv_view(&conv)?;
        let channel = self.target_channel(&view, channel)?;
        if !view.is_dm && !view.members.contains(&self.me) {
            return Err("You are not a member of this server anymore.".into());
        }
        if text.trim().is_empty() && paths.is_empty() {
            return Err("Type a message or attach a file.".into());
        }
        if text.chars().count() > MAX_TEXT_CHARS {
            return Err(format!("Messages can be up to {MAX_TEXT_CHARS} characters."));
        }
        let attachments = self.import_attachments(conv, view.is_dm, paths).await?;
        if let Err(e) = tincan_core::state::validate_message(text, &attachments) {
            tracing::warn!(error = %e, "message failed validation");
            return Err("Couldn't send this message.".into());
        }
        let ev = self
            .net
            .publish(conv, Body::Message { channel, text: text.to_string(), attachments })
            .await
            .map_err(net_err)?;
        let delivered = if view.is_dm { self.net.dm_delivered(&conv) } else { 0 };
        self.message_dto(&view, &ev, delivered).ok_or_else(|| "Couldn't send this message.".to_string())
    }

    /// `delete_message`.
    pub(crate) async fn cmd_delete_message(&self, conv: ConvId, message: EventId) -> Result<(), String> {
        let view = self.conv_view(&conv)?;
        let gone = || "That message doesn't exist anymore.".to_string();
        let ev = self.storage.event(&message).ok().flatten().ok_or_else(gone)?;
        if ev.conv() != conv || !matches!(ev.event.body, Body::Message { .. }) {
            return Err(gone());
        }
        if ev.deleted {
            return Ok(());
        }
        let mine = ev.event.author == self.me;
        if !mine && (view.is_dm || !view.is_owner(&self.me)) {
            return Err("You can only delete your own messages.".into());
        }
        self.net.publish(conv, Body::Delete { target: message }).await.map_err(net_err)?;
        Ok(())
    }

    /// `mark_read`.
    pub(crate) fn cmd_mark_read(&self, conv: ConvId, channel: Option<&str>) -> Result<(), String> {
        let view = self.conv_view(&conv)?;
        let channel = if view.is_dm { None } else { Some(parse_channel(channel.ok_or_else(|| "Pick a channel first.".to_string())?)?) };
        if let Some(newest) = self.storage_messages(&conv, channel.as_ref(), None, 1).first()
            && let Err(e) = self.storage.set_read(&conv, channel.as_ref(), newest.event.hlc)
        {
            tracing::warn!(error = %e, "saving a read marker failed");
            return Err("Couldn't mark this as read.".into());
        }
        self.refresh(&conv, Refresh::UNREAD);
        Ok(())
    }

    /// `typing`.
    pub(crate) async fn cmd_typing(&self, conv: ConvId, channel: Option<&str>) -> Result<(), String> {
        let view = self.conv_view(&conv)?;
        let channel = self.target_channel(&view, channel)?;
        self.net.typing(conv, channel).await;
        Ok(())
    }

    // ------------------------------------------------------------------ DMs

    /// A DM conversation record appeared (open_dm, add_contact, or an incoming DM).
    pub(crate) fn on_dm_opened(&self, conv: ConvId, peer: UserId) {
        self.invalidate_view(&conv);
        let delivered = self.net.dm_delivered(&conv);
        self.dm_acked.lock().entry(conv).or_insert(delivered);
        // The peer may now connect for blobs, but only fetches what is shared with this DM.
        self.access.write().add_dm(conv, peer);
        if matches!(self.storage.get_contact(&peer), Ok(None)) {
            let name = self.user_name(&peer);
            let rec = ContactRecord { user: peer, name, added_ms: now_ms() };
            if let Err(e) = self.storage.upsert_contact(&rec) {
                tracing::warn!(error = %e, "saving a contact failed");
            }
        }
    }

    /// The contact confirmed my DM events up to `up_to`: `message-updated` (pending false).
    pub(crate) fn on_dm_delivered(&self, conv: ConvId, up_to: u64) {
        let prev = {
            let mut acked = self.dm_acked.lock();
            let prev = acked.get(&conv).copied().unwrap_or(0);
            if up_to <= prev {
                return;
            }
            acked.insert(conv, up_to);
            prev
        };
        let from = prev.saturating_add(1).max(up_to.saturating_sub(MAX_DELIVERED_UPDATES - 1));
        match self.storage.events_range(&conv, &self.me, from, up_to) {
            Ok(evs) => {
                for ev in evs.iter().filter(|e| matches!(e.event.body, Body::Message { .. })) {
                    self.emit_message(Ev::MessageUpdated, &conv, ev);
                }
            }
            Err(e) => tracing::warn!(error = %e, "reading delivered DMs failed"),
        }
    }

    /// `open_dm`.
    pub(crate) async fn cmd_open_dm(&self, user: UserId) -> Result<dto::DmSummary, String> {
        let conv = self.net.dm_open(user).await.map_err(net_err)?;
        self.on_dm_opened(conv, user);
        self.dm_summary(&conv).ok_or_else(|| "Couldn't open that conversation. Try again.".to_string())
    }

    /// `dm_watch`: keep the connection to the contact of the DM on screen up, so their
    /// presence is right and calls work after the link idled out or a restart (also for a
    /// contact who shares no server). One DM at a time; null releases it.
    pub(crate) fn cmd_dm_watch(&self, conv: Option<ConvId>) -> Result<(), String> {
        let Some(conv) = conv else {
            self.dm_watch.lock().take();
            return Ok(());
        };
        let view = self.conv_view(&conv)?;
        let peer = view.peer.filter(|_| view.is_dm).ok_or_else(|| "That isn't a direct message.".to_string())?;
        let mut w = self.dm_watch.lock();
        if !w.as_ref().is_some_and(|(c, _)| *c == conv) {
            // Dials now and redials with backoff while held; dropping the old guard lets
            // that link idle out as usual.
            *w = Some((conv, self.net.dm_watch(peer)));
        }
        Ok(())
    }

    /// `add_contact`: a `tincan1u...` code or a bare user id.
    pub(crate) async fn cmd_add_contact(&self, code: &str) -> Result<dto::DmSummary, String> {
        let user = parse_user_code(code).ok_or_else(|| "That isn't a Tincan ID. Paste the code that starts with tincan1u.".to_string())?;
        if user == self.me {
            return Err("That's your own ID. Paste the ID of the person you want to message.".into());
        }
        let summary = self.cmd_open_dm(user).await?;
        if let Ok(Some(c)) = self.storage.get_contact(&user)
            && c.name.is_empty()
        {
            let rec = ContactRecord { name: short_name(&user), ..c };
            let _ = self.storage.upsert_contact(&rec);
        }
        Ok(summary)
    }

    // ------------------------------------------------------------------ settings

    /// `settings_set`: validate, persist, apply what can apply live.
    pub(crate) fn cmd_settings_set(self: &Arc<Self>, new: dto::Settings) -> Result<dto::Settings, String> {
        let new = settings::normalize(new)?;
        let old = self.settings.read().clone();
        settings::save(&self.storage, &new)?;
        *self.settings.write() = new.clone();
        if new.allow_hosting != old.allow_hosting {
            self.net.set_allow_hosting(new.allow_hosting);
        }
        if new.custom_relay_url != old.custom_relay_url {
            let text = if self.cfg.test.offline_network {
                "Relays are off in this test profile, so the relay setting has no effect."
            } else {
                "Restart Tincan to use the new relay setting."
            };
            self.toast(ToastKind::Info, text);
        }
        self.media.apply_settings(self, &new);
        Ok(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trip() {
        let id = EventId([0xab; 32]);
        let c = format_cursor((123_456, id));
        assert_eq!(c, format!("123456:{}", id.to_hex()));
        assert_eq!(parse_cursor(&c).unwrap(), (123_456, id));
        assert!(parse_cursor("nope").is_err());
        assert!(parse_cursor("1:zz").is_err());
        assert!(parse_cursor(&format!("x:{}", id.to_hex())).is_err());
    }

    #[test]
    fn id_parsing() {
        assert!(parse_conv("00").is_err());
        assert!(parse_conv(&"ab".repeat(32)).is_ok());
        assert!(parse_user("not a user").is_err());
        let u = tincan_core::keys::Identity::generate().user();
        assert_eq!(parse_user(&u.to_z32()).unwrap(), u);
    }
}
