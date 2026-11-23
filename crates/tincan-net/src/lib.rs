//! tincan-net: the iroh endpoint, server sessions and direct messages.
//!
//! - [`build_endpoint`] binds the iroh endpoint (relays, media-tuned transport, mDNS).
//! - [`Net::start`] spawns the router (`tincan/server/1`, `tincan/dm/1`, plus whatever the
//!   app registers, e.g. blobs), loads every stored conversation and runs one session actor
//!   per server: cold open, member and host roles, history sync, publish/fan-out, roster,
//!   liveness ticks, clean handoff, crash failover by succession, proactive migration and
//!   partition merge (docs/architecture.md). DMs connect on demand with an outbox retried on
//!   a backoff.
//! - Events for the app arrive on [`Net::subscribe`] as [`NetEvent`]s.
//! - Rendezvous: the host publishes an encrypted pkarr record (n0 pkarr relay and mainline
//!   DHT) that cold joiners and cold-opening members resolve in parallel with their dials.
//! - Media: voice rooms with a forwarding host SFU, screen share as one QUIC stream per GOP,
//!   soundboard announcements, and 1:1 calls over the DM connection ([`Net::voice_join`],
//!   [`Net::share_start`], [`Net::call_start`], [`MediaSession`]). Other code can still tap
//!   media control messages through [`hooks`]; see `docs/notes-tincan-net.md`.
#![warn(missing_docs)]

mod addr;
mod config;
mod dm;
mod error;
mod events;
pub mod hooks;
mod inner;
mod limits;
mod link;
mod media;
mod net;
mod power;
mod proto;
mod rdv;
mod replica;
mod session;
mod timing;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_media;
#[cfg(test)]
mod tests_security;

pub use config::{MDNS_CADENCE, MDNS_SERVICE_NAME, NetConfig, RelayChoice, build_endpoint, transport_config};
pub use dm::DmHold;
pub use error::{NetError, Result};
pub use events::{ConnState, ConnectionInfo, NetEvent, PathKind};
pub use hooks::{CtlSource, ExtraCtlHandler};
pub use media::{
    AUDIO_IN_QUEUE, AUDIO_OUT_QUEUE, CallEnd, CallInfo, CallState, HOST_UPLINK_BUDGET_BPS, MediaSession, RELAYED_SHARER_BPS,
    RELAYED_VIEWER_BPS, SHARE_QUEUE, VIDEO_IN_QUEUE, VIDEO_MAX_BPS, VIDEO_MIN_BPS, VoiceInfo,
};
pub use net::Net;
