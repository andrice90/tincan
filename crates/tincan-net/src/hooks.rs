//! A tap on media, voice and call control messages.
//!
//! The session and DM layers handle history, presence and election themselves, and the media
//! layer of this crate handles every media and call message (`VoiceJoin`, `VoiceLeave`,
//! `Subscribe`, `Unsubscribe`, `KeyframeRequest`, `TargetBitrate`, `ReceiverReport`,
//! `SoundPlayed`, `CallInvite`, `CallAccept`, `CallDecline`, `CallEnd`, `CallState`; see
//! [`crate::Net::voice_join`] and [`crate::Net::call_start`]). An [`ExtraCtlHandler`]
//! registered with `Net::set_extra_ctl_handler` additionally sees each of them afterwards
//! (logging, diagnostics, experiments). `Net::send_ctl_to_host`, `Net::send_ctl_to_member`,
//! `Net::broadcast_ctl`, `Net::send_dm_ctl` and the connection accessors stay available, but
//! the media layer owns the datagrams and uni streams on server and DM connections: reading
//! them elsewhere steals media.

use tincan_core::wire::Ctl;
use tincan_core::{ServerId, UserId};

/// Where an extra control message came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtlSource {
    /// This device is a member of `server`; the message came from its host.
    FromHost {
        /// The server.
        server: ServerId,
        /// The host.
        host: UserId,
    },
    /// This device hosts `server`; the message came from a connected member.
    FromMember {
        /// The server.
        server: ServerId,
        /// The member (authenticated QUIC peer).
        member: UserId,
        /// The member's short id.
        short_id: u16,
    },
    /// The message came over the DM connection with `peer`.
    Dm {
        /// The other user.
        peer: UserId,
    },
}

/// Observes media and call control messages after the media layer handled them.
///
/// Called on the session or DM actor task: it must not block (hand work to a channel).
pub trait ExtraCtlHandler: Send + Sync + 'static {
    /// Handle one message.
    fn handle(&self, source: CtlSource, ctl: Ctl);
}

impl<F> ExtraCtlHandler for F
where
    F: Fn(CtlSource, Ctl) + Send + Sync + 'static,
{
    fn handle(&self, source: CtlSource, ctl: Ctl) { self(source, ctl) }
}

/// True for the `Ctl` variants routed to the media layer (and the [`ExtraCtlHandler`]). They
/// do not count as liveness: a host or member is alive when its session actor talks.
pub fn is_extra(ctl: &Ctl) -> bool {
    matches!(
        ctl,
        Ctl::VoiceJoin { .. }
            | Ctl::VoiceLeave
            | Ctl::Subscribe { .. }
            | Ctl::Unsubscribe { .. }
            | Ctl::KeyframeRequest { .. }
            | Ctl::TargetBitrate { .. }
            | Ctl::ReceiverReport { .. }
            | Ctl::SoundPlayed { .. }
            | Ctl::CallInvite { .. }
            | Ctl::CallAccept { .. }
            | Ctl::CallDecline { .. }
            | Ctl::CallEnd { .. }
            | Ctl::CallState { .. }
    )
}
