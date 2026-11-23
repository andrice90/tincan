# App API (Rust app <-> UI)

The single source of truth is [`ui/src/api/types.ts`](../ui/src/api/types.ts): the `Commands` map (Tauri `invoke` name -> args and result) and the `Events` map (Tauri `emit` name -> payload). `crates/tincan-app/src/dto.rs` mirrors those types with `#[serde(rename_all = "camelCase")]`; enum string values are the lowercase/kebab strings shown in the TS unions.

Rules:

- Command arguments are camelCase objects exactly as in `Commands[name].args` (Tauri converts to snake_case Rust parameters).
- Errors: commands return `Result<T, String>`; the string is user-facing, short, and says what to do ("Couldn't reach anyone in that server. Check the invite or try again later."). Special machine-readable error strings: `no-hardware-encoder`, `not-in-voice`, `deafened`.
- Ids: users as z-base-32, everything else lowercase hex.
- `messages` returns a page oldest-first; `cursor` is an opaque string (`<hlc>:<eventIdHex>`) for the next older page.
- Events carry full objects so the UI can replace its copy without refetching. `snapshot` is sent after joins, leaves and the first-run name pick.
- `speaking` and `mic-level` are high-frequency; everything else is low-frequency.
- Video frames never go through events: `screen_watch` takes a Tauri `Channel` and receives raw `ArrayBuffer`s (layout in the type comment and `tincan_core::media::ui_video_message`).
- Files picked by the user (attachments, soundboard clips) come from `@tauri-apps/plugin-dialog` as absolute paths.
- Attachments over 25 MB and clips over 5 MB are refused when their real size is known: the transfer state is `failed`, and `download` of a file that declares more than 25 MB errors with "This file is larger than 25 MB, so Tincan won't download it." A blob is served only to peers in a conversation it is shared with (see the security model in architecture.md).
- `open_file` opens documents, media and images whose bytes match their extension with the default app and reveals everything else in Explorer; every export carries the Mark of the Web.
- `dm_watch` names the DM on screen (null when none). The app keeps the connection to that contact up while it is named, so `DmSummary.online` is right and calls work for a contact who shares no server.
- Voice rooms have a chat. `messages`, `send_message`, `mark_read` and `typing` take a voice room's id as `channelId` exactly like a text channel's, without `voice_join`; `Channel.unread` and `ServerSummary.unread` count a room's chat. A deleted room refuses new messages like a deleted text channel ("That channel doesn't exist anymore."). Builds before this change reject voice room messages in validation, so all members need a current build.
- `VoiceParticipant.volume` is the saved per-user playback volume (0..2, 1 for me); `voice-updated` follows `set_user_volume`.
- Names in every DTO are peer-supplied and cleaned of control and hidden format characters; the UI isolates them with `<bdi>`.

The UI ships `ui/src/api/mock.ts`, an in-browser fake backend implementing the same commands and events with sample data, selected automatically when not running inside Tauri. It lets the UI run in a plain browser (`pnpm dev`) for development and screenshots.
