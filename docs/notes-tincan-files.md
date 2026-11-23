# tincan-files: integration notes

Crate: `crates/tincan-files` (iroh-blobs =0.105.0 on iroh 1.5). Read the crate docs (`src/lib.rs`) for the full API; this page lists what the app must do.

## Wiring

1. Set `TOKIO_WORKER_THREADS=2` at the very top of `main`, before any thread or runtime exists. The iroh-blobs `FsStore` builds its own tokio runtime sized by this variable (one thread per core otherwise: 54-58 threads on a 24-core machine).
2. Build the endpoint (tincan-net), then `Files::load(&profile_dir.join("blobs"), endpoint.clone(), policy).await?`. It adds an in-memory address lookup to the endpoint. It does not spawn a Router.
3. Register `files.protocol()` on the shared Router under `tincan_files::ALPN` (= `iroh_blobs::ALPN`, `/iroh-bytes/4`).
4. On exit call `router.shutdown().await`. The protocol handler's shutdown runs `Files::shutdown` (downloads stop, partial progress is kept, the store is flushed and closed). `Files::shutdown` is idempotent and may also be called directly. Without it, download tasks and the store stay alive until the runtime ends, and a hard exit loses the last partial chunk.

`AccessPolicy` has two methods, both run on the provider event loop (which also paces every uploaded chunk), so keep them non-blocking: no IO, a short lock at most.

- `allow(&UserId)`: once per incoming blobs connection. May this peer connect at all?
- `allow_blob(&UserId, &BlobHash)`: once per get and observe request on an allowed connection. May this peer fetch, or ask about, this blob? The provider remembers the vetted peer per connection id (request events carry only the connection id) and forgets it when the connection closes.

A closure `Fn(&UserId) -> bool + Send + Sync` implements both: it allows every blob to the peers it allows (tests use it). The app implements `allow_blob` per conversation (`tincan-app` `index.rs`, see the Blob access section of `notes-tincan-app.md`).

## Mapping to the protocol

| App need | Call |
|---|---|
| Send attachment / add clip | `import(path, kind)` (TryReference, no copy; file must not change afterwards) or `import_bytes` |
| `Have` / `Hello.have` | `complete_hashes()`; `FileEvent::Completed` signals additions, with `from` (the provider that delivered the last verified bytes; `None` for imports and blobs already held). `provided_by(hash)` gives the same after a lagged receiver |
| Message with attachment arrives (auto-prefetch <= 25 MB) | `download(hash, Attachment, [author, ...holders])` (refused past `MAX_ATTACHMENT_BYTES`) |
| `Holders` reply / holder comes online | `add_providers(hash, holders)` (wakes an `Unavailable` request) |
| Pull a heard clip | `download(hash, Clip, [player, ...])` |
| `transfer` command | `transfer(hash)` (in memory), `status(hash)` (store) |
| Open file | `export(hash, Downloads\Tincan\name)` |
| Clip playback | `read_to_bytes(hash, 5 MB)` |
| Resume after restart | `partial_downloads()` then `download(...)` with fresh holders |
| Throttle while in a call | `set_upload_limit(Some(bps))`, `None` after |

`TransferState` has no `Idle`: the UI's `idle` means "no request and not held" (`transfer(hash) == None`).

## Behaviour

- Providers are tried one at a time in the given order, each asked only for the missing ranges. A dead provider costs the 4 s connect timeout. When all failed: `Unavailable`, parked until `add_providers` (re-adding a provider that already failed retries it).
- At most 3 downloads run at once; others are `Queued`.
- Progress events are coalesced to about 4 per second per download; state changes are always sent.
- Tags: `att-<hex>` and `clip-<hex>`. A download tags the hash when it starts, so garbage collection (every hour) never deletes partial data. `remove(hash)` drops both tags, `remove_kind` one; GC then deletes the data (referenced source files are never deleted).
- Provider: connections intercepted by `allow`; get and observe requests intercepted by `allow_blob` (a denied observe also hides whether this node holds the blob); get-many and push disabled; every 16 KiB chunk passes the upload token bucket (burst 16 KiB). `uploaded_bytes()` counts served payload.
- Only single-blob gets are served. A get that asks for child ranges would make the provider read an allowed blob as a hash sequence and send the blobs it lists, past the per-blob check, so it is refused (`provider::get_verdict`).
- Size caps per kind: `BlobKind::max_size` is 25 MiB for attachments (`MAX_ATTACHMENT_BYTES`) and 5 MiB for clips (`MAX_CLIP_BYTES`). The declared size in a message is only the sender's claim. The real size is checked when it is known: at start for a partial blob, at the first verified chunk otherwise, and at finish; verified bytes past the cap also end it. A kind whose cap the blob exceeds is dropped from a merged request (the kind it continues as is tagged); if none is left the request turns `TransferState::TooLarge` (with `total` = the refused size), its tags are deleted so GC frees the partial data and `partial_downloads` never lists it, and asking again for a kind with a smaller cap repeats `TooLarge` without fetching. Asking again for a kind whose cap allows it starts over. A blob held as one kind and too large for another reports `TooLarge` for the other.

## Known limits

- `router.shutdown()` took about 3 s in the test whose node had just timed out dialing an offline provider (8 ms otherwise). The time is spent after `Files::shutdown` returned, in iroh's endpoint close; the cause is not confirmed.
- GC was checked with a 300 ms interval in a scratch copy: tagged partial and complete blobs survived (including across a restart), a removed blob was deleted, a referenced source file was not. iroh-blobs logs `ERROR error during gc run` if a GC run overlaps store shutdown, and its GC loop stops for the rest of the session after any GC error (iroh-blobs behaviour).
- No Observe-before-fetch on the download side: a partial holder is discovered by trying it (it serves what it has, then the next provider completes the rest).
