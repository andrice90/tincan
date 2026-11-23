//! Tincan core: shared types and pure protocol logic. No IO, no async, no threads.
//!
//! Every other crate depends on this one and only this one (internally). The
//! contract between crates lives here; see `docs/architecture.md` for the
//! protocol it implements and `docs/notes-tincan-core.md` for integration notes.
#![warn(missing_docs)]

pub mod build;
pub mod code;
pub mod election;
pub mod error;
pub mod event;
pub mod hlc;
pub mod ids;
pub mod image;
pub mod invite;
pub mod keys;
pub mod media;
pub mod rdv;
pub mod state;
pub mod storage;
pub mod sync;
pub mod wire;

pub use error::{CoreError, Result};
pub use ids::{BlobHash, ChannelId, ConvId, EventId, ServerId, UserId};

/// Protocol version carried in events, invites and records.
pub const PROTOCOL_VERSION: u8 = 1;

/// ALPN for member <-> host connections of a server.
pub const ALPN_SERVER: &[u8] = b"tincan/server/1";
/// ALPN for direct-message connections between two users.
pub const ALPN_DM: &[u8] = b"tincan/dm/1";

/// QUIC application close code a host uses when it leaves on purpose.
pub const CLOSE_HOST_LEAVING: u32 = 1;
/// QUIC application close code for "you were redirected / not the host".
pub const CLOSE_REDIRECT: u32 = 2;
/// QUIC application close code for a rejected peer (not a member, bad invite).
pub const CLOSE_REJECTED: u32 = 3;
/// QUIC application close code for a normal client disconnect.
pub const CLOSE_NORMAL: u32 = 0;

/// Current unix time in milliseconds (0 if the system clock is before 1970).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_is_plausible() {
        // After 2024-01-01 and before 2200-01-01.
        let t = now_ms();
        assert!(t > 1_704_067_200_000);
        assert!(t < 7_258_118_400_000);
    }

    #[test]
    fn alpns_are_distinct() {
        assert_ne!(ALPN_SERVER, ALPN_DM);
    }
}
