# tincan-core integration notes

What the other crates need to know beyond the doc comments. Rustdoc: `cargo doc -p tincan-core --no-deps`.

## Applying events (net)

- Use one `state::PendingBuffer` per conversation. For each incoming `StoredEvent`: `pending.ingest(&mut state, ev, now_ms)` returns the verdict plus every event it applied, in order (the incoming one first if accepted, then anything it unblocked). Persist those with `Storage::insert_event` in that order. On `Change::Deleted { target }`, call `Storage::mark_deleted`.
- `Verdict::Gap` / `Future` events are buffered by `ingest`. `Stale` (duplicate) and `Reject` events are dropped. A `Reject` is final: the author's later events on that replica stay `Gap`. On `PublishAck { ok: false }` tincan-net keeps the event queued and never reuses its seq, unless the author's own validation also rejects it (see `notes-tincan-net.md`, publish).
- Before authoring, check `ConvState::can_post(author, channel)` for messages and `ConvState::next_seq(author)` for the seq.
- Two rules from architecture.md are deliberately not part of validation, because they depend on arrival order and would split replicas permanently:
  - A message to a channel deleted at the same time is accepted. Deleted channels and their messages are hidden.
  - A `MemberAdmit` using a revoked or used-up invite is accepted. The host enforces revocation and `max_uses` at join time with `ConvState::invite_status` (`build::verify_join_request` does this). Any member can create invites anyway, so no security is lost.
  - Admitting someone who is already a member is accepted as a no-op.
  - Proof that every rule is order-independent: `state::tests::shuffled_delivery_converges` (40 random delivery orders plus worst cases give identical state).
- Invite fields: `max_uses == 0` means unlimited; `expires_ms == u64::MAX` means it never expires. Expiry is judged at the admit's hlc, so it is deterministic.
- DMs: `ConvState::new_dm(a, b)`. `Message.channel` must be `state::dm_channel(&conv)` (use `build::dm_message`). Only Message and Delete bodies; you can only delete your own messages.

## Startup rebuild (store + net)

- `ConvState::rebuild` needs EVERY stored event of the conversation. For each `(author, head)` in `Storage::heads(conv)`, call `events_range(conv, author, 1, head)`.
- `Storage::state_events` alone gives heads that are too short, so the next live event of that author is a `Gap` forever, and it loses Delete authority for old messages. Rebuild tolerates partial input but the result is incomplete.
- `rebuild` is deterministic (any input order gives the same state) and matches live application.

## Storage (store)

- Index events by `StoredEvent::conv()`, not `event.server`: a Genesis event carries `server = [0; 32]`.
- `mark_deleted` must keep the signed bytes. Deleted events are still synced, because seqs are contiguous. If the body is purged, later joiners can never receive the rest of that author's history. Hide the text in queries instead.
- `MemStorage` is the reference implementation. Its tests in `storage.rs` show the required ordering and paging semantics.
- `Storage::insert_events(&[&StoredEvent]) -> Vec<Result<bool>>` stores a sync page: one result per event, in order, each with the `insert_event` semantics, and a refused (conflicting) event does not affect the others. The trait has a default that calls `insert_event` per event (what `MemStorage` uses); `SqliteStorage` overrides it with one transaction per page. tincan-net stores every applied batch through it (`Replica::ingest_many`).

## Sync (net)

- Stream authors in `ConvState::sync_author_order()` order (owner, then members by admission time), each author's events in ascending seq. The receiver can then apply each run straight away instead of buffering it.
- The "state events of all authors before messages" idea in architecture.md does not work with contiguous seqs: a state event cannot apply before the same author's earlier messages.
- `Heads::digest` ignores authors with head 0.

## Joining (net)

Every step has a `build` helper:

1. Member creates the invite: `build::new_invite` (unlimited uses) or `new_invite_with_limit`, then publishes the InviteCreate and shares `invite.encode()`.
2. Joiner sends `build::join_request(joiner, &invite, &host, name, seq = 1, hlc, ts_ms)`.
3. Host runs `build::verify_join_request(&ctl, &state, &me, now_ms)`. On `Err`, send `rejection.to_ctl()`.
   - Also check that `VerifiedJoin::joiner` equals the QUIC peer id.
   - If `already_member` is set, reply with the existing admit.
   - Otherwise build `build::admit(...)`, apply and store it, then apply `VerifiedJoin::profile`, then reply `build::join_accepted(secret, genesis, admit)`.
4. Joiner runs `build::verify_join_accepted(&ctl, &invite, &me)`. This also proves the secret matches the invite's rendezvous keys.
5. Joiner sends `build::hello(...)`. The host checks it with `build::verify_hello(&ctl, hello_key, &peer, &me)`.

`build::new_server(identity, name, owner_name, &hlc)` returns the four first events, the random `server_secret` and the derived keys. It takes `owner_name` because it creates the owner's Profile event.

## Election (net)

- A `SignedHostClaim` is signed by the host it names. `Ctl::Handoff.claim` is therefore the old host's current claim. The successor issues its own claim (`build::host_claim(me, server, epoch + 1, Some(old), now)`).
- `election::score` gives an eligible candidate at least 1, so `score > 0` means exactly "eligible".
- `election::beats_by_margin` implements the 30 % proactive-migration margin.

## Behaviour changes to pre-existing code

None of these changes a signature.

- `SignedEvent::sign` returns `Err(InvalidEvent)` instead of panicking when the author is not the signer.
- `SignedEvent::decode` rejects trailing bytes.
- `SignedHostClaim::sign` returns `Err(BadKey)` when the claim names another host.
- `Hlc::observe` ignores remote values more than 5 minutes ahead of the wall clock, and `now_at` saturates.
- `FrameDecoder::next` consumes a frame that fails to decode before returning the error, so the stream can continue. An oversized length is still fatal.
- `annexb_nals` strips trailing zero bytes from the last NAL too and skips empty NALs.
- Additions to existing types:
  - `state::Member` gained `admits`, `last_leave` and `admitted_hlc`.
  - `ConvState` derives `PartialEq, Eq`.
  - `Storage::insert_events` (a provided method, so existing implementations still compile).
  - `Ctl::DmProfile { name }`, appended as variant 39: the sender's display name on a DM connection. `within_limits` drops names over `4 * MAX_NAME_CHARS` bytes; receivers also run `state::validate_name` on the trimmed name. Peers that predate it skip the frame, since `FrameDecoder` consumes undecodable frames. Putting the name into `DmHello` instead was not done: postcard is not self-describing, so a 4-field `DmHello` cannot decode an old 3-field one.

## Short codes (`code.rs`)

- `ShortCode`: 30 bits over `CODE_ALPHABET` (32 symbols). `parse` takes 6 alphabet characters in any case and skips whitespace and dashes; `Display` is `ABC-DEF`, `compact()` is the KDF input.
- `CodeKeys::derive(kind, code)`: argon2id (`KDF_MEMORY_KIB` 64 MiB, `KDF_PASSES` 5, 1 lane), salt per `CodeKind`. About 200 ms in release; call it off async executors (tincan-net uses `spawn_blocking` and a cache). A test pins one output: changing parameters or salts breaks every published code.
- `seal_code` / `open_code`: the invite code inside `InviteCreate.max_uses` (flag bit 31, code XOR a mask keyed by `ServerKeys::code_key`, a new sub-key). `ConvState::short_code_invites` lists the owner's unrevoked, never-expiring flagged invites newest first; callers must still check each `invite_pk` against `CodeKeys::invite_pk` (the net layer does).
- `InviteState` gained `created_hlc`.
- Records: `code_invite` cuts an `Invite` down to fit a pkarr packet (name 32 chars, 2 peers, relay URLs up to 96 chars, 2 direct addresses each); `seal_invite` / `open_invite`; `FriendRecord::new/seal/open` (the signature covers the record key).
- New dependency: `argon2` 0.5 (pure Rust, `alloc` only).
