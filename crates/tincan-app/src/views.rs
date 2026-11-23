//! Cached per-conversation views and the DTO builders.
//!
//! `Net::conv_state` clones the whole `ConvState`, message index included, so it is never
//! called per message. Each conversation gets a small [`ConvView`] (names, channels, owner)
//! that is rebuilt only after a non-message change (`NetEvent::ConvUpdated`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::Serialize;
use tincan_core::event::{Body, ChannelKind as CoreKind, StoredEvent};
use tincan_core::hlc::hlc_ms;
use tincan_core::state::ConvState;
use tincan_core::{ChannelId, ConvId, UserId};
use tincan_net::{ConnState, PathKind as NetPath};

use crate::app::Core;
use crate::dto;
use crate::emit::Ev;
use crate::shell::{self, clean_text};

/// One channel of a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelInfo {
    /// Channel id.
    pub id: ChannelId,
    /// Name.
    pub name: String,
    /// Text or voice.
    pub kind: CoreKind,
    /// Position within its kind.
    pub position: u32,
    /// Deleted by the owner.
    pub deleted: bool,
}

/// The parts of a `ConvState` the UI needs, cheap to share.
#[derive(Clone, Debug)]
pub(crate) struct ConvView {
    /// The conversation.
    pub conv: ConvId,
    /// A DM.
    pub is_dm: bool,
    /// DMs: the other party.
    pub peer: Option<UserId>,
    /// Server name.
    pub name: String,
    /// Server owner.
    pub owner: Option<UserId>,
    /// Visible channels, text first, by position.
    pub channels: Vec<ChannelInfo>,
    /// Every channel by id, deleted ones included.
    pub all_channels: BTreeMap<ChannelId, ChannelInfo>,
    /// Current members (servers).
    pub members: Vec<UserId>,
    /// Display names of everyone who ever had one here.
    pub names: HashMap<UserId, String>,
}

impl ConvView {
    /// Build from a state snapshot. Names stay as published (commands compare against
    /// them); every DTO shows them through [`display`], since they are peer-supplied.
    pub fn from_state(st: &ConvState, me: &UserId) -> Self {
        let info = |c: &tincan_core::state::Channel| ChannelInfo {
            id: c.id,
            name: c.name.clone(),
            kind: c.kind,
            position: c.position,
            deleted: c.deleted,
        };
        let mut members: Vec<UserId> = st.members.keys().filter(|u| st.is_member(u)).copied().collect();
        members.sort();
        Self {
            conv: st.conv,
            is_dm: st.is_dm,
            peer: if st.is_dm { st.dm_parties.iter().find(|u| *u != me).copied() } else { None },
            name: st.name.clone(),
            owner: st.owner,
            channels: st.sorted_channels().into_iter().map(info).collect(),
            all_channels: st.channels.iter().map(|(id, c)| (*id, info(c))).collect(),
            members,
            names: st.members.iter().filter(|(_, m)| !m.name.is_empty()).map(|(u, m)| (*u, m.name.clone())).collect(),
        }
    }

    /// The name of `user` here for display (cleaned), if it has a visible one.
    pub fn display_name(&self, user: &UserId) -> Option<String> {
        self.known_name(user).map(clean_text).filter(|n| !n.is_empty())
    }

    /// The name of `user` here, if known.
    pub fn known_name(&self, user: &UserId) -> Option<&str> { self.names.get(user).map(String::as_str) }

    /// Is `user` the owner?
    pub fn is_owner(&self, user: &UserId) -> bool { self.owner.as_ref() == Some(user) }
}

/// First 8 characters of the z32 id: the fallback display name.
pub(crate) fn short_name(user: &UserId) -> String { user.to_z32().chars().take(8).collect() }

/// `s`, or `fallback` when cleaning left nothing.
fn or_fallback(s: String, fallback: &str) -> String { if s.is_empty() { fallback.to_string() } else { s } }

/// A peer-supplied server, channel or file name for a DTO: control and hidden format
/// characters removed (`fallback` if nothing is left).
pub(crate) fn display(name: &str, fallback: &str) -> String { or_fallback(clean_text(name), fallback) }

/// A peer-supplied attachment name for a DTO: like [`display`], and runs of whitespace
/// (no-break spaces included) fold to one space, as in the exported name, so padding
/// cannot push the real extension out of sight.
pub(crate) fn file_display(name: &str) -> String {
    or_fallback(shell::clean_line(name, tincan_core::state::MAX_ATTACHMENT_NAME_CHARS), "file")
}

/// Which DTOs of a conversation to recompute.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Refresh {
    /// `server-updated`.
    pub server: bool,
    /// `channels-updated`.
    pub channels: bool,
    /// `members-updated`.
    pub members: bool,
    /// `dm-updated`.
    pub dm: bool,
}

impl Refresh {
    /// Everything.
    pub const ALL: Refresh = Refresh { server: true, channels: true, members: true, dm: true };
    /// Unread state changed (server dot and channel dots, or the DM).
    pub const UNREAD: Refresh = Refresh { server: true, channels: true, members: false, dm: true };

    /// Union.
    pub fn merge(&mut self, o: Refresh) {
        self.server |= o.server;
        self.channels |= o.channels;
        self.members |= o.members;
        self.dm |= o.dm;
    }
}

/// Key of the last payload sent per DTO (deduplication).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SentKey {
    /// `server-updated`.
    Server(ConvId),
    /// `channels-updated`.
    Channels(ConvId),
    /// `members-updated`.
    Members(ConvId),
    /// `dm-updated`.
    Dm(ConvId),
}

fn host_state(s: ConnState) -> dto::HostState {
    match s {
        ConnState::Connecting => dto::HostState::Connecting,
        ConnState::Member => dto::HostState::Member,
        ConnState::Hosting => dto::HostState::Hosting,
        ConnState::Solo => dto::HostState::Solo,
        ConnState::Offline => dto::HostState::Offline,
    }
}

pub(crate) fn path_kind(p: NetPath) -> dto::PathKind {
    match p {
        NetPath::Direct => dto::PathKind::Direct,
        NetPath::Relay => dto::PathKind::Relay,
        NetPath::None => dto::PathKind::None,
    }
}

/// True when a roster from this state describes who is online now.
fn roster_current(s: ConnState) -> bool { matches!(s, ConnState::Member | ConnState::Hosting | ConnState::Solo) }

impl Core {
    /// The cached view of `conv` (built on first use). None for unknown conversations.
    pub(crate) fn view(&self, conv: &ConvId) -> Option<Arc<ConvView>> {
        if let Some(v) = self.views.lock().get(conv) {
            return Some(v.clone());
        }
        let st = self.net.conv_state(conv)?;
        let v = Arc::new(ConvView::from_state(&st, &self.me));
        self.views.lock().insert(*conv, v.clone());
        Some(v)
    }

    /// Drop the cached view (after a non-message change).
    pub(crate) fn invalidate_view(&self, conv: &ConvId) { self.views.lock().remove(conv); }

    /// A server view, or the user-facing error for an unknown server.
    pub(crate) fn server_view(&self, conv: &ConvId) -> Result<Arc<ConvView>, String> {
        match self.view(conv) {
            Some(v) if !v.is_dm => Ok(v),
            _ => Err("That server isn't on this device anymore.".to_string()),
        }
    }

    /// Display name of `user` for DMs and typing: my name, a server name, the contact
    /// record, or the short id.
    pub(crate) fn user_name(&self, user: &UserId) -> String {
        if *user == self.me
            && let Some(n) = self.name.read().clone()
        {
            return n;
        }
        let views: Vec<Arc<ConvView>> = self.views.lock().values().cloned().collect();
        if let Some(n) = views.iter().filter(|v| !v.is_dm).find_map(|v| v.display_name(user)) {
            return n;
        }
        match self.storage.get_contact(user).map(|c| c.map(|c| clean_text(&c.name))) {
            Ok(Some(name)) if !name.is_empty() => name,
            _ => short_name(user),
        }
    }

    /// Author name inside a conversation.
    pub(crate) fn author_name(&self, view: &ConvView, user: &UserId) -> String {
        if view.is_dm {
            return self.user_name(user);
        }
        view.display_name(user).unwrap_or_else(|| short_name(user))
    }

    // ------------------------------------------------------------------ DTOs

    /// `Me`, None before a name was picked.
    pub(crate) fn me_dto(&self) -> Option<dto::Me> {
        self.name.read().clone().map(|name| dto::Me {
            user_id: self.me.to_z32(),
            user_code: tincan_core::invite::user_code(&self.me),
            name,
        })
    }

    /// Is any text channel of the server unread?
    fn channel_unread(&self, conv: &ConvId, ch: &ChannelInfo) -> bool {
        ch.kind == CoreKind::Text && self.storage.unread(conv, Some(&ch.id), &self.me).unwrap_or(false)
    }

    /// `ServerSummary` of a joined server.
    pub(crate) fn server_summary(&self, conv: &ConvId) -> Option<dto::ServerSummary> {
        let view = self.view(conv)?;
        if view.is_dm {
            return None;
        }
        let info = self.net.connection(conv);
        let online_count = if roster_current(info.state) {
            self.net
                .roster(conv)
                .map(|r| r.online.len() + usize::from(!r.online.iter().any(|e| e.user == self.me)))
                .unwrap_or(1)
        } else {
            1
        };
        let unread = view.channels.iter().any(|c| self.channel_unread(conv, c));
        Some(dto::ServerSummary {
            id: conv.to_hex(),
            name: display(&view.name, "Unnamed server"),
            owner_id: view.owner.map(|o| o.to_z32()).unwrap_or_default(),
            is_owner: view.is_owner(&self.me),
            connection: dto::Connection {
                state: host_state(info.state),
                host_id: info.host.map(|h| h.to_z32()),
                host_name: info.host.map(|h| if h == self.me { self.user_name(&h) } else { self.author_name(&view, &h) }),
                path: path_kind(info.path),
                rtt_ms: info.rtt_ms,
            },
            unread,
            online_count: u32::try_from(online_count).unwrap_or(u32::MAX),
        })
    }

    /// `Channel` DTO.
    pub(crate) fn channel_dto(&self, conv: &ConvId, ch: &ChannelInfo) -> dto::Channel {
        dto::Channel {
            id: ch.id.to_hex(),
            server_id: conv.to_hex(),
            name: display(&ch.name, "unnamed"),
            kind: match ch.kind {
                CoreKind::Text => dto::ChannelKind::Text,
                CoreKind::Voice => dto::ChannelKind::Voice,
            },
            position: ch.position,
            unread: self.channel_unread(conv, ch),
        }
    }

    /// Visible channels of a server.
    pub(crate) fn channels(&self, view: &ConvView) -> Vec<dto::Channel> {
        view.channels.iter().map(|c| self.channel_dto(&view.conv, c)).collect()
    }

    /// Current members of a server, online first, then by name.
    pub(crate) fn members(&self, view: &ConvView) -> Vec<dto::Member> {
        let info = self.net.connection(&view.conv);
        let roster = if roster_current(info.state) { self.net.roster(&view.conv) } else { None };
        let host = info.host;
        let mut out: Vec<dto::Member> = view
            .members
            .iter()
            .map(|u| {
                let entry = roster.as_ref().and_then(|r| r.online.iter().find(|e| e.user == *u));
                dto::Member {
                    user_id: u.to_z32(),
                    name: self.author_name(view, u),
                    online: entry.is_some() || *u == self.me,
                    is_host: host == Some(*u),
                    is_owner: view.is_owner(u),
                    voice_channel_id: entry.and_then(|e| e.voice).map(|c| c.to_hex()),
                    muted: entry.is_some_and(|e| e.muted),
                    deafened: entry.is_some_and(|e| e.deafened),
                    sharing: entry.is_some_and(|e| e.sharing.is_some()),
                    speaking: self.media.is_speaking(u),
                }
            })
            .collect();
        out.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        out
    }

    /// `Message` DTO of a stored message event (None for other bodies). `delivered` is the
    /// DM delivery ack (ignored for servers). Records the attachments in the hash index.
    ///
    /// Attachment names are cleaned like every peer name, and the MIME type is the one of
    /// the name the file would be saved under, not the sender's claim: the UI's icon and
    /// image preview then match what `open_file` will do with it.
    pub(crate) fn message_dto(&self, view: &ConvView, ev: &StoredEvent, delivered: u64) -> Option<dto::Message> {
        let Body::Message { channel, text, attachments } = &ev.event.body else { return None };
        let author = ev.event.author;
        if !ev.deleted {
            self.index_attachments(view.conv, view.is_dm, author, attachments);
        }
        Some(dto::Message {
            id: ev.id.to_hex(),
            conv_id: view.conv.to_hex(),
            channel_id: if view.is_dm { None } else { Some(channel.to_hex()) },
            author_id: author.to_z32(),
            author_name: self.author_name(view, &author),
            text: if ev.deleted { String::new() } else { text.clone() },
            attachments: if ev.deleted {
                Vec::new()
            } else {
                attachments
                    .iter()
                    .map(|a| dto::Attachment {
                        hash: a.hash.to_hex(),
                        name: file_display(&a.name),
                        size: a.size,
                        mime: shell::mime_for_name(&shell::safe_file_name(&a.name)).to_string(),
                    })
                    .collect()
            },
            sent_at: hlc_ms(ev.event.hlc),
            deleted: ev.deleted,
            pending: view.is_dm && author == self.me && ev.event.seq > delivered,
        })
    }

    /// `DmSummary` of a DM conversation.
    pub(crate) fn dm_summary(&self, conv: &ConvId) -> Option<dto::DmSummary> {
        let view = self.view(conv)?;
        let peer = view.peer?;
        let last = self.storage_messages(conv, None, None, 1).into_iter().next();
        Some(dto::DmSummary {
            conv_id: conv.to_hex(),
            user_id: peer.to_z32(),
            name: self.user_name(&peer),
            online: self.net.peer_online(&peer),
            unread: self.storage.unread(conv, None, &self.me).unwrap_or(false),
            last_message_at: last.map(|e| hlc_ms(e.event.hlc)),
        })
    }

    /// A page of stored messages (newest first), logging storage errors.
    pub(crate) fn storage_messages(
        &self,
        conv: &ConvId,
        channel: Option<&ChannelId>,
        before: Option<(u64, tincan_core::EventId)>,
        limit: usize,
    ) -> Vec<StoredEvent> {
        use tincan_core::storage::Storage;
        match self.storage.messages(conv, channel, before, limit) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "reading messages failed");
                Vec::new()
            }
        }
    }

    /// Every server and DM summary, in join order.
    pub(crate) fn snapshot(&self) -> dto::AppSnapshot {
        use tincan_core::storage::Storage;
        let convs = self.storage.convs().unwrap_or_default();
        let mut servers = Vec::new();
        let mut dms = Vec::new();
        for c in convs {
            if c.is_dm {
                dms.extend(self.dm_summary(&c.id));
            } else {
                servers.extend(self.server_summary(&c.id));
            }
        }
        dto::AppSnapshot {
            me: self.me_dto(),
            servers,
            dms,
            settings: self.settings.read().clone(),
            voice: self.media.voice_snapshot(self),
            call: self.media.call_snapshot(self),
            version: self.cfg.app_version.clone(),
        }
    }

    /// Emit `snapshot` now.
    pub(crate) fn emit_snapshot(&self) { self.events.emit(Ev::Snapshot, self.snapshot()); }

    /// Recompute the selected DTOs of `conv` and emit those that changed since last sent.
    pub(crate) fn refresh(&self, conv: &ConvId, what: Refresh) {
        let Some(view) = self.view(conv) else { return };
        if view.is_dm {
            if what.dm
                && let Some(d) = self.dm_summary(conv)
            {
                self.emit_if_changed(SentKey::Dm(*conv), Ev::DmUpdated, d);
            }
            return;
        }
        if what.channels {
            let channels = self.channels(&view);
            self.emit_if_changed(SentKey::Channels(*conv), Ev::ChannelsUpdated, dto::ChannelsUpdated { server_id: conv.to_hex(), channels });
        }
        if what.members {
            let members = self.members(&view);
            self.emit_if_changed(SentKey::Members(*conv), Ev::MembersUpdated, dto::MembersUpdated { server_id: conv.to_hex(), members });
        }
        if what.server
            && let Some(s) = self.server_summary(conv)
        {
            self.emit_if_changed(SentKey::Server(*conv), Ev::ServerUpdated, s);
        }
    }

    /// Emit unless the same payload was the last one sent under `key`.
    fn emit_if_changed(&self, key: SentKey, ev: Ev, payload: impl Serialize) {
        let Ok(value) = serde_json::to_value(payload) else { return };
        {
            let mut sent = self.sent.lock();
            if sent.get(&key) == Some(&value) {
                return;
            }
            sent.insert(key, value.clone());
        }
        self.events.emit_value(ev, value);
    }

    /// Forget what was sent for `conv` (after leaving it).
    pub(crate) fn forget_sent(&self, conv: &ConvId) {
        self.sent.lock().retain(|k, _| !matches!(k, SentKey::Server(c) | SentKey::Channels(c) | SentKey::Members(c) | SentKey::Dm(c) if c == conv));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_names_are_cleaned_for_display() {
        assert_eq!(display("Gen\u{202E}eral\u{200B}", "unnamed"), "General");
        assert_eq!(display("\u{2066}\u{2069}\u{7}", "unnamed"), "unnamed");
        let padded = format!("Invoice.pdf{}.exe", "\u{a0}".repeat(60));
        assert_eq!(file_display(&padded), "Invoice.pdf .exe", "padding cannot hide the extension");
        assert_eq!(file_display("photo\u{202E}gnp.exe"), "photognp.exe");
        assert_eq!(file_display("\u{FEFF}"), "file");
    }
}
