# iroh

## Summary
iroh 1.0 shipped on 2026-06-15. The current release is iroh 1.5.0 (2026-11-19), and its MSRV is 1.91. The compatible set as of today is iroh 1.5.0, iroh-blobs =0.105.0 (2026-11-13), iroh-gossip =0.103.0 (2026-11-16), iroh-mdns-address-lookup =0.6.1 and iroh-mainline-address-lookup =0.6.1. All of them resolve to a single iroh 1.5.0 in one dependency tree, which I confirmed with a real build. iroh-docs 0.101.0 has not been updated since June. It pins iroh-blobs ^0.103 and iroh-gossip ^0.101, so it cannot share a tree with blobs 0.105.

Several pre-1.0 renames are now final:
- Node became Endpoint in 0.94, so NodeId is now EndpointId and NodeAddr is now EndpointAddr.
- Discovery became AddressLookup in 0.96.
- quinn was replaced by noq, n0's QUIC with multipath and NAT traversal, in 0.97.
- ProtocolHandler moved to async fns, and AcceptError replaced ProtocolError, in 0.90.
- The mDNS and DHT lookups are no longer features. In 1.x they are separate crates (they were `discovery-local-network`/`discovery-pkarr-dht` in 0.95 and `address-lookup-mdns`/`address-lookup-pkarr-dht` in 0.96-0.98).

I built a spike in the scratch folder, compiled it in dev, release and size-optimised profiles, and ran it on this machine. What it showed:
- Endpoint with a persistent SecretKey, plus a Router carrying a custom ALPN handler and the iroh-blobs protocol: works.
- Connecting by EndpointId only, through n0 DNS with mDNS turned off: works. The connection started on the relay and moved to a direct path.
- Datagrams are on by default. `max_datagram_size()` returned 1162 at the start of a direct connection.
- RTT and direct-vs-relay come from `conn.paths()`.
- An FsStore file import with TryReference works.
- A 1 MiB range request works. Reading past the downloaded range returns an error.
- With providers [dead, seeder], the download stalled for exactly the pool connect_timeout on the dead one, then finished from the seeder.
- A peer holding part of a file serves most of what it has, after which the downloader moves on to the next provider.
- A fresh peer completed the download with the original uploader offline, using a partial holder and a complete holder.
- After a clean restart the download resumed and fetched only the remainder. A hard kill about 0.1 s into a download lost all progress. A hard kill 3 s after a range finished kept it.

The size-optimised exe is 6.85 MiB, against 20.8 MiB for a default release build. Idle CPU is about 0.1 s per 90 s with 24-30 MB working set and no peer connected. FsStore starts its own tokio runtime with one thread per core: 54-58 threads on this 24-core machine, 11 with TOKIO_WORKER_THREADS=2.

The main caveats:
- The iroh-blobs README says this release line is "not yet considered production quality". Its suggested alternative, 0.35, needs iroh 0.35, which loses public relay support on 2026-12-31.
- n0's public relays and DNS are for "development and hobby use only". Only the latest stable iroh is officially supported on them, rate limits are not published, and there is no SLA.
- The blobs protocol has no content discovery, so Tincan must track who holds each hash.
- Everything was tested on one machine, so WAN hole punching, relay throughput and datagram size over a relay are unverified.

## Recommendation
Use iroh 1.5.x as Tincan's only transport. Pin it as iroh = "1.5" and pin the 0.x crates exactly: iroh-blobs = "=0.105.0", iroh-mdns-address-lookup = "=0.6.1". Run one Endpoint and one Router per app instance with two ALPNs:
- `tincan/control/1`: Tincan's own protocol for chat, presence, host election and SFU-style media forwarding. Send media as QUIC datagrams of about 1100 B or less (larger video frames go on short-lived uni streams), and control traffic on bi streams.
- `iroh_blobs::ALPN`, for attachments.

Endpoint setup:
- Build it with `presets::N0`, `SecretKey::from_bytes` loaded from %APPDATA%, and `MdnsAddressLookup::builder()` for LAN peers.
- Leave out the Mainline DHT lookup at first. It only matters if n0 DNS becomes a problem, and it is one line to add later.
- Lower `max_idle_timeout` to about 10 s and keep-alive to about 2 s so a lost host is detected in seconds; the defaults are 30 s and 5 s. Host migration can then key off `conn.closed()`.
- Score host stability from `conn.paths()`: prefer a selected direct (`is_ip`) path, low `rtt()`, and uptime.

Skip iroh-gossip: the host-star design does not need epidemic broadcast, and its default 4 KiB messages add little. Skip iroh-docs: it is stale and incompatible with blobs 0.105.

Blobs:
- Use a persistent `FsStore` with `ImportMode::TryReference` and named tags.
- The host keeps a holders list per hash and puts it in each attachment message. The downloader is given only the providers currently known to be online.
- Set the pool `connect_timeout` to 3-5 s; the default is 1 s, and a dead id stalls the queue for exactly that long.
- When the downloader returns "Unable to download", re-queue the download whenever a holder comes online.
- Always call `router.shutdown()` on exit, so partial progress survives.
- For video, build range-priority fetching plus a Tauri custom-protocol Range server yourself. It is not built in.
- Hide iroh-blobs behind a small internal trait, because the line is declared not production quality and has breaking releases every few weeks.

Resources:
- Set TOKIO_WORKER_THREADS (for example 2-4) before any runtime starts, or FsStore spawns one thread per core.
- Ship a size-optimised profile (opt-level "s", fat LTO, codegen-units 1, panic abort, strip). It takes the networking core to about 7 MB.
- Have the installer add a Windows Firewall rule, so users do not get a prompt on first run.

Relays: develop on the n0 public relays. Before any public release, run your own `iroh-relay` on a small VPS with Let's Encrypt and `access.shared_token`, and point clients at it with `RelayMode::Custom` and `RelayConfig.auth_token`. The alternative is n0 Pro at $19/mo. This is the one piece of infrastructure that breaks "nobody runs a server". The reasons are that public relays are dev/hobby-only, rate-limited, and officially support only the latest iroh. Also plan an invite format that carries relay URL plus direct addresses (EndpointAddr). That reduces dependence on n0 DNS.

## Versions
- iroh 1.5.0 latest, 2026-11-19; 1.0.0 released 2026-06-15; MSRV 1.91; default features metrics, fast-apple-datapath, portmapper, tls-ring
- iroh-base 1.5.0 EndpointId/PublicKey, SecretKey, EndpointAddr, RelayUrl
- iroh-relay 1.5.0 relay client and server; build the binary with --features server
- noq / noq-proto / noq-udp 1.5.0 n0's QUIC (quinn fork) with multipath and NAT traversal; replaced quinn in iroh 0.97
- iroh-blobs 0.105.0 2026-11-13; requires iroh ^1.0, irpc ^1.0; self-declared not production quality; pin with =
- iroh-gossip 0.103.0 2026-11-16; requires iroh ^1.4; not recommended for Tincan
- iroh-docs 0.101.0 2026-06-15; pins iroh-blobs ^0.103 and iroh-gossip ^0.101, so it is incompatible with blobs 0.105
- iroh-mdns-address-lookup 0.6.1 2026-10-20; LAN lookup; replaces the old discovery-local-network / address-lookup-mdns features
- iroh-mainline-address-lookup 0.6.1 2026-10-20; Mainline DHT lookup; replaces discovery-pkarr-dht / address-lookup-pkarr-dht
- iroh-util 0.7.0 provides connection_pool, re-exported as iroh_blobs::util::connection_pool (connect_timeout 1 s by default)
- iroh-tickets 1.0.0 EndpointTicket / BlobTicket encoding
- irpc 1.0.0 used internally by iroh-blobs 0.105
- bao-tree 0.17.0 BLAKE3 verified streaming used by iroh-blobs
- tokio 1.54.1 resolved version; honors TOKIO_WORKER_THREADS
- rustc / cargo 1.99.1 MSVC toolchain used for all builds

## Risks
- n0's public relays are for 'development and hobby use only', with no SLA and unpublished, changeable rate limits. 'Only the latest stable release of iroh is officially supported' on them. Shipping on them means forced iroh upgrades and possible throttling of relayed voice or screen share. Relayed traffic is all traffic between peers that cannot hole-punch, common behind CGNAT or symmetric NAT. Production needs a self-hosted iroh-relay or a paid plan, which conflicts with the 'nobody runs a server' goal.
- Connecting by id alone depends on n0's DNS/pkarr service (dns.iroh.link), which is also shared n0 infrastructure. Mitigations: invites carrying a full EndpointAddr (relay URL plus direct addresses), MemoryLookup filled from the host's member list, mDNS on LAN, and the Mainline DHT lookup as a fallback.
- The iroh-blobs 0.10x line is self-declared 'not yet considered production quality', and 0.105.0 alone had about 8 breaking changes. Its suggested production alternative, 0.35, needs iroh 0.35, which loses public relays on 2026-12-31, so there is no stable blobs option on iroh 1.x.
- Every result was measured on one Windows machine. Hole punching across real NATs, data carried by a relay, WAN dial times, relay rate limits and datagram size over a relay path are all unverified. Note too that 1162 B was the max datagram size at the start of a direct connection.
- The Downloader is sequential and gives up once the provider list is exhausted. An offline provider costs a full connect_timeout. How quickly it fails over when a provider silently vanishes mid-stream was not measured; the QUIC idle timeout (30 s default) presumably bounds it. Tincan must track holders per hash, give only providers known to be online, and re-queue failed downloads.
- There is no content discovery and no 'which ranges do you have' query, so a partial holder is only discovered by trying it. A partial holder delivered most but not all of its data (3984 of 4096 chunks).
- Durability of partial downloads: a hard crash shortly after data arrives loses the recent progress (about 0.1 s in, everything was lost). Always shut down the Router and store cleanly, or call store.sync_db() periodically.
- FsStore spawns its own tokio runtime with one worker per core (54-58 threads per process on 24 cores). The only knob is the process-wide TOKIO_WORKER_THREADS variable.
- The default ImportMode::Copy probably duplicates attachments on NTFS, which has no reflinks (inferred from the source). TryReference avoids the copy, but the original file must not be modified or moved, or later serving fails or produces invalid data.
- Windows Defender Firewall creates inbound rules per exe path. The spike runs left inbound Allow rules for the scratch iroh-transport.exe builds; the partial.exe and resume.exe examples were not checked. These are left for the owner to remove. Tincan's installer should add a rule so users are not prompted, or are prompted only once.
- Streaming video while it downloads is not built in. BlobReader errors on missing ranges instead of waiting, so playhead-priority range requests and a waiting reader or Range-serving custom protocol must be built on top.
- iroh-docs is stale (June 2026) and pins older blobs and gossip, so it cannot be used for chat-history sync alongside blobs 0.105.
- A crates.io crate named 'tincan-chat' (a P2P voice/text terminal chat) already exists, so the name overlaps.
- Release cadence is fast: iroh minors arrive about monthly, iroh-blobs breaking releases every 1-3 weeks, and n0 moves public relays to the latest version within about 24 h. Expect regular dependency maintenance.

## Open
- What throughput and latency do voice and screen-share datagrams get through a relay (n0 public versus self-hosted), and what is max_datagram_size() on a relay-only path?
- What are the actual public relay rate limits (unpublished), and do they throttle a 2-5 Mbps screen share relayed through them?
- How long does the downloader take to fail over when a provider disappears mid-transfer (network unplugged rather than a clean close)? Can it be shortened with QuicTransportConfig on the downloader's endpoint?
- How fast is hole punching across two residential NATs on Windows, and what fraction of pairs stay relayed?
- Does n0's DNS/pkarr service (dns.iroh.link) have usage limits or terms for production apps separate from the relays? Should Tincan self-host iroh-dns-server, or rely on invites carrying an EndpointAddr plus the DHT?
- What is the RAM and CPU cost per open connection, for a host serving 10-25 members with live voice? This was not measured; the idle numbers had no peers.
- Does iroh-blobs' BlobReader or export_ranges offer a way to wait for a range to arrive (for example via observe() streams), or must Tincan poll the bitfield?
- Does the WebView2 app plus a Tauri tokio runtime capped by TOKIO_WORKER_THREADS behave well, given that the variable also caps Tauri's async runtime?
