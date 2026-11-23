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

The UI ships `ui/src/api/mock.ts`, an in-browser fake backend implementing the same commands and events with sample data, selected automatically when not running inside Tauri. It lets the UI run in a plain browser (`pnpm dev`) for development and screenshots.
