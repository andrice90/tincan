//! The UI event stream: names from the `types.ts` `Events` map and a bounded broadcast.

use serde::Serialize;
use tokio::sync::broadcast;

use crate::UiEvent;

/// Capacity of the UI event broadcast. A receiver that falls further behind gets
/// `Lagged`; the shell then re-sends a snapshot.
pub(crate) const UI_EVENT_CAPACITY: usize = 1024;

/// Every event the app can emit. The names are keys of the `types.ts` `Events` map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Ev {
    /// `snapshot`
    Snapshot,
    /// `server-updated`
    ServerUpdated,
    /// `channels-updated`
    ChannelsUpdated,
    /// `members-updated`
    MembersUpdated,
    /// `message-added`
    MessageAdded,
    /// `message-updated`
    MessageUpdated,
    /// `typing`
    Typing,
    /// `dm-updated`
    DmUpdated,
    /// `voice-updated`
    VoiceUpdated,
    /// `speaking`
    Speaking,
    /// `call-updated`
    CallUpdated,
    /// `transfer-updated`
    TransferUpdated,
    /// `clips-updated`
    ClipsUpdated,
    /// `sound-played`
    SoundPlayed,
    /// `mic-level`
    MicLevel,
    /// `toast`
    Toast,
}

impl Ev {
    /// Every variant, in `types.ts` order.
    pub const ALL: [Ev; 16] = [
        Ev::Snapshot,
        Ev::ServerUpdated,
        Ev::ChannelsUpdated,
        Ev::MembersUpdated,
        Ev::MessageAdded,
        Ev::MessageUpdated,
        Ev::Typing,
        Ev::DmUpdated,
        Ev::VoiceUpdated,
        Ev::Speaking,
        Ev::CallUpdated,
        Ev::TransferUpdated,
        Ev::ClipsUpdated,
        Ev::SoundPlayed,
        Ev::MicLevel,
        Ev::Toast,
    ];

    /// The `types.ts` event name.
    pub const fn name(self) -> &'static str {
        match self {
            Ev::Snapshot => "snapshot",
            Ev::ServerUpdated => "server-updated",
            Ev::ChannelsUpdated => "channels-updated",
            Ev::MembersUpdated => "members-updated",
            Ev::MessageAdded => "message-added",
            Ev::MessageUpdated => "message-updated",
            Ev::Typing => "typing",
            Ev::DmUpdated => "dm-updated",
            Ev::VoiceUpdated => "voice-updated",
            Ev::Speaking => "speaking",
            Ev::CallUpdated => "call-updated",
            Ev::TransferUpdated => "transfer-updated",
            Ev::ClipsUpdated => "clips-updated",
            Ev::SoundPlayed => "sound-played",
            Ev::MicLevel => "mic-level",
            Ev::Toast => "toast",
        }
    }
}

/// Every event name the app can emit (for the contract test).
pub(crate) const EVENT_NAMES: [&str; 16] = {
    let mut out = [""; 16];
    let mut i = 0;
    while i < Ev::ALL.len() {
        out[i] = Ev::ALL[i].name();
        i += 1;
    }
    out
};

/// Sends UI events. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Emitter {
    tx: broadcast::Sender<UiEvent>,
}

impl Emitter {
    /// A new stream with [`UI_EVENT_CAPACITY`].
    pub(crate) fn new() -> Self { Self { tx: broadcast::channel(UI_EVENT_CAPACITY).0 } }

    /// A new receiver.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<UiEvent> { self.tx.subscribe() }

    /// Emit `ev` with `payload` (serialized to camelCase JSON). Nobody listening is fine.
    pub(crate) fn emit(&self, ev: Ev, payload: impl Serialize) {
        match serde_json::to_value(payload) {
            Ok(payload) => {
                let _ = self.tx.send(UiEvent { name: ev.name(), payload });
            }
            Err(e) => tracing::error!(event = ev.name(), error = %e, "encoding a UI event failed"),
        }
    }

    /// Emit an already encoded payload.
    pub(crate) fn emit_value(&self, ev: Ev, payload: serde_json::Value) {
        let _ = self.tx.send(UiEvent { name: ev.name(), payload });
    }

    /// Short `toast` helper.
    pub(crate) fn toast(&self, kind: crate::dto::ToastKind, text: impl Into<String>) {
        self.emit(Ev::Toast, crate::dto::Toast { kind, text: text.into() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique() {
        let set: std::collections::HashSet<_> = EVENT_NAMES.iter().collect();
        assert_eq!(set.len(), EVENT_NAMES.len());
        assert!(EVENT_NAMES.iter().all(|n| !n.is_empty()));
    }
}
