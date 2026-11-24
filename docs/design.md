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
| `--bg-backdrop` | `#0f1114` | `#dcdfe3` | Floating: titlebar and the space around the cards |
| `--shadow-float` | `0 1px 2px` 30%, `0 2px 8px` 18% black | 6% and 5% | Floating: card shadow |

Type: `"Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif` (ships with Windows, no font download). Sizes 12, 13, 14, 16, 20. Weights 400 and 600.

Spacing: 4px scale (4, 8, 12, 16, 20, 24, 32). Radius: 6px controls, 8px cards and tiles, 12px server icons and Floating layout cards (`--r-float`), 50% avatars. Floating gap: 8px (`--gap-float`).

Motion: `transition: background-color 120ms ease-out, color 120ms ease-out`. No entrance animations. Speaking ring fades in 80ms. The Floating layout adds one more: panels opening and closing (see below).

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
- Channel list and member list widths: a grab bar on the edge that faces the content resizes them (channel list 200 to 420 px, default 240; member list 220 to 360 px, default 220). Drag or use the arrow keys, Home and End; double-click resets. The bar shows a `--border` line on hover and an `--accent` line while dragging or focused. In a narrow window both give way to their minimum before the content goes below 276 px (the room left in the 800 px minimum window at default widths). In the Floating layout the gaps take that room: the content keeps 240 px, see "Floating layout".
- Rail: DM button, divider, servers (the server icon, or initials on a rounded square; selected = accent fill, or an accent ring around an icon), add-server button. Unread = small dot under the icon.
- Avatars everywhere (member list, message authors, DM list and header, voice tiles, call panel rows of a call, user panel) show the person's picture over the coloured initial. The initial stays underneath as the fallback while the picture loads, when there is none, or if it fails to load. Pictures load lazily at the avatar's size.
- Unread text channel: name in `--text-1` at weight 600, and a badge at the row's end (a 6 px `--text-1` dot; a count capped at "99+" once the app sends one). The channel on screen never shows one.
- Channel list header: server name, "Hosted by <name>" (real state from the session), menu: invite people, rename server (owner only), leave server.
- Voice rooms list who is inside. Avatar ring turns `--speaking` while that person talks. Mic-off and screen icons appear only when true.
- Call panel (only while in a call): room name, connection label (Direct / Relayed, with RTT on hover), mute, deafen, share screen, leave.
- User panel: avatar, name, settings button. Mute and deafen live in the call panel.

## Floating layout

A second layout, picked in Settings > Appearance > Layout: "Floating" (the default) or "Classic" (the flat panels above). It is saved on this PC (`localStorage` key `tincan.layout`, per profile) and set as `data-layout="floating"` on `<html>` before the first paint; `src/styles/floating.css` holds its rules.

```
+-------------------------------------------------------------------------+
| title bar (backdrop)                                                    |
| +----+ +-------------+ +-----------------------------+ +-------------+  |
| |rail| | channel card| | content card                | | right card  |  |
| |    | | server head | | header, messages, stage,    | | members, or |  |
| |    | | channels    | | screen share                | | a voice     |  |
| |    | | voice rooms | |                             | | room's chat |  |
| |    | +-------------+ +-----------------------------+ +-------------+  |
| |    | | account card| | composer card               | | composer    |  |
| +----+ +-------------+ +-----------------------------+ +-------------+  |
+-------------------------------------------------------------------------+
```

- Every region is a card on the backdrop: server rail, channel card (server or DM list), account card (voice controls while in a room or call, then avatar, name, settings), content card (header, messages, voice stage, screen share, soundboard), composer card (composer and typing line; hidden on views without a composer, such as a room's tiles), right card (member list beside a text channel, or a voice room's chat while its chat panel is open). The room chat's composer is its own card under the right card, the same radius, gap, shadow, `--bg-main` and 52px height as the composer card, so the right column is a chat card over a composer card (the member list has none).
- Cards share one radius `--r-float` (12px), one shadow `--shadow-float` and one gap `--gap-float` (8px) between cards and around the window. The titlebar and the space around the cards are `--bg-backdrop`. Rail and channel, account and right cards are `--bg-side`; content and composer cards `--bg-main`; rail buttons `--bg-main`.
- Grab bars sit in the gaps: the sidebar's to the right of the channel and account cards, the member list's to the left of the right card. The 2px line is centred in the gap.
- Minimum window (800x520): the gaps and the rail take 104px, so the sidebar (200 min), content (240 min) and right card (member list 220 min, room chat 256 min, else 340) share 696px. Cards give way to those minimums. The channel card scrolls when the voice controls take its height.
- Motion: `--motion-panel` (220ms `cubic-bezier(0.65, 0, 0.35, 1)`, ease-in-out). Each open has the same close in reverse, and pushes the cards next to it:
  - right card: width 0 to the panel's width, the content card narrows as it slides; the content inside stays at its minimum width, anchored right, so it slides in with the card's edge;
  - account card: the voice controls grow from height 0 when joining a room or call (and the channel card above shrinks), and shrink back when leaving;
  - composer card: grows from height 0 under the content card when a view has a composer, and shrinks away when the next one has none;
  - room chat beside the tiles: it is the right column (chat card over its composer card), so both cards slide in and out together like the member list, and the content card narrows and widens with them.
  
  What closes stays on screen, inert, until its transition ends, then unmounts. Under `prefers-reduced-motion` every change is instant. Classic keeps no motion: its panels open and close at once, as before.

## Screens

1. **Welcome** (first launch only). Heading "Pick a display name", one input, "Continue". Identity keys are generated silently.
2. **Home / no servers.** Two actions: "Create server", "Join with invite".
3. **Text channel.** Messages grouped by author within 5 minutes. Hover shows timestamp and actions (copy, delete own). Attachments render as file cards: name, size, peers that have it, progress while downloading, open/download button. Images preview inline once downloaded.
4. **Voice room view.** Shown when you click a voice room (its row or name, or its chat button). Opening a room never joins it. In a room you are not in: tiles of the people inside ("No one is here yet." when empty), "Join voice" in the header, and no call controls for that room (the call panel shows only the call you are in, if any). Only "Join voice" connects audio. Once joined: participant tiles in a grid; when anyone shares a screen the share takes the stage and tiles move to a strip. Double-click the stage for full window. A "Chat" button at the right of the header toggles the room's chat as a 340px panel beside the tiles, so you can talk and type at once, joined or not.
4b. **Voice room chat.** Every voice room has its own text chat, and it is only ever that panel (there is no full-page room chat). The chat button at the right of the room's row does what the header's Chat button does on the room on screen (toggles the panel); on another room it opens that room with the panel open. Both buttons show pressed while the panel is open. The room row and the Chat button are bold while the chat has unread messages; the chat is marked read only while the panel is on screen.
5. **Direct messages.** Rail DM button. List of conversations, each opens a chat with a call button in its header. Messages to an offline contact show "Waiting for <name> to come online" until delivered.
6. **Soundboard.** Opened from the call panel while in a voice room. Grid of clip tiles (name, duration). Click a tile to play it into the call. Two groups: "Your sounds" (local library, "Add sound" picks an audio file) and "Heard in this call" (clips others played that you don't have yet). A "Heard" tile shows a download button; once pulled it moves to "Your sounds" and you seed it. Tile shows progress while downloading and a play state while playing. While a sound of mine plays, a stop button (filled square, "Stop <name>") sits at the right of its tile and stops it at once, for everyone. Limits shown at the add step only: up to 20 seconds, 5 MB. The soundboard closes on Escape, its close button, its toggle in the call panel, and a click anywhere outside it (its own menus and dialogs don't count).
6c. **Volumes.** Right-click (or click) a member, or right-click a voice tile: the menu shows their name, then "Volume" and "Soundboard volume" sliders (0 to 200 %, saved per person on this PC; the soundboard volume is on top of their volume), then the actions. Right-click a voice room: "Soundboard volume" for every sound heard in that room, mine included (saved per room), then the owner's rename and delete.
6b. **Share screen picker.** Opened from the call panel's share button. Two groups, Displays and Windows, each a grid of tiles (name, size). A Text / Motion toggle (sharp text at up to 15 fps, or smooth motion at up to 30 fps). "Share" starts it; the button becomes "Stop sharing". A machine with no hardware H.264 encoder shows that in place of the grid.
7. **Invite dialog.** The invite code in a read-only field and "Copy invite".
8. **Join dialog.** Paste field, "Join server". Error states say what failed and what to do.
8b. **Crop image.** Opened after picking an image for "Change avatar" (Settings, Profile) or "Change icon" (the owner's server menu, which also has "Remove icon"). A 280px square viewport with the final shape outlined and everything outside it dimmed: a circle for avatars, the rail's rounded square (12/44 of the side) for server icons. Drag (pointer or touch) or the arrow keys move the image (Shift moves further); the "Zoom" slider, the wheel and +/- zoom from 1x (the shape just covered) to 4x. The image always covers the shape. Cancel and Save; Save renders the visible crop to 128x128 and encodes WebP under 64 KB (PNG at falling sizes when WebP is unavailable). Escape cancels and returns to where the picker was opened.
9. **Settings.** Profile (avatar with "Change avatar" and "Remove", name), Voice (input, output, input level meter, noise suppression, echo cancellation), Files (download folder), Network (connection status, custom relay URL), Appearance (theme: Match Windows, Dark, Light; layout: Classic, Floating).

## Icons

Lucide (tree-shaken, only the icons used). Icon buttons always carry `aria-label` and a tooltip.

## App icon

Two tin cans joined by a slack string, flat, one colour on transparent. Source in `src-tauri/icons/icon.svg`.
