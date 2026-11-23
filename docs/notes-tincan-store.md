# tincan-store notes

`SqliteStorage` (crates/tincan-store) implements `tincan_core::storage::Storage` and adds the app tables.

## Opening

- `SqliteStorage::open(profile_dir.join("tincan.db"))` creates missing parent directories. `open_in_memory()` is for tests.
- Connection settings: WAL, `synchronous=NORMAL`, `busy_timeout` 5 s, `foreign_keys=ON`, `journal_size_limit` 4 MiB, statement cache of 64.
- Schema version is `PRAGMA user_version` (`SCHEMA_VERSION = 1`). A file with a higher version is refused, not opened. Add migrations by appending to `MIGRATIONS` in `src/schema.rs`; never edit a step that has shipped.
- Share it as `Arc<SqliteStorage>`, and coerce to `Arc<dyn Storage>` for tincan-net. It is one connection behind a mutex. Calls are short and blocking.

## Semantics chosen where the trait docs are silent

- The conversation of an event is `Event.server`, except `Genesis`: it is stored under `ConvId::from_event(&id)`, because its `server` field is all zeros.
- `insert_event`: returns `false` for a duplicate id. A *different* event with an already stored `(conv, author, seq)` returns `Err(CoreError::Storage)` and is logged at `warn`. `MemStorage` stores both, which is the one known difference. A test feeds the same events to `SqliteStorage` and `MemStorage` and checks that every other read matches.
- `insert_events_batch(&[StoredEvent]) -> usize` (inherent method, not on the trait) inserts in one transaction. Use it for sync pages: about 70 ms for 10k events on this machine. All-or-nothing.
- `messages(conv, None, ..)` pages every message of the conversation regardless of channel. This is the DM path: DM messages still carry some `channel` in `Body::Message`.
- `state_events` returns every non-Message event, Deletes included, ordered by `(author, seq)`. As the core docs say, this is not enough for `ConvState::rebuild`. Rebuild from `heads` plus `events_range(conv, author, 1, head)`.
- `heads` is ordered by author. `peers` is ordered by `last_seen_ms` descending, then user id. `convs` is ordered by `joined_ms`, then id.
- `delete_conv` removes the record **and** that conversation's events, cached peers and read markers, in one transaction.
- `mark_deleted` only sets the flag. The signed bytes stay, since sync needs them.
- Reads decode the stored bytes without re-verifying signatures.

## App tables

| API | Notes |
|---|---|
| `get_settings_json` / `set_settings_json` (+ typed `get_settings<T>` / `set_settings<T>`) | One JSON document. Invalid JSON is refused. |
| `add_clip` / `get_clip` / `list_clips` / `rename_clip` / `remove_clip` | `add_clip` returns `false` and changes nothing if the hash is already in the library. List is ordered by `added_ms`. |
| `upsert_contact` / `get_contact` / `list_contacts` / `remove_contact` | An upsert updates the name and keeps the first `added_ms`. |
| `set_read` / `last_read` / `unread(conv, channel, me)` | Markers never move backwards. `channel = None` is the conversation-wide marker (DMs) and matches messages in every channel. With no marker, every message from someone else counts as unread. Deleted messages never count. For a server-level unread dot, check each text channel. |
| `upsert_download` / `get_download` / `list_downloads` / `remove_download` | `state` is free text (use the UI `TransferState` strings). List is in insertion order. |

## Indexes

- `events`: primary key `id`, plus unique `(conv, author, seq)` (used by heads and ranges).
- `(conv, channel, hlc, id)`: paging and unread within one channel.
- Partial `(conv, hlc, id) WHERE channel IS NOT NULL`: paging and unread across a whole conversation.
- Partial `(conv, author, seq) WHERE channel IS NULL`: `state_events`, which never touches message rows.
- A test asserts the plans: no temp B-tree and no full scan, and the `(hlc, id) < (?, ?)` cursor is an index range.
