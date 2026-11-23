//! Network configuration and endpoint construction.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Duration;

use iroh::endpoint::{IdleTimeout, QuicTransportConfig, presets};
use iroh::{Endpoint, EndpointId, RelayMode, RelayUrl};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use parking_lot::Mutex;
use tincan_core::keys::Identity;

use crate::error::{NetError, Result};

/// mDNS service name shared by every Tincan instance (`_tincan1._udp.local`).
pub const MDNS_SERVICE_NAME: &str = "tincan1";
/// mDNS query cadence.
pub const MDNS_CADENCE: Duration = Duration::from_secs(5);

/// Which iroh relays to use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RelayChoice {
    /// n0's public relays (development and hobby tier).
    #[default]
    N0Default,
    /// Custom relay URLs (for example a self-hosted `iroh-relay`).
    Custom(Vec<String>),
    /// No relays: direct connections only (LAN, tests). Global address lookup is off too.
    Disabled,
}

/// Configuration of the network layer.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Relay selection.
    pub relay: RelayChoice,
    /// LAN discovery via mDNS (`service_name = "tincan1"`, cadence 5 s).
    pub mdns: bool,
    /// App version reported in `Status`.
    pub app_version: String,
    /// "Allow hosting" setting. When false this device never becomes host on its own
    /// (it still hosts a server it just created) and is excluded from succession.
    pub allow_hosting: bool,
    /// Bind the IPv4 socket to this address instead of `0.0.0.0:0` (tests use `127.0.0.1:0`).
    /// An IPv6 address replaces the IPv6 default socket instead.
    pub bind_addr: Option<SocketAddr>,
    /// The local user's display name, used for the owner Profile of new servers and the
    /// Profile event of a join. Change it with `Net::set_display_name`.
    pub display_name: String,
    /// The local user's avatar, announced to DM contacts (`Ctl::DmAvatar`). Change it with
    /// `Net::set_avatar`. Servers learn it from the `ProfileImage` event the app publishes.
    pub avatar: Option<tincan_core::event::ImageRef>,
    /// Publish and resolve the rendezvous record (pkarr relay and mainline DHT). Always off
    /// when `relay` is `RelayChoice::Disabled`, so tests and LAN-only setups make no
    /// rendezvous traffic at all.
    pub rendezvous: bool,
    /// Folder that stands in for the DHT for short code records (invite and friend codes)
    /// when the rendezvous layer is off: offline test mode and tests, where every profile on
    /// the machine shares it. `None` with the rendezvous layer off: short codes are
    /// unavailable and only full invites and IDs work.
    pub code_dir: Option<std::path::PathBuf>,
}

impl NetConfig {
    /// True if the rendezvous record layer runs (`rendezvous` set and relays not disabled).
    pub fn rendezvous_enabled(&self) -> bool { self.rendezvous && self.relay != RelayChoice::Disabled }
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            relay: RelayChoice::N0Default,
            mdns: true,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            allow_hosting: true,
            bind_addr: None,
            display_name: "Anonymous".to_string(),
            avatar: None,
            rendezvous: true,
            code_dir: None,
        }
    }
}

/// Data a peer may have in flight towards us per connection (all streams together). The
/// QUIC default is unlimited, which let a peer park 100 streams x 1.25 MB in streams the
/// app never reads.
pub const RECEIVE_WINDOW: u32 = 16 * 1024 * 1024;
/// Per-stream receive window.
pub const STREAM_RECEIVE_WINDOW: u32 = 1024 * 1024;
/// Concurrent bidi streams a peer may open (control stream, blob requests).
pub const MAX_BIDI_STREAMS: u32 = 8;
/// Concurrent uni streams a peer may open (one per video group and track).
pub const MAX_UNI_STREAMS: u32 = 16;

/// The media-tuned QUIC transport config from docs/architecture.md "Connections":
/// idle timeout 10 s, keep-alive 2 s, 16 KiB datagram send buffer, 256 KiB receive buffer,
/// default congestion controller, plus bounded receive windows and stream counts. Applied
/// endpoint-wide (dial and accept, blobs connections included).
pub fn transport_config() -> QuicTransportConfig {
    use iroh::endpoint::VarInt;
    let mut builder = QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_secs(2))
        .datagram_send_buffer_size(16 * 1024)
        .datagram_receive_buffer_size(Some(256 * 1024))
        .receive_window(VarInt::from_u32(RECEIVE_WINDOW))
        .stream_receive_window(VarInt::from_u32(STREAM_RECEIVE_WINDOW))
        .max_concurrent_bidi_streams(VarInt::from_u32(MAX_BIDI_STREAMS))
        .max_concurrent_uni_streams(VarInt::from_u32(MAX_UNI_STREAMS));
    match IdleTimeout::try_from(Duration::from_secs(10)) {
        Ok(t) => builder = builder.max_idle_timeout(Some(t)),
        Err(e) => tracing::warn!(error = %e, "idle timeout out of range; using the iroh default"),
    }
    builder.build()
}

/// Build and bind the iroh endpoint for `identity`.
///
/// - Relays per `cfg.relay`: `presets::N0` (n0 relays, n0 DNS address lookup and pkarr
///   publishing) for `N0Default` and `Custom`, `presets::Minimal` plus `RelayMode::Disabled`
///   for `Disabled`.
/// - The transport config from [`transport_config`].
/// - mDNS LAN lookup when `cfg.mdns` (service `"tincan1"`, cadence 5 s). `Net::start`
///   subscribes to it to dial members found on the LAN.
///
/// ALPNs are not set here: `Net::start` spawns the Router, which registers them.
pub async fn build_endpoint(identity: &Identity, cfg: &NetConfig) -> Result<Endpoint> {
    let mut builder = match &cfg.relay {
        RelayChoice::Disabled => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
        RelayChoice::N0Default => Endpoint::builder(presets::N0),
        RelayChoice::Custom(urls) => {
            let mut parsed = Vec::with_capacity(urls.len());
            for u in urls {
                let url: RelayUrl = u
                    .trim()
                    .parse()
                    .map_err(|_| NetError::Invalid(format!("\"{u}\" is not a valid relay URL.")))?;
                parsed.push(url);
            }
            if parsed.is_empty() {
                return Err(NetError::Invalid("Enter at least one relay URL.".into()));
            }
            Endpoint::builder(presets::N0).relay_mode(RelayMode::custom(parsed))
        }
    };
    builder = builder.secret_key(identity.secret().clone()).transport_config(transport_config());
    if let Some(addr) = cfg.bind_addr {
        builder = builder.bind_addr(addr).map_err(|e| NetError::Endpoint(format!("bad bind address {addr}: {e}")))?;
    }
    let endpoint = builder.bind().await.map_err(|e| NetError::Endpoint(e.to_string()))?;
    if cfg.mdns {
        match MdnsAddressLookup::builder()
            .service_name(MDNS_SERVICE_NAME)
            .discovery_cadence(MDNS_CADENCE)
            .build(endpoint.id())
        {
            Ok(mdns) => match endpoint.address_lookup() {
                Ok(services) => {
                    services.add(mdns.clone());
                    mdns_registry().lock().insert(endpoint.id(), mdns);
                }
                Err(e) => tracing::warn!(error = %e, "endpoint has no address lookup; mDNS disabled"),
            },
            Err(e) => tracing::warn!(error = %e, "mDNS unavailable on this network"),
        }
    }
    Ok(endpoint)
}

fn mdns_registry() -> &'static Mutex<HashMap<EndpointId, MdnsAddressLookup>> {
    static REGISTRY: OnceLock<Mutex<HashMap<EndpointId, MdnsAddressLookup>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take the mDNS service `build_endpoint` attached to `id`, if any.
pub(crate) fn take_mdns(id: &EndpointId) -> Option<MdnsAddressLookup> { mdns_registry().lock().remove(id) }
