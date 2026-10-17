# Polish 06 · The composer and everything you send

Mockup `mockups/06-composer.html`. From product `09`, `00` ruling 9, `08-audit`, `07` R4/R10/R12; changes `design/04` where
it says so; tokens and fit from `01-system.md` and `polish/01-system.md` (**P1§**). Sizes in pt. The build today
(`--seed-demo`): a field and a send square, nothing else; the picker runs off the window bottom at 1120 and 900 wide.

## 1. The card (frames 1–2)

One row, bottom-aligned, inside the dock (`px 20`, `pb 16`, the 20 typing slot above). Left to right:

| Part | Spec |
|---|---|
| Card | `raised`, 1 `border`, radius 10, min height 40, pad 5 (controls sit 5 in from every edge). Focused: `focus` border + 3 halo (S§2 field). |
| Attach | Icon button 28, paperclip 16 `text_soft`, radius 6. Tooltip "Attach files  ⌘U". Gap 4 to the field. |
| Field | 14/20 `text`, py 4 (so one line = 28 = a control), grows to 8 lines then scrolls (04). Placeholder "Message #showcase". |
| GIF | Icon button 28, new glyph `gif-16` (rounded 16 × 12 outline with "GIF", stroke 1.5). Tooltip "GIFs  ⇧⌘G". |
| Emoji | Icon button 28, smile 16. Tooltip "Emoji  ⌘E". Gap 2 after GIF, 4 to Send. GIF/Emoji take `selected` while their tab is open. |
| Send | 28 square, radius 6. Empty: no fill, send glyph `text_disabled`. Sendable: `accent`, white glyph. Edit: check. Tooltip "Send  ↩" plus " · Silently ⌥↩" on Discord and Telegram. Right-click: the send menu (§5). |

**Per network.** A control the account cannot use is not laid out at all: neighbors close up, no ghost, no lock.

| | Attach | GIF | Emoji | Send | Picker tabs | Per-chip options | Silent ⌥↩ | Reply ping | Link chip |
|---|---|---|---|---|---|---|---|---|---|
| Discord | yes | yes | yes | yes | Emoji · GIFs (Stickers at P2) | Spoiler; Send original (HEIC) | yes | yes | yes |
| Slack | yes | — | yes | yes | Emoji | Send original | — | no reply mode | yes |
| Telegram | yes | yes | yes | yes (voice P2 swaps it while empty) | Emoji · GIFs · Stickers | Spoiler (photo/video); Send as file; Send original | yes | — | yes |
| Teams | yes | — | yes | yes | Emoji | Send original | — | — | — |

Slack/Teams GIFs wait for their client calls (`00`); Teams importance is cut; edit removes Attach and GIF (§6); signed
out, read-only and slow mode replace the card (`04`).

## 2. Attachments

**Ways in.** ⌘U (file panel, multiple) · ⌘V of a screenshot ("Pasted image 9.41.12.png", written to `outbox/`) or of
Finder files · drop. Clipboard order: Finder paths, then an image with no text entry, else text (a browser "copy" that
carries both stays text). Nothing uploads at staging.

**Drop target (frame 3).** The whole conversation pane, composer included; a thread pane is its own target. On drag
enter: an inset 8 frame, radius 10, 2 `accent` border, `accent_wash` fill, over the messages (no reflow). Centered:
paperclip 20 `accent_text`, 8, "Drop to attach to #showcase" 16/600, 4, "Up to 10 files · 20 MB each on Discord" 13
`text_soft`. Every path a folder: border `danger`, `danger_soft` wash, "Folders can't be sent. Zip it first." Read-only
conversation: no frame at all, and the drop does nothing. A mixed drop stages files and adds one refused folder chip.

**Tray (frame 4).** Inside the card above the input row: pad 8 8 0, gap 8, one row, scrolls sideways (thumb rule S§2).

| Chip | Spec |
|---|---|
| Image | 64 × 64, radius 8, cover-cropped, decoded at 128 px off the main thread. Video: still + 11/500 duration bottom-left on black 60%. |
| File | 64 high, 160–280 wide, radius 8, `field` fill + `border`, pad 8: type tile 36 (radius 6, `selected`, Lucide `file-text`/`file-archive`/`file-video`/`file-audio`/`file-code`/`file` 16 `text_soft`), gap 8, name 13/18 `text` (middle ellipsis), "PDF · 2.4 MB" 12/16 `text_soft`. |
| Hover / focus | Remove: 20 square, radius 6, `overlay` + `border`, × 12, top-right inset 4 (hit 28). Options ⋯ same size left of it, only where an option applies. Both are drawn always, `text_soft`; hover makes them `text`. |
| Tag | A set option: 16 high, radius 4, black 60%, 11/500 white, bottom-left inset 4: "Spoiler", "File", "JPEG". A spoiler image is also dimmed 45% with eye-off 16. |
| Refused | `danger` border, `danger_soft` fill; second line in `danger`: "Too big for Discord · limit 20 MB", "Folders can't be sent", "Not on disk anymore". Send stays enabled; Return shows the caption "Remove clip.mov to send. It is 84 MB." |

**Options menu** (⋯ or Space on a focused chip; P1§ menu, check slot): Mark as spoiler · Send as file (Telegram,
image or video) · Send original HEIC (shown on converted chips; the chip says "JPEG" until checked).

**Limits in words.** From `Capabilities` (`09` §7); a boosted Discord server's larger limit wins there. Count: Discord
and Teams 10, Slack 10 shown at a time; on a full tray the next file is not added and the caption says "Discord sends
up to 10 files at once. 1 was not added." Telegram takes more and splits: caption (`text_soft`, info 12) "Sends as 2
albums" or "Sends as 2 messages: photos, then files". Caption = the typed text; over Telegram's 1,024 the caption line
says "The text goes as its own message first."

**Chip keys.** ⇧Tab, or ← at the field's start, focuses the last chip (2 inset `focus`); ←/→ move, Backspace removes,
Space opens options, ↓/Tab/Esc return. **Teams:** a non-image file's chip says "Needs OneDrive access" + ghost
"Allow…" (sign-in sheet, second token, `00`); a tenant refusal makes it refused: "Your organization blocks file sharing".

**Sending (frame 5, ruling 9).** Return makes a pending row and frees the composer at once. Row: body 55%, files in
final boxes from the local paths, `media_dim` until each lands; then a 4 bar (white 10% track, `accent`, 240), 8,
"Uploading 2 of 3 · 3.1 of 5.4 MB" 12 `text_faint` tabular, 8, × 20 "Cancel upload" (Esc on the focused row). The bar
moves only on `Upload` (≤ 4/s). Cancel → Failed "Upload cancelled". Offline at Return: nothing leaves; caption
(wifi-off `warning`, words `text`) "Files need a connection. Your text and files are kept." Link lost mid-upload:
"Waiting for Telegram…", bar frozen, resumes within 60 s, else Failed "Not sent · photo.jpg didn't upload: connection
lost." Retry (only what did not land; Telegram from its last part) · Delete.

## 3. The one picker (frames 6–9)

**Shell.** P1§ popover: `overlay`, 1 white 10% edge, radius 10, popover shadows, 352 × 400, opaque, fades 120/80.
**Fit** (fixes audit S1): 8 from every window edge. From the composer: above the card, 6 gap, right edge on the
trigger's right edge, shifted left to fit. From a reaction: below the toolbar or chip, else above, else the taller
side, height shrunk to the space (≥ 240, the grid scrolls). Width never shrinks. Built on open, dropped on close.

| Band | Height | Content |
|---|---|---|
| Tabs | 40 | Segmented control 28 (radius 6, pad 2, segment 4) at pad 6 10: "Emoji · GIFs · Stickers", only the tabs this account has. Absent with one tab (Slack, Teams, every reaction). |
| Search | 36 | Search field 28, pad 0 10 8. Focused on open. "Search emoji" / "Search GIFs on Discord" / "Search stickers". |
| Strip | 32 | Emoji: category glyphs 16 in 28 buttons (clock, server icon 16 radius 4, smile, leaf, cup, ball, car, bulb, heart, flag); current `selected`. Stickers: 28 pack thumbs (clock, star, packs), scrolls sideways. GIFs: none. |
| Grid | rest: 256 at 400, 96 at 240 | One `uniform_list`, pad 0 10. Section label 24, 11/500 `text_soft`, sentence case. |
| Footer | 36 | Hairline above, pad 0 10: preview + name, right-side control. |

**Emoji.** 9 × 36 cells, glyph 22 (system emoji font), radius 6, hover/keyboard `selected`. Sections: "Frequently used"
(frecency, 18, per account in SQLite; feeds the toolbar's 3 slots); this Space's custom emoji under its name and 14
icon; other servers' only with Nitro; the nine Unicode groups (~1,900, static table). Unsendable emoji never appear
(Telegram custom: Premium only; Telegram reactions: the chat's allowed set). Search: one flat ranked grid; none: "No
emoji match “zzz”". Footer: glyph 22, `:name:` 13, Space 12 `text_soft`; right, skin tone 28 → six 24 swatches in the
footer. Offline works; uncached custom emoji draw as `:name:` 11 tiles.

**GIFs.** Justified rows 96 high (aspect kept, scaled to fill 332, gap 4, radius 6): one row height, so still a
`uniform_list`, no cropping. Empty query: "Favorites" (Discord) / "Saved" (Telegram), then "Trending" (Discord). Stills,
a `raised` box at final size first. **Play** (GPUI notes): only the hovered or keyboard-selected cell, after 300 ms
rest, one at a time; stops on leave, blur, Reduce Motion. It is content the user points at, the only way to judge a
GIF; P1§7 governs chrome. Telegram's (MP4) stay still in v1. Star 24 top-right on hover toggles favorites. Click/Return sends now with the reply target; typed
text stays. Footer: "Searches go to Discord · via Tenor" (provider from the result) / "Searches go to @gif"; while
asking, "Searching Discord for “cat”…" over the old results. None: "No GIFs for “cat”". Offline: empty state "GIFs need
a connection" / "Favorites come back with Discord." Query waits 300 ms and 2 characters; the next one cancels.

**Stickers** (Telegram). 4 × 80 cells, still 72, radius 6. Sections: "Recent", "Favorites", then the pack picked in
the strip under its name ("Forest Folk · 24"). A pack loads when its thumb is clicked, never on scroll. TGS and WEBM
show their thumbnail and never play. Click sends at once. Search filters loaded packs by name and sticker emoji.

**Keyboard.**

| Key | Does |
|---|---|
| ⌘E / ⇧⌘G | Open on Emoji / GIFs; again on the same tab closes; from the other tab switches. |
| Tab / ⇧Tab | Next / previous tab (field keeps focus). |
| ↓ from search, arrows | Into the grid; arrows move by cell (GIFs: by cell along the row, ↑↓ to the nearest cell). ↑ on the first row returns to search. |
| ⌘↑ / ⌘↓, Page Up/Down | Previous / next section. |
| Return | Insert (emoji) or send (GIF, sticker); closes. ⇧Return inserts and keeps it open. |
| F | Star or unstar the selected GIF or sticker. |
| Esc | Closes; focus returns to the composer (`07` R12). Typing in the grid goes to search. |

## 4. `:`, `@`, `#` and the large ping (frames 10–11)

One popover (P1§): above the card, left-aligned, 340 wide, pad 4, rows 28 (people 36). `:` opens after two letters,
never in a link or after a digit (10:30): glyph 20, `:tada:` 13, custom rows add the Space 12 `text_soft` right; 8 rows;
frecency, prefix, keyword. Return/Tab inserts; Esc or a space with no match closes. `@` adds roles and groups after
people ("notifies 14 people"); `@here/@channel/@everyone` only where allowed; non-members last, "not in #ops".

**Large ping.** Return on a message that reaches more than 25 people (sum of `@everyone`, `@here`, roles, groups, by
member count) does not send. The card border turns `warning`, and the caption reads (triangle-alert `warning`, words
`text`, number 600): "This notifies **1,204 people** in #general. Return again to send." The second Return sends; any
edit, Esc or leaving disarms. ⌥Return on the armed message sends silently and still counts as the confirm.

## 5. Niceties (frames 12–13)

- **Silent send** (Discord, Telegram). ⌥Return or the send menu; learned from the send tooltip and menu. Seen: bell-off
  12 `text_faint` after the row's time, "Sent without notification", kept on the echo (flag 4096 / `silent`). Slack and
  Teams keep ⌥Return as a new line (changes `04`'s key table).
- **Reply ping** (Discord). In the reply mode bar, left of ×: ghost 24 "@ On" `accent_text` / "@ Off" `text_soft`,
  tooltip "Mention Mara in the reply". Last choice kept per account, as Discord does.
- **Link preview chip.** Under the card (the caption line, 6 gap): 20 high, radius 4, `hover` fill, pad 0 6, link 12,
  "Preview: example.com" 12 `text_soft`, × 12. × → "No preview: example.com" `text_faint` with "Undo" ghost. Two or
  more links: "Previews: 2 links". A caption, when present, sits right of it. Absent on Teams.
- **Send menu** (right-click or long-press Send; P1§ menu): "Send without notification ⌥↩" (Discord, Telegram),
  "Schedule message…" (Slack, Telegram, P2). Empty menu = no menu (Teams).
- **Schedule (P2).** Popover 240: "Tomorrow 9:00 AM", "Monday 9:00 AM", "Custom…". Then mode bar "Scheduled for Tue
  9:00 AM" ×, send glyph clock. The network holds it; tinyclient runs no timer.

## 6. Edit and reply with attachments

Card order: mode bar 32, tray, input row. **Reply**: everything as usual. **Edit**: text only on all four; Attach and
GIF are absent, draft chips stashed and restored (04). Caption edits work everywhere (Telegram `editMessageCaption`);
removing a file (Discord) and replacing a photo (Telegram) are P2.

## New tokens

| Token | Value | Use |
|---|---|---|
| `media_dim` | black 45% | Uploading and spoiler chips, uploading images in a pending row |
| `media_tag` | black 60% | Chip tags, video duration |
| `PICKER_W` / `PICKER_H` / `PICKER_MIN` | 352 / 400 / 240 | Picker fit (P1§4) |
| `GIF_ROW` | 96 | Justified GIF row height |
| Glyphs | `gif-16`, `file-text`, `file-archive`, `file-video`, `file-audio`, `file-code`, `leaf`, `coffee`, `volleyball`, `car`, `lightbulb`, `heart`, `flag` (14/16 each) | |

## GPUI notes (rev `cb13b37`)

- **Drop**: `on_drop::<ExternalPaths>` on the pane, and `drag_over::<ExternalPaths>(|style, paths, …|)` for the frame;
  the platform delivers paths on `FileDropEvent::Entered`, so folders are known before the drop. The frame is a
  `deferred` absolute child so the list does not reflow. `Exited` clears it.
- **Clipboard**: `read_from_clipboard()` entries are `String`, `Image` (with `ImageFormat`) and `ExternalPaths`; walk
  them in §2 order. Image bytes are written to `outbox/` on a background task; the chip keeps the path only.
- **Linux selection paste**: middle-click inserts PRIMARY's text at the clicked
  position and focuses the field, without sending a message or replacing another
  selected range. PRIMARY is separate from the Copy/Paste clipboard; selecting
  text in a field publishes it for middle-click paste. Word, line, and Shift-click
  selections publish on press; only a plain click starts a drag and publishes
  on release, so a stationary move cannot shrink a word or line selection.
  An empty selection does
  nothing. This also works in single-line fields, which flatten pasted newlines.
- **File panel**: `cx.prompt_for_paths` (files, multiple). **Progress**: bar width from the last `Upload`; no timer.
- **Picker**: `deferred(anchored().position_mode(..).snap_to_window_with_margin(px(8.)))` gives shift; flip and shrink
  are computed from the trigger bounds before layout. Grid = `uniform_list` of rows (9 emoji / 1 justified GIF row / 4
  stickers), so ~12 rows exist; images decode in `Images` only for visible rows and drop on scroll-off; the entity and
  `media/<net>/pick/` die on close.
- **Still-image decode**: media cache filenames have no extension. On Linux and
  Windows, detect the image format from its bytes, as on macOS. Thumbnail decoding
  shrinks large sources but never enlarges a smaller source to the requested edge.
- **GIF decode rule**: every cell is a first-frame still (`images.rs` reads index 0 only). The one hovered cell swaps to
  `img()` of the provider's smallest GIF rendition (Tenor `tinygif`, ~220 px): this rev's `img()` steps frames itself
  and stops under `reduce_motion()`. Played only if width × height × 4 × frames ≤ 8 MB, else it stays still; dropped
  on leave. Telegram's saved GIFs are MP4 and the shell has no video decoder: stills only in v1.

## Deliberately absent

Disabled or locked controls and upsells. A formatting bar. Uploading at staging. Locking the composer during upload.
Autoplaying GIF grids and animated stickers. Our own GIPHY key. GIFs on Slack and Teams. Teams importance, Loop and
Urgent. Slash commands (`/` posts as text with the caption from `09`). Voice recording (P2). Image editing or crop.
Inline images in the field. A sticker button on the card. Per-emoji skin-tone long-press. A send sound. Spinners.
