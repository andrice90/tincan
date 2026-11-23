# tincan-files: integration notes

Crate: `crates/tincan-files` (iroh-blobs =0.105.0 on iroh 1.5). Read the crate docs (`src/lib.rs`) for the full API; this page lists what the app must do.

## Wiring

1. Set `TOKIO_WORKER_THREADS=2` at the very top of `main`, before any thread or runtime exists. The iroh-blobs `FsStore` builds its own tokio runtime sized by this variable (one thread per core otherwise: 54-58 threads on a 24-core machine).
2. Build the endpoint (tincan-net), then `Files::load(&profile_dir.join("blobs"), endpoint.clone(), policy).await?`. It adds an in-memory address lookup to the endpoint. It does not spawn a Router.
3. Register `files.protocol()` on the shared Router under `tincan_files::ALPN` (= `iroh_blobs::ALPN`, `/iroh-bytes/4`).
4. On exit call `router.shutdown().await`. The protocol handler's shutdown runs `Files::shutdown` (downloads stop, partial progress is kept, the store is flushed and closed). `Files::shutdown` is idempotent and may also be called directly. Without it, download tasks and the store stay alive until the runtime ends, and a hard exit loses the last partial chunk.

`AccessPolicy::allow(&UserId)` runs once per incoming blobs connection, on the provider event loop: keep it non-blocking ("member of a shared server or a DM contact"). A closure `Fn(&UserId) -> bool + Send + Sync` implements it.

## Mapping to the protocol

| App need | Call |
|---|---|
| Send attachment / add clip | `import(path, kind)` (TryReference, no copy; file must not change afterwards) or `import_bytes` |
| `Have` / `Hello.have` | `complete_hashes()`; `FileEvent::Completed` signals additions |
| Message with attachment arrives (auto-prefetch <= 25 MB) | `download(hash, Attachment, [author, ...holders])` |
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
- Provider: connections intercepted by the policy; observe enabled (no per-request events, the connection is already vetted); push disabled; every 16 KiB chunk passes the upload token bucket (burst 16 KiB). `uploaded_bytes()` counts served payload.

## Known limits

- `router.shutdown()` took about 3 s in the test whose node had just timed out dialing an offline provider (8 ms otherwise). The time is spent after `Files::shutdown` returned, in iroh's endpoint close; the cause is not confirmed.
- GC was checked with a 300 ms interval in a scratch copy: tagged partial and complete blobs survived (including across a restart), a removed blob was deleted, a referenced source file was not. iroh-blobs logs `ERROR error during gc run` if a GC run overlaps store shutdown, and its GC loop stops for the rest of the session after any GC error (iroh-blobs behaviour).
- No Observe-before-fetch on the download side: a partial holder is discovered by trying it (it serves what it has, then the next provider completes the rest).
