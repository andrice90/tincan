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
- Short codes: `get_invite_code` returns the server's 6-character code (`ABC-DEF`); for the owner it is made first when there is none (a KDF and a record write, so it can take a moment), other members get null until then. `rotate_invite_code` is owner only; the old code stops admitting at once. `get_friend_code` makes my friend code on first use; `rotate_friend_code` replaces it. `join_server` takes a short code or a full invite or link; `add_contact` takes a friend code, a `tincan1u...` ID or a bare user id. Input is case-insensitive, dash and spaces optional. Offline (`TINCAN_OFFLINE_NETWORK=1`) codes resolve through `profiles/.offline-codes`, shared by every profile on the machine. Errors: "No server uses that code. Check it, or ask for a new one.", "No one uses that friend code. Check it, or ask for a new one.", "Only the server owner can change the invite code."
- `dm_watch` names the DM on screen (null when none). The app keeps the connection to that contact up while it is named, so `DmSummary.online` is right and calls work for a contact who shares no server.
- `VoiceParticipant.volume` is the saved per-user playback volume (0..2, 1 for me); `voice-updated` follows `set_user_volume`.
- Names in every DTO are peer-supplied and cleaned of control and hidden format characters; the UI isolates them with `<bdi>`.

The UI ships `ui/src/api/mock.ts`, an in-browser fake backend implementing the same commands and events with sample data, selected automatically when not running inside Tauri. It lets the UI run in a plain browser (`pnpm dev`) for development and screenshots.
