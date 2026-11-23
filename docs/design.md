# Tincan UI design

Two tin cans and a string. The interface stays out of the way: neutral "tin" surfaces, one warm "string" accent, Windows-native type, no decoration that does not carry state.

## Principles

- Left nav, content center. Server rail, channel list, content. Member list is an optional right panel.
- Every badge, dot and colour encodes real state (unread, speaking, muted, host, connection type, download progress). Nothing ornamental.
- One setup step: pick a display name. Everything else has a default.
- Copy follows the unslop-ui rules: sentence case, verb-first buttons, no eyebrows, no subtitle under every heading, no arrows on buttons, no em dashes in UI strings.

## Tokens

Defined once in `src/styles/tokens.css` as CSS custom properties. Dark is the default; light follows `prefers-color-scheme` unless the user picks one in settings.

| Token | Dark | Light | Use |
|---|---|---|---|
| `--bg-rail` | `#141619` | `#e4e6e9` | Server rail |
| `--bg-side` | `#1a1d21` | `#eef0f2` | Channel list, member list |
| `--bg-main` | `#202328` | `#ffffff` | Content |
| `--bg-raised` | `#282c32` | `#ffffff` | Menus, dialogs, tiles |
| `--bg-hover` | `rgb(255 255 255 / 0.05)` | `rgb(0 0 0 / 0.04)` | Row hover |
| `--bg-selected` | `rgb(255 255 255 / 0.09)` | `rgb(0 0 0 / 0.07)` | Selected row |
| `--border` | `rgb(255 255 255 / 0.08)` | `rgb(0 0 0 / 0.09)` | Hairlines |
| `--text-1` | `#e9eaec` | `#1a1c1f` | Body |
| `--text-2` | `#a4a9b0` | `#5b6068` | Supporting |
| `--text-3` | `#6e747c` | `#8a9098` | Timestamps, placeholders |
| `--accent` | `#e0784f` | `#c95f37` | String: primary button, focus ring, mention, selection |
| `--accent-fg` | `#1a1c1f` | `#ffffff` | Text on accent |
| `--speaking` | `#3fb37f` | `#2b9466` | Speaking ring, direct connection |
| `--warn` | `#e2a336` | `#b07a14` | Relayed connection, degraded |
| `--danger` | `#e5484d` | `#cd2b31` | Leave call, delete, errors |

Type: `"Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif` (ships with Windows, no font download). Sizes 12, 13, 14, 16, 20. Weights 400 and 600.

Spacing: 4px scale (4, 8, 12, 16, 20, 24, 32). Radius: 6px controls, 8px cards and tiles, 12px server icons, 50% avatars.

Motion: `transition: background-color 120ms ease-out, color 120ms ease-out`. No entrance animations. Speaking ring fades in 80ms.

## Window layout

```
+------+-----------------+----------------------------------+-----------+
| rail | channel list    | header: # general                | members   |
| 64px | 240px           |----------------------------------| 220px     |
|      |                 | messages                         | (toggle)  |
| DMs  | Text            |                                  |           |
| ---  |  # general      |                                  |           |
| GN   |  # clips        |                                  |           |
| BC   | Voice           |                                  |           |
| +    |  Lounge         |                                  |           |
|      |   (o) Sam       |                                  |           |
|      |   ( ) Alex [scr]|                                  |           |
|      |-----------------|                                  |           |
|      | call panel      | composer                         |           |
|      | user panel      |                                  |           |
+------+-----------------+----------------------------------+-----------+
```

- Custom title bar (decorations off) 32px tall, draggable, with min/max/close on the right. Keeps the window one continuous surface.
- Channel list and member list widths: a grab bar on the edge that faces the content resizes them (channel list 200 to 420 px, default 240; member list 220 to 360 px, default 220). Drag or use the arrow keys, Home and End; double-click resets. The bar shows a `--border` line on hover and an `--accent` line while dragging or focused. In a narrow window both give way to their minimum before the content goes below 300 px.
- Rail: DM button, divider, servers (initials on a rounded square, selected = accent fill), add-server button. Unread = small dot under the icon.
- Unread text channel: name in `--text-1` at weight 600, and a badge at the row's end (a 6 px `--text-1` dot; a count capped at "99+" once the app sends one). The channel on screen never shows one.
- Channel list header: server name, "Hosted by <name>" (real state from the session), menu: invite people, rename server (owner only), leave server.
- Voice rooms list who is inside. Avatar ring turns `--speaking` while that person talks. Mic-off and screen icons appear only when true.
- Call panel (only while in a call): room name, connection label (Direct / Relayed, with RTT on hover), mute, deafen, share screen, leave.
- User panel: avatar, name, settings button. Mute and deafen live in the call panel.

## Screens

1. **Welcome** (first launch only). Heading "Pick a display name", one input, "Continue". Identity keys are generated silently.
2. **Home / no servers.** Two actions: "Create server", "Join with invite".
3. **Text channel.** Messages grouped by author within 5 minutes. Hover shows timestamp and actions (copy, delete own). Attachments render as file cards: name, size, peers that have it, progress while downloading, open/download button. Images preview inline once downloaded.
4. **Voice room view.** Selected when you open a voice room. Participant tiles in a grid; when anyone shares a screen the share takes the stage and tiles move to a strip. Double-click the stage for full window.
5. **Direct messages.** Rail DM button. List of conversations, each opens a chat with a call button in its header. Messages to an offline contact show "Waiting for <name> to come online" until delivered.
6. **Soundboard.** Opened from the call panel while in a voice room. Grid of clip tiles (name, duration). Click a tile to play it into the call. Two groups: "Your sounds" (local library, "Add sound" picks an audio file) and "Heard in this call" (clips others played that you don't have yet). A "Heard" tile shows a download button; once pulled it moves to "Your sounds" and you seed it. Tile shows progress while downloading and a play state while playing. Limits shown at the add step only: up to 20 seconds, 5 MB.
6b. **Share screen picker.** Opened from the call panel's share button. Two groups, Displays and Windows, each a grid of tiles (name, size). A Text / Motion toggle (sharp text at up to 15 fps, or smooth motion at up to 30 fps). "Share" starts it; the button becomes "Stop sharing". A machine with no hardware H.264 encoder shows that in place of the grid.
7. **Invite dialog.** The invite code in a read-only field and "Copy invite".
8. **Join dialog.** Paste field, "Join server". Error states say what failed and what to do.
9. **Settings.** Profile (name), Voice (input, output, input level meter, noise suppression, echo cancellation), Files (download folder), Network (connection status, custom relay URL).

## Icons

Lucide (tree-shaken, only the icons used). Icon buttons always carry `aria-label` and a tooltip.

## App icon

Two tin cans joined by a slack string, flat, one colour on transparent. Source in `src-tauri/icons/icon.svg`.
