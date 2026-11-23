# protocol

## Summary
Tincan's group layer should be a grow-only log of author-signed events, one log per server, and every member stores all of it. The host only fans out events and collects presence. It never decides message order or validity. Order is (HLC, event id). Validity comes from the signature, a contiguous per-author seq, and membership intervals computed from signed admin events. Under that model host election can be cheap and imperfect: a split brain costs a voice interruption and a version-vector resync, never a forked history.

Prior art points the same way:
- **Cwtch Managed Groups:** the closest match. A manager stores and relays signed messages numbered per member, and a reconnecting member sends its last-known counter for each member.
- **Jami swarms:** git history with commit validation. Their docs list ban ordering under partitions and untrusted timestamps as unsolved.
- **Keet/Autobase:** a causal DAG with an indexer quorum. More machinery than Tincan needs.
- **Berty:** rotating rendezvous points, and replication nodes that cannot decrypt.
- **Quiet:** moved to a signed sigchain in 2025; release 9.0.2 fixed unsigned changes that let members forge them. The auth library it adopted (local-first-web/auth, now archived) uses Seitan invites that any member can admit.
- **Photon, Unity, PlayFab:** the host names a successor, a deterministic fallback covers the rest, and the host holds no unique state.

Rendezvous needs no servers we run. A throwaway probe compiled against iroh 1.5.0 and n0-mainline 0.8.0 on this machine (2026-11-23 UTC) measured:
- One encrypted pkarr record per server, carrying 12 online ids and the host relay URL, is 889 B.
- It resolved from n0's pkarr relay in about 146 ms.
- From mainline DHT a cold client got the first item in 0.44 to 1.2 s. A full query took about 3 s, and one of three full queries missed.
- The record was still there 20.5 minutes later with no republish.
- A DHT put took about 3.75 s including bootstrap. A writer whose clock runs fast blocks later writes with NotMostRecent; writing seq = seen+1 with cas works.
- Dialing a peer that went offline hangs 30 s, so a 4 s app-level dial timeout is mandatory.
- Recommended layers: cached-peer dials, LAN mDNS, and the record. A user who reaches no one in 6 s becomes a solo host.

Election works like this:
- The host ranks a succession list and broadcasts it in the roster.
- Signed HostClaims carry epoch numbers.
- Ticks every 2 s; the host counts as dead after 10 s.
- Each member keeps a warm standby connection to the first successor, and lower-ranked candidates wait k x 3 s before taking over.
- Hysteresis keeps the host from flapping. Two partitions merge by comparing connected count, then epoch, score and id.

Sync uses per-author range vectors, about 5 KB for 100 authors. Measured negentropy 0.5.3 is fine for gaps at the end and clustered gaps but poor for scattered ones, so it is deferred.

Security:
- UserId is the iroh EndpointId.
- Invites follow the Seitan pattern and the link never carries the server secret.
- Admin is owner-rooted, with last_seq cutoffs for kicks, bans and demotions.
- No extra end-to-end encryption for group content in v1, and the blob provider rejects non-members.
- DMs are sealed with a separate X25519 prekey. Encrypt and decrypt were tested.

DMs use a direct iroh connection with the same log format and an outbox. Store-and-forward mailboxes are deferred to v1.5.

## Recommendation
TINCAN GROUP-STATE PROTOCOL, V1

Core decision: the host is not an authority. A server's state is a grow-only set of signed events.
- Display order is (hlc, event_id).
- Validity rules are deterministic, and every member evaluates them locally.
- The host only fans out events, aggregates presence and voice state, and runs the voice/screen SFU.
- Mistakes in host election therefore cost only presence and voice continuity. Histories merge by set union.

1) IDENTITY AND KEYS
- UserId = the iroh EndpointId, an ed25519 public key, encoded z-base-32 (52 characters) for display and links. V1 allows one device per identity. Store the key under %APPDATA%\Tincan, protected with DPAPI.
- Each user has a separate X25519 dm_prekey, published inside their signed Profile event. Do not reuse the ed25519 key for Diffie-Hellman; the ed25519-dalek 3 docs warn against it.
- ServerId = BLAKE3 of the owner-signed Genesis event. This follows Jami, where the conversation id is the first commit's hash.
- server_secret = 32 random bytes known to all members. Derive sub-keys with blake3::derive_key(context, server_secret || server_id):
  - "tincan v1 rendezvous signing key": an ed25519 seed, giving rdv_sk and rdv_pk.
  - "tincan v1 rendezvous record encryption key": rdv_read_key.
  - "tincan v1 hello mac key": hello_key.

2) INVITES (Seitan pattern from local-first-web/auth; any member can admit)
- An InviteCreate{invite_pk, expires_ms, max_uses} event goes into the log. invite_sk = SecretKey::from_bytes(derive_key("tincan v1 invite key", seed16)).
- Link: tincan://join/<base32(postcard(Invite))>, about 300 characters, plus a QR code. The Invite holds v, server_id, name_hint, invite_seed (16 bytes), rdv_pk, rdv_read_key, up to 2 bootstrap UserIds (the inviter and the first successor) and expires_ms.
- The link never contains server_secret. Before admission a joiner can find who is online but cannot write the rendezvous record or pass the Hello MAC.
- Join flow:
  - The joiner dials the bootstrap peers and online peers.
  - A non-host replies with Redirect.
  - The joiner sends the host JoinRequest. invite_sig signs "tincan-join1" || server || joiner || host_endpoint || next_seq || ts_ms, which binds the request to that host and blocks replay.
  - The host checks: invite present, not revoked or expired, uses below max_uses (best effort across partitions), joiner not banned.
  - The host authors MemberAdmit{user, invite_pk, from_seq} and returns JoinAccepted{server_secret, genesis, admit}.
  - Normal sync follows.
- Defer short codes such as ABCD-EFGH-JKLM. They could be done with a DHT mutable item keyed from the code, but an online member would have to republish it.

3) RENDEZVOUS (layered; timings from today's measurements)
Record:
- RdvRecord{v, epoch, host, host_relay, published_ms, online: up to 12 UserIds in succession order}.
- Encoding: postcard, sealed with XChaCha20-Poly1305 under rdv_read_key (24-byte nonce prefix), base64url, then split into 240-character TXT strings under "_tincan" named "i=<chunk>".
- Wrapped as an iroh_dns::pkarr::SignedPacket signed by rdv_sk. Measured 889 B against the 1104 B maximum.
- Published two ways:
  - n0 pkarr relay: PUT https://dns.iroh.link/pkarr/<z32>. This is an optional accelerator; non-_iroh records are off-label there.
  - Mainline BEP44: the same signed bytes as a MutableItem with no salt.
- Write rule: a publisher first resolves, then uses seq = max(now_us, seen_seq + 1) and passes cas = seen_seq. Re-signing with an explicit seq is shown in the API notes and was checked. If it skips this, a host with a fast clock blocks every later write until real time catches up (NotMostRecent was observed).
- Clobber rule: before publishing, if the resolved record names a different host with epoch >= mine and published_ms is under 15 minutes old, dial that host first (4 s timeout). Publish only if it is unreachable or loses the merge comparison.
- The record is only a hint. Everyone holding server_secret can overwrite it, kicked members included. Dialers must verify the host through Hello, a SignedHostClaim, and the member and ban lists.

Cold open of a server:
- T+0, in parallel:
  - L1, cache: dial the last host, the succession list and up to 6 most-recently-seen members. Use cached EndpointAddr values (relay URL plus direct addresses), stagger 150 ms, and time each dial out at 4 s; a dial to a stale record otherwise hangs 30 s (measured). Any member answers Welcome or Redirect.
  - L2, LAN: iroh-mdns-address-lookup with service_name "tincan1" and discovery_cadence 5 s (default is 700 ms). Run it only while a server is open, and intersect discovered ids with the member set.
  - L3, record: n0 relay GET (about 150 ms) and DHT get_mutable, taking the first valid item (about 0.5 to 1.2 s cold), then draining up to 3 s for a higher seq. One DHT miss does not mean nobody is online; retry once.
- T+6 s with nobody reached: become solo host at epoch = last_seen_epoch + 1 and publish the record.
- While solo: re-dial up to 6 cached peers and re-resolve the record every 60 s, backing off to 5 minutes after 30 minutes alone.
- Host duties:
  - Publish when becoming host and on roster changes (30 s debounce, at least 10 s apart; n0's relay limits PUTs per IP to roughly 1 per 4 s with a burst of 2).
  - Refresh the relay copy every 5 minutes and the DHT copy every 30 minutes. BEP44 items may expire after 2 hours; ours survived at least 20 minutes.
  - Re-resolve the record every 5 minutes to detect a foreign host after a partition.
- Use an ephemeral DHT client: create it, bootstrap (about 3.5 s), do the put or get, then drop it.
- Rejected options:
  - (b) Mainline announce_peer/get_peers: publishes IP:port in plain sight and returns no EndpointId. n0's iroh-content-discovery needs an extra udp-addr-index server to map addresses to endpoints.
  - (c) iroh-gossip as the rendezvous: it still needs one reachable bootstrap peer, and it builds a HyParView mesh (active 5, passive 30) that duplicates the star. Delta Chat uses it with tickets carried in chat messages.

4) HOST ELECTION AND MIGRATION
- Members send Status{hosting_ok, ac_power, battery_saver, metered, nat, uptime_s, avg_session_s (14-day average), relay_rtt_ms, synced, voice} on change and every 30 s. Sources: GetSystemPowerStatus ACLineStatus and SystemStatusFlag; ConnectionProfile.GetConnectionCost(), where NetworkCostType is Fixed or Variable or Roaming is set; iroh net_report.
- The host adds its own per-member observations (PathInfo.is_ip(), rtt()) and broadcasts Roster{epoch, rev, online, succession up to 4}. It sends deltas on change (250 ms debounce) and a full roster every 5 minutes.
- Peers never need to compute identical scores; they follow the host's list. Fallback when no list exists: the lowest UserId among reachable members.
- Ineligible to host: !hosting_ok, battery_saver, metered, or not synced.
- Score for eligible members, tie broken by the lower UserId:
  - 400 x tier. Tier 2 = global_v6, or udp_v4 with mapping_varies_by_dest_ipv4 == Some(false) plus an observed direct path. Tier 1 = direct path observed. Tier 0 = relay only.
  - + 200 x ac_power
  - + 200 x min(uptime, 2 h) / 2 h
  - + 150 x min(avg_session, 8 h) / 8 h
  - + 50 x (1 - min(relay_rtt, 300 ms) / 300 ms)
- Epochs: SignedHostClaim{server, epoch, host, prev, issued_ms}, signed by the host's key.
- A member follows a new claim only if it comes from its current host (HostMoved or a merge result), or the member has lost its host. Unsolicited higher-epoch claims are ignored. This stops one flaky successor from stealing the server.
- Liveness:
  - Tick every 2 s in both directions.
  - Suspect the host at 6 s of silence; declare it dead at 10 s.
  - A QUIC close with app code HOST_LEAVING means immediate switch.
  - iroh's defaults alone are too slow: keepalive 5 s, path idle 15 s direct and 30 s relayed, connection idle 30 s.
- Crash failover:
  - Every member keeps one idle standby connection to S1, the first successor.
  - S1 watches the host too. When the host is dead, S1 promotes itself to epoch E+1 and pushes HostClaim over the standby connections.
  - Members that cannot reach S1 within 3 s try S2, and so on. S_k promotes itself only after k x 3 s without reaching S1 through S_(k-1).
  - Estimated text outage is about 10 to 13 s. Voice reconnects after that.
- Clean handoff:
  - Host sends Handoff{claim(E+1, S1), roster}.
  - S1 replies HandoffAck within 2 s.
  - Host broadcasts HostMoved, then closes.
  - Triggers: app exit, PBT_APMSUSPEND (sleep), "stop hosting".
  - Estimated well under 1 s for text; the 0.36 s connect was measured on one machine.
- Hysteresis:
  - Never migrate just because a better member joined.
  - Proactive migration only when (a) the host becomes ineligible (60 s grace, then wait up to 10 minutes for voice to end), or (b) the best candidate's score is at least 30% higher continuously for 10 minutes, no voice is active, and at least 30 minutes have passed since the last migration.
- Partition merge:
  - Triggers: the record names a foreign host, a Redirect, or a cache dial reaches a member under another host.
  - The two hosts exchange HostCompare{claim, connected, score}. Both pick the winner by max(connected, epoch, score, -UserId).
  - The loser sends HostMoved(epoch = max + 1, winner) to its members and joins as a member. The winner bumps its epoch.
  - Everyone then runs version-vector sync with the winner.

5) REPLICATED HISTORY
- Event{v, server, author, seq, hlc, body}. seq is per author per server and contiguous from 1. hlc is 48-bit Unix milliseconds shifted left 16, OR a 16-bit logical counter (Kulkarni et al. 2014).
- Signature: ed25519 over "tincan-ev1" || postcard(Event). Event id = BLAKE3 of the encoded event.
- Validation:
  - seq and hlc must increase strictly per author.
  - Buffer events with hlc more than 5 minutes in the future.
  - The author must be in an admitted interval [MemberAdmit.from_seq, Kick/Ban/Leave.last_seq].
  - Admin authority uses the same cutoff: RoleSet{admin: false, last_seq}. A demoted admin's events with seq > last_seq carry no authority, which closes the backdating hole Jami flags.
  - Only the owner sets roles. Admins can kick or ban non-admins only, so admins cannot remove each other (avoids Jami's conflicting bans).
- Edits: only the target's author can edit; the last edit by (hlc, id) wins.
- Delete: by the author or an admin. Leaves a tombstone and purges the body locally.
- Equivocation: same (author, seq) with a different id. Keep the lower id and flag the author. After a reinstall, a client learns your_next_seq from Welcome before writing.
- Sync:
  - On connect both sides send SyncHave{per-author range sets}, about 40 to 50 B per author (about 5 KB for 100).
  - Each side streams what the other lacks, newest-first per channel, in pages of at most 256 KiB. A new joiner gets the latest 200 messages per channel first, then the backfill.
  - Live path: Publish, host validates, PublishAck, fan-out.
  - VvDigest every 60 s catches gaps. A missing author seq exposes a host that withholds events (Cwtch precedent).
- Cost:
  - 100k messages is about 25 to 30 MB.
  - Verifying 100k signatures measured 3.2 s on one core. Verify in a background thread pool, and verify visible pages first.
- Why not negentropy for v1:
  - Measured on 100k items: 50 missing at the end cost 3.2 KB in 3 rounds; 1,000 clustered two-sided differences cost 17.8 KB; 400 scattered differences cost 391 KB.
  - The author set is bounded, vectors are simpler, and they show withholding directly.
  - Bring negentropy back if retention or partial history breaks seq contiguity.

6) MEMBERSHIP AND SECURITY
- QUIC TLS with raw public keys proves EndpointId = UserId.
- Hello carries mac = keyed_blake3(hello_key, server || client_endpoint || host_endpoint || nonce). This lets a host admit a member it has not yet seen admitted, then fetch their MemberAdmit chain.
- A malicious host cannot forge or alter messages, because events are signed. It can withhold events, which shows up as gaps, and it can see members' IP addresses, which is inherent to P2P; an optional relay-only mode would hide them.
- No extra end-to-end encryption for group content in v1. Only members, including the host who is entitled to read, see plaintext. n0 relays carry only QUIC ciphertext.
- Files: the iroh-blobs provider uses ConnectMode::Intercept to reject endpoint ids that are not members. Per-file keys are optional.
- Ban: the host refuses the user's connections and peers drop the user's events after last_seq.
- v1.1: KeyRotate puts a new server_secret in per-member HPKE-style lockboxes, so kicked users lose rendezvous and Hello access.

7) DMs AND 1:1 CALLS
- ALPN tincan/dm/1, dialed by UserId through iroh's N0 address lookup. A DM log uses the same Event format, with server = BLAKE3("tincan dm" || sorted(a, b)), and runs version-vector sync on every connect. Calls use the same connection.
- Offline delivery in v1: a local outbox shows "pending". Delivery fires immediately, when the recipient appears in any shared server's roster, and on retries backing off from 30 s to 1, 2, 5 and 15 minutes, each with a 4 s dial timeout.
- A first DM from a stranger with no shared server lands in Requests.
- v1.5: MailboxDrop events in a mutual server's log, sealed to the recipient's dm_prekey (envelope tested; 72 B overhead), with a 14-day expiry and per-sender caps. Whoever is host delivers them when the recipient connects.

8) CONTACTS
- Profile{name up to 32 characters, avatar blob up to 256 KB, dm_prekey}; the latest per author wins. Carried in every server log and in the DM handshake.
- Local nicknames. Show a 4-character id suffix when display names collide.
- A safety number derived from BLAKE3 of the sorted ids.
- Add a contact from a shared server's member list (the key is already verified by the log), or by pasting tincan://user/<z32>.

DEFER
- Multi-device and device certificates; key backup.
- Short invite codes.
- Rotating rendezvous keys (Berty-style, 24 h).
- KeyRotate on ban.
- DM mailboxes; always-on seeding peers (Keet blind peers, Briar Mailbox).
- Group E2E (openmls 0.9.2) and a DM double ratchet.
- Negentropy.
- Per-channel roles and private channels.
- Retention and pruning.
- iroh-gossip for servers above about 100 online.
- Ownership transfer.
- A self-hosted relay and DNS.
- DhtAddressLookup for endpoints: it pulls in a second copy of n0-mainline (0.7).

## Versions
- iroh 1.5.0 features: default plus unstable-net-report for Endpoint::net_report(); compiled
- iroh-base 1.5.0 SecretKey::from_bytes, as_signing_key, PublicKey::to_z32
- iroh-dns 1.5.0 pkarr SignedPacket; compiled
- iroh-relay 1.5.0 
- noq / noq-proto 1.5.0 iroh's QUIC stack; connection idle timeout defaults to 30 s
- n0-mainline 0.8.0 BEP44 client; compiled and run live
- iroh-mainline-address-lookup 0.6.1 depends on n0-mainline ^0.7 (0.7.2 gets pulled in); compiled; not recommended for v1
- iroh-mdns-address-lookup 0.6.1 compiled; use service_name tincan1, cadence 5 s
- iroh-gossip 0.103.0 compiled; deferred
- iroh-blobs 0.105.0 provider ConnectMode::Intercept for member-only serving (source read)
- iroh-docs 0.101.0 RBSR prior art; not recommended
- pkarr 9.0.0 not needed; pulls in pubky mainline ^8 as a second DHT stack
- mainline 8.0.1 pubky's DHT; superseded for Tincan by n0-mainline
- negentropy 0.5.3 compiled and measured; deferred
- ed25519-dalek 3.0.0 
- x25519-dalek 3.0.0 feature static_secrets; DM envelope tested
- chacha20poly1305 0.11.0 new_from_slice and XNonce::try_from API compiled
- blake3 1.8.x 
- openmls 0.9.2 deferred group E2E
- hpke 0.15.1 alternative for lockboxes; not compiled

## Risks
- Soft dependency on n0's public relays and dns.iroh.link. Using n0's pkarr relay for non-_iroh records is off-label, PUTs are rate limited per IP, and the policy could change. Keep it optional, with the DHT and the cached-peer layer as fallbacks.
- Mainline DHT is UDP and flaky. 1 of 3 cold full queries returned NOT FOUND, and a put takes about 3.75 s. Networks that block UDP lose this layer entirely, so a single miss must not be read as 'nobody online'.
- Anyone holding server_secret can overwrite the rendezvous record, kicked members included, which can hide the server from cold joiners. Mitigations are the cache layer, host verification and the resolve-before-publish rule; the full fix (rotating the secret) is deferred to v1.1.
- Epoch inflation: a malicious member can claim a huge epoch. This is limited by accepting claims only from the current host or after losing the host, and by the merge comparator ranking connected count first, but a trusted-member threat model is still assumed.
- The host learns every member's IP address. Relay-only mode would hide it at the cost of relay bandwidth.
- HLC timestamps are claimed by the author. A member can backdate messages within its seq order; the last_seq cutoffs make bans and demotions deterministic but cannot fix display order.
- Per-author seq numbers break if one identity runs on two devices or reinstalls without syncing, producing equivocation. V1 must enforce one device per identity and fetch your_next_seq before writing.
- A leaked invite link lets anyone join until it is revoked or expires. max_uses can be exceeded across partitions.
- Losing the owner key means admins can never be changed. Recovery and transfer are deferred.
- A dial to an offline peer hangs 30 s by default. Forgetting the app-level timeout makes failover and joining feel broken.
- A host on a laptop that sleeps produces crash-like failovers with about 10-13 s of text outage, unless the PBT_APMSUSPEND handoff runs in time.
- The host's upload bandwidth limits text fan-out and especially voice/screen SFU load in large servers. Not measured here.
- A fresh joiner to a 100k-message server downloads 25-30 MB and spends about 3.2 s of single-core CPU verifying signatures.
- Measurements come from one Windows machine and a handful of runs. Connect latencies were measured between two endpoints on the same machine, so real-world numbers will be higher.

## Open
- What is the target server size (online and total members)? It decides whether roster deltas, version vectors and host fan-out suffice, or whether gossip or multiple hosts are needed.
- Should new members see the full history (Discord behaviour) or only messages from their join time? This affects bandwidth and privacy.
- Is a dependency on n0's free relays and DNS acceptable for launch, or should a Tincan-run fallback relay and pkarr relay be budgeted?
- Is multi-device per user needed in v1? It changes UserId semantics (device certificates) and the seq model.
- Should a kick rotate server_secret in v1? It costs a lockbox event of about 80 B per member and adds complexity.
- Are short invite codes wanted? They need a member online to republish them to the DHT.
- Should the voice SFU host always be the text host, or can each voice channel elect its own media host (for example, the best-connected participant)?
- What retention and disk caps apply to history and to cached attachment blobs?
