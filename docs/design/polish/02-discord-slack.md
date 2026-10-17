# Polish 02 · The Discord home and the Slack home

Every list a Discord or Slack user lives in. Requirements: `docs/product/polish/00-rulings.md`, `01-discord.md`, `02-slack.md`, `05` §6, `07`. Mockup: `mockups/02-discord-slack.html` (frames 1–11, 1120 × 720, including before/after against `--seed-demo` shots of the current build). Tokens, type, radii and components are `docs/design/01-system.md` as amended by `polish/01-system.md` (C1–C11 followed here). Row heights are 28 and 48 (ruling 3 of `docs/design.md`); the 32/64 rows of `02-layout-navigation.md` §3 are overruled here.

## 1. What the current build gets wrong (shots `discord`, `discord-many`, `slack`, `space0`)
One weight for everything: DMs are one-line 28 rows with no status, capped at 6, while 50 servers get all the room. Every server row has a chevron that opens a tree inside the list. Folders are gone. Slack expands every workspace at once and lists threads as tree rows ("↳ Thread in eng-platform", "↳ Deleted message"). Mentions read "@2", "5" or a grey "@1"; muted rows carry a bell in the badge column. Nobody in voice is visible until you scroll a server.

## 2. Shared metrics (both homes, the server tree, Threads)
| Part | Spec |
|---|---|
| Sidebar | 316 wide. Title strip 52, jump strip 44 (field 28 + compose 28, 16 side padding), then one list. Rows inset 8 from the pane, radius 6, padding 8. |
| Title strip | Left: network icon or place icon 20, name 16/600, chevron-down 12 `text_faint`; the name and chevron are one 28-high button that opens the menu for this place. Right: segmented `All · Unread n` (28; counts are `text_faint` 11 and hidden at 0). The tree and Threads have `‹` 24 first and no segment. |
| One-line row (28) | Leading box 20 (glyph 16 centred, or avatar/icon 20), gap 8, name 13/18, trailing slot right-aligned. Names start at x 36 in the row everywhere, so channels, DMs and servers line up. |
| Two-line row (48) | Avatar 32, gap 10, name 13/18 over line 2 12/16. Line 1 right: time 11 (where shown). Line 2 right: badge. |
| Section header (28; 24 when first in the list) | Label 11/500 `text_faint` at text start, baseline 8 above the first row. Chevron 12 after the label, drawn on hover or when collapsed. Collapsed with hidden unread: the dot in the right slot. No count, no rule. |
| Indents | Folder children +12. Sub-rows (threads, voice occupants, "n more threads") +28, so their glyph sits under the parent's name. |
| Badges (system C6) | Mention of me: `accent` pill, white numeral, never "@", also on muted rows. Messages to me (DM, group DM, new thread replies): unfilled count, `text_soft` 600 (`text` on an unread row). Ambient unread (channel, server, app, folder, collapsed group, other workspace): name 600 plus an 8 dot `text`. Nothing else. |
| Tag | 16 high, radius 4, padding 4, 11/500 `text_soft` on `quiet_fill`: `APP`, `SYSTEM`, `Guest`, an org name, `Deactivated`. |

Row states (one-line and two-line alike): **rest** `text_soft` · **hover** `hover` fill, trailing slot may swap content in place (never shifts a neighbour) · **selected** (the open conversation) `selected` fill and `text` · **unread** name `text` 600, line 2 `text`, a dot on places without a count · **mention** as unread plus the accent badge · **muted** everything `text_faint` at 400, no glyph (the bell lives in the row menu), a mention keeps its accent pill · **typing** line 2 "typing…" in `text_soft` (dots stepped per `06`) · **in a call** line 2 `accent_text` glyph + words, presence is Busy.

## 3. Discord home (`--mode discord`), frames 1–2
Top to bottom, one scroll:
1. **Friends** (28): users glyph, "Friends", trailing "6 online" 12 `text_faint` and incoming requests as a count badge. Opens People scoped to Discord, Online tab.
2. **Message requests** (28, only when non-empty): mail glyph, trailing a bare `text_faint` number. Never a badge, never counted anywhere.
3. **In voice** header + rows (48, only while a friend is in voice or a DM call, at most 3 then "n more"). Lead: two faces stacked (group avatar). Line 1: speaker glyph 14 `accent_text`, channel name, then " · Server" `text_faint` (the server truncates first). Line 2: "Ines, Tomasz, Yusuf" or "Ines, Tomasz +3". Hover: a small secondary **Join** button (24) in the trailing slot. Click opens the channel; only Join joins. A DM call row: the person's avatar, phone glyph, "Call in your DMs".
4. **Direct messages** header + DM rows (48), Discord's order. Every DM with a number, then the newest up to 10, then "Show n more" (28 action row, expands in place for the session).
5. **Servers** header + server and folder rows (28) in the user's order.

**DM row**: avatar 32 + presence 10 (Busy when in a call); name (friend nickname › global name › username); line 2, first that exists: in a call (`accent_text`) › typing › activity ("Playing Hades", "Listening to Spotify") › custom status (emoji + text) › last message (`text_soft`, "You: " for mine). No line 2: one line, centred. Hover: time on line 1, close × (20 box) on line 2 when there is no badge. **Group DM**: two faces 21 on the diagonal (front ringed 2 in the row colour), name or "Rosa, Bea +2", line 2 "4 members" unless typing or in a call. `APP` / `SYSTEM` tag after a bot's or Discord's name.
**Server row**: icon 20 squared radius 5 (initial on tint when none); name; trailing friends-in-voice (speaker 12 + n, 11/500 `text_faint`), then the rollup: mention pill, else the unread dot. Muted: faint in place; a direct @me keeps the pill. No chevron, no inline tree: a click opens the server in the sidebar and back returns to the same scroll.
**Folder row** (28): a 3 × 16 bar in the folder's own colour at x 8 (`text_faint` if none), name or the first three server names, chevron after the name. Starts collapsed. **Collapsed, it still lists** the open server and any server holding a number, indented 12 on a 3 pt spine in the folder colour (70%) that runs from the bar down to the last visible child. Plain unread hidden inside: the folder name turns `text` 600 with the dot. The header never repeats a number already shown below it. Hover: server count `text_faint`.
**50 servers** (frame 2): folders collapsed, the Servers header sticks under the jump field, muted servers faint. `Unread` keeps Friends (when requests exist), DMs with a number, servers and folders with news; 50 servers became 9 rows in the seed.

## 4. A Discord server's tree (`--mode space`), frames 3–4
- **Title strip**: `‹`, icon 20, name, chevron (server menu: Mark as read, Mute, Hide muted channels, Pin to rail, Server info, Open in Discord). No banner, boost, member or online count.
- **At most one row under it**: a live event or one within 24 h (calendar glyph, title, time `text_faint` right). P2.
- **Order** is Discord's (`position`, text before voice). Category headers are section headers. No unread-first sort.
- **Kinds, by glyph only**: `#` text · megaphone announcement · speaker voice · radio stage · two-bubble forum · image media. **Private**: the kind glyph with a lock 12 at its lower right on a 1 pt plate of the row colour. **Read-only**: no row mark; the conversation replaces the composer with a line. **Muted**: faint, no glyph. **NSFW**: unchanged row (the gate is in the conversation).
- **Followed threads** (only joined and unread or open, at most 3): corner glyph 14, name, count badge; then "n more threads" action sub-row opening the list in the right pane. A thread whose starter is gone uses its own name.
- **Voice**: occupied channel `text_soft` with `4/12` when it has a user limit; occupants as 28 sub-rows (avatar 20, server nickname, trailing mic-off / deafen 14 `danger`, screen share or camera 14 `text_soft`), 6 then "+n", friends first. Empty voice channel: `text_faint`, nothing trailing. Stage: "Live · 2 speaking" `text_faint`.
- **Footers** (28 action rows, `text_faint`): "Show 41 muted channels" when *Hide muted channels* is on in Discord (reveals for the session); "Browse 212 more channels" for onboarding opt-outs (from the stored tree).
- **Collapse**: muted categories start collapsed; over 80 visible channels, every category without a number starts collapsed. Collapsed still shows the open row, unread unmuted channels, rows with a number and voice channels with people. The user's own collapse wins after and is kept per server.
- **300 channels** (frame 4): 10 headers, 9 rows that need you, 2 footers = 21 rows, 588 pt, one screen.

## 5. Slack home (`--mode slack`), frames 5–6
- The rail button opens the last workspace directly. **Title strip**: workspace icon 20, name, chevron; with several workspaces, an accent badge after the chevron counts mentions in the others, else a 6 dot for plain unread there. `ctrl-1…5` switch.
- **Header menu** (popover 280, radius 10, items 28): *Workspaces* (only when more than one: icon 20, name, mention badge or dot, link state in words "Offline", shortcut `⌃n`; current highlighted) · own block (avatar 32 + presence, name, status line) · Update your status · Pause notifications › (30 min, 1 h, until tomorrow; then "Paused until 3:40 PM") · Set yourself as away / active · Mark all as read `⇧Esc` · Open in Slack · Add a workspace… · Sign out of *Workspace* (`danger`). With one workspace the menu keeps everything but the workspace list (the chevron stays, because the status items live there).
- **List**, all 28, one `uniform_list`: **Threads** row (forum glyph, count of threads with new replies, accent when one mentions me) · the user's sections in Slack's order · **Channels** · **Direct messages** by recency, self-DM first as "Name (you)" · **Apps**, collapsed, a dot when unread, a mention pill only for a direct mention.
- **Channel row**: `#`, lock (private) or building glyph (Slack Connect); name; trailing huddle (headphones 14 `text_soft`), local draft (pencil 14 `text_faint`), badge. A–Z without the `#`; **muted sorted last** in its section, faint. "n quiet channels" (28 action row) ends a section when channels idle 30 days are folded; open, starred and unread never fold.
- **DM row**: avatar 20 + presence 8; name; status emoji 14 after the name; tag (`Guest`, the other org's name); count badge. Deactivated: name `text_disabled`, no presence, `Deactivated` tag. Hover tooltip: status text, local time "3:40 PM, 9 h ahead". **Group DM**: two faces 14 in the 20 box, "Mara, Dev, +2". **App row**: icon 20 squared, name, weight when unread, no count.

## 6. Slack Threads (frame 7)
Opens in the sidebar from the Threads row (`‹ Threads`, workspace name `text_faint` at right). One `uniform_list` of 48 rows, newest reply first. **Row**: lead = the last two repliers stacked; line 1 `#channel` (the `#` `text_faint`), time right; line 2 the root's first line ("Original message deleted" when gone); badge = new replies (accent when one mentions me). Unread rows are weight. Enter opens the thread in the right pane with its channel as the conversation; moving through the list replaces the pane (`02` R18). A read row stays until the view is left.

## 7. Consistency between the two homes
| Same in both | Each keeps its own |
|---|---|
| Strip anatomy (place name + chevron menu, `All · Unread`), jump strip | Discord: Friends, Message requests, In voice, Servers, folders. Slack: Threads, sections, Apps, quiet channels |
| 28 and 48 rows, x 36 name line, section header, action rows, badges, tags | Discord DMs are 48 (who is around matters); Slack DMs are 28 inside sections (people are filed with channels) |
| Unread = 600 + dot; to me = unfilled count; mention = accent pill; muted = faint, no glyph | Discord keeps muted rows in place; Slack sorts them last (each network's own rule) |
| Collapsed groups never hide what needs me | Discord's words: server, category, friends. Slack's: workspace, section, huddle, Connect |

## 8. Empty, loading, error (frames 8–9); nothing animates
| List | Empty | Loading / first sign-in | Error |
|---|---|---|---|
| Discord home | "No servers yet" · "Servers you join in Discord show up here." (no DMs: the DM section is not drawn) | Strip right: "Getting your servers…"; rows land in place top to bottom; unfilled slots are static skeletons in final geometry (48: circle 32, bars 10/8; 28: square 20, bar 10) after 150 ms | Banner 32 "Discord is offline · saved at 10:42" (`warning` wifi-off); signed out: `danger` glyph + "Sign in" small button. Saved rows stay usable |
| Server tree | "No channels you can read" | six 28 skeletons, bars 60/45/70/50/65/40% | "You're no longer in Driftwood" · "Its channels stay readable from what was saved." · Remove from list |
| Slack sidebar | "You're not in any channels yet" · Open in Slack | as Discord, "Getting Harbor Labs…" | banner, as Discord |
| `Unread` segment | check glyph, "Nothing unread", "3 muted channels have new messages" · Show them | n/a | n/a |
| Threads | "No new replies" · "Threads you follow show here when someone replies." | five 48 skeletons | banner "Couldn't load threads" + Try again over saved rows |

The first Discord home remains in its loading state until its chat read finishes,
even when cached servers arrive sooner. DMs then appear above servers in the
normal order. A completed empty DM read still shows servers. Once the home has
loaded, background refreshes keep its existing rows visible, including when
returning from a sublist.

Network homes are cached for the GUI session: switching back displays saved
chats and folds immediately, then refreshes from chatd in the background.
Explicit navigation skips the event-burst debounce. A network opened for the
first time still waits for its local chat read. Returning from a sublist restores
the completed home's loading state, including a previously empty home.

## New tokens
For reconciliation with `polish/01-system.md`: `LEAD 20` (leading box of every 28 row), `NAME_X 36`, `INDENT_CHILD 12`, `INDENT_SUB 28`; `tag` component (16, r4, 11/500, `text_soft` on `quiet_fill`, which survives here only as the tag fill); `folder_spine` (the folder's own colour at 70%, 3 wide); `sticky_header` (`sidebar` fill, no rule, per C8). **Divergence to reconcile**: C9 puts DMs in homes on 48 rows; Slack DMs stay 28 because they sit in the user's sections beside channels in one `uniform_list` (Slack's own structure, ruling 1). Discord DMs are 48. Glyphs added: mail, phone, radio, image, building-2, calendar, headphones, headphone-off, monitor, eye, list, moon, smile, log-out, external-link, check-check, wifi-off.

## GPUI notes
- **One row height per `uniform_list`.** Server tree and Slack sidebar: `uniform_list` at 28. Threads: `uniform_list` at 48. The Discord home mixes 48 (DMs, In voice) and 28: it is a `list` with `ListState`, every item's height known from its kind, so nothing is measured; about 80 items. Folds call `ListState::splice` on the changed range only.
- **The list is a flat `Vec<Row>`** rebuilt by one flattener on collapse, filter or data change. The "collapsed still shows what needs me" rule lives in the flattener (child kept if open, has a number, unread-unmuted, or voice with people) and sets the spine's first/last flags. Diff by row key, not index (`08` S2 on full rebuilds).
- **Sticky header**: an absolutely positioned 28 div over the list top, drawing the header of the section that contains the first visible row (index = offset / 28 for uniform lists, `logical_scroll_top()` for the home). When the next header is within 28 of the top, the sticky div's top is `next_y − 28` (pushed, not animated). No hairline (C8).
- **Fold rows** ("Show n more", "n quiet channels", "Show 41 muted channels", "n more threads") are ordinary 28 rows that toggle a session flag; the flag lives in the shell per place, not in chatd.
- **Hover swaps** happen inside a trailing slot of fixed width so the name never re-truncates. Presence and the group's front face take `theme::over(fill, sidebar)` as their ring per state (#121315, #202123, #2E2F31).
- The private-channel lock and the spine are child divs; no masks, no blur. Typing dots are the only timer, in their own entity.


### Discord server columns

At wide window sizes, selecting a Discord server keeps the Discord overview
visible in a 280 pt column and opens its channels in the regular sidebar beside
it. The overview retains DMs, folders, expansion state and scroll position;
selecting another server replaces only the channels column and highlights that
server immediately. Choosing a DM returns to the Discord home and hides the
channels column. Message requests likewise return to the home before drilling
into their list. Home data continues refreshing while a server is selected.

The extra column appears only when the rail, both lists, the 392 pt minimum
conversation and any visible right pane fit. Otherwise the original single
sidebar and back navigation apply. The existing sidebar resize handle changes
the channels width; the overview width is fixed. Other networks keep their
existing layout.


## Deliberately absent
A server-icon column; chevrons and inline trees on server rows; banners, boosts, member and online counts; role colours; "@" in mention badges; a grey muted-mention look; mute bells on rows; elapsed timers and rich-presence art; time on Discord DM rows at rest; a read-only mark; unread-first sorting; prefix grouping; Slack's Unreads, Activity, Later, Huddles and Canvases views (Open in Slack); total reply count in a Threads row (in the pane header); any animation of rows filling in.
