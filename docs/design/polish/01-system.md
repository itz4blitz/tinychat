# Polish 01 · The system, consistency and app chrome

The rules the other six polish designers draw with. Mockup: `mockups/01-system.html`. Sizes in points. Base:
`docs/design/01-system.md` (cited **S§**); only section 3's table changes it. Inputs: product polish `00`, `05`, `07`, `08`.

## 1. Calm by default: rules you can check on a screenshot

"Thrown all at you" is too many signals per row in too many shapes: a current Inbox row has a colored logo, Space,
`›`, channel, pin, time, `@n` pill and a preview starting with my own name. Eight marks; the target is four.

| # | Rule | Check on a screenshot |
|---|---|---|
| C1 | **One emphasis per row.** Unread is the title at 600 plus one badge. No accent time, no colored title, no second badge. | Squint: each row has at most 2 colored marks (avatar with presence, one badge). |
| C2 | **Secondary text recedes in two steps.** Title `text`; preview and read one-line titles `text_soft`; time, context (` · Harbor Labs`) and section labels `text_faint`. Unread lifts the preview to `text`. | At most 3 text colors per row (S§4.7); never `text_faint` on `selected` or `overlay`. |
| C3 | **Four signals at most per row**: avatar (presence included), title line, one right-side mark per line (time on line 1; badge, dot or state glyph on line 2). | Count marks right of the title: ≤ 2. |
| C4 | **An icon earns its place** only when it names a kind the avatar cannot (`#`, speaker, `↳` thread, lock) or an abnormal state (red failed, pencil draft, red missed call). Never decorative, never repeating a word on the same row, never two leading icons. Pin and muted bells are not drawn: the Pinned header and the dimmed text say them. | No row has two glyphs before its title. |
| C5 | **Network glyph**: in a list that mixes networks only, and only while 2 or more networks are signed in. 14, monochrome `text_faint`, leading the title. Full color only on the rail, the conversation strip, the account menu and sign-in. | No colored logo inside a list row. |
| C6 | **Counts, dots, nothing.** Mention of me: filled accent pill with the numeral. Messages to me (DM, group DM): unfilled count. Ambient unread (channel, Space, network button, collapsed group, apps): dot. Muted: nothing, except a mention, which keeps the same accent pill. Read: nothing. | One badge style per meaning; no `@` inside a badge. |
| C7 | **Muted** = every text on the row `text_faint`, weight 400, no glyph. A muted mention keeps its pill. | Muted rows read as one grey; pill is the only color. |
| C8 | **Groups separate by space, not rules.** A section header (28) gives the gap; there are no hairlines between rows or groups inside a list. Hairlines exist only between panes, under a title strip, above a list's fixed footer, and between menu groups. | No horizontal line inside a scrolling list. |
| C9 | **Density.** Two-line rows (48) where line 2 carries something to read for most rows: conversations with a preview, People with status. One-line rows (28) for places scanned by name: channels, servers, folders, menus. People inside a pane (members, switcher, menu headers) use 36. One list, one height: a two-line row with nothing for line 2 stays 48 with the title centered. | Rows in one list are equal height. |
| C10 | **Rhythm** on the 4 grid: list inset 8, row padding 8, so text starts 16 from the pane edge (24 with an avatar column); 2 between lines in a row; 0 between rows; 12 above each group. | Left text edges line up across every list in a pane. |
| C11 | **Truncation**: one ellipsis per line. Context truncates first and is dropped when fewer than 6 characters would remain; the title never truncates before its context. Times, badges and glyphs never shrink. | No `H… ›`, no `Fe… ›`. |
| C12 | **A preview that starts with my own mention drops it** (`@Robin Vale need a yes` → `need a yes`). Sender prefix only in groups and channels (`Mara: …`); mine is `You: …`. | No preview begins with the user's name. |
| C13 | **Hover reveals in place.** Row actions (Message, Call) replace the right-side marks inside the same box; nothing shifts. | Hover and rest frames differ only in fill and that box. |
| C14 | **Words**: sentence case, no all caps, `›` as the only path separator, `…` only when a dialog or more input follows, never "chatd", "Space", a Rust value or a command. Network words inside a network's home ("server", "workspace"). | Grep the copy. |

## 2. The parts, one of each

**Row family** (radius 6, inset 8, padding 8; states as S§3 List row).

| Row | Height | Lead | Line 1 | Line 2 | Right | Used by |
|---|---|---|---|---|---|---|
| Place (one-line) | 28 | glyph 16 or avatar 20 (place: rounded square), gap 8 | name 13 | none | dot, pill or count, 16 high | channel and server trees, folders, Apps expanded, menus |
| Person (mid) | 36 | avatar 24, gap 8 | name 13, then ` · context` 12 `text_faint` on the same line | none | badge or hover actions | member lists, switcher, menu headers, pickers of people |
| Conversation (two-line) | 48 | avatar 32, gap 10 | glyph 14 (C5) + title 13 + ` · context` faint | preview 12/16 | time 11 top, badge bottom | Inbox, Telegram/Teams chat lists, DMs in homes, People |
| Tree indent | | +16 per level; a 16 chevron slot reserved at every level of a collapsible tree, `text_faint`, before the lead | | | | Discord, Slack, Teams trees |
| Fixed footer | 32 | none | 12 `text_faint`, links `accent_text` | | | "Older conversations are in each network" |

**Section header.** Height 28, label 11/500 `text_faint`, sentence case, at text start (x 16), baseline 8 above the
first row, so 12 of the 28 is the gap. No count, no rule, no fill. Collapsible: a 12 chevron right of the label, on hover
and while collapsed; a collapsed header with hidden unread shows the dot in the right slot (ruling 3 still lists the
rows that need me). The first header in a list is 24. Day headers in the Inbox are this header.

**Title strip.** Every pane starts with one: 52 high, drags the window, title 16/600 at x 16 on the strip's baseline 32,
actions as 28 icon buttons 4 apart, 12 from the right edge. Conversation and right pane strips have a hairline under
them; the sidebar's does not (its field below separates, S§4.4). Sidebar, People, network homes and the right pane all
use the same 16/600, with no grey count beside it. The sidebar adds one 44 row under its strip: the field, full width
minus 44, and one 28 icon button right of it (compose in Inbox and homes, scope in People). Same field width everywhere.

**Badges** (all 16 high, 11/600, tabular): *Mention* accent fill, white numeral, radius 8, min width 16, padding 5.
*Count* no fill, numeral `text_soft` 600 (`text` on an unread row), right aligned, same box. *Dot* 8, `text` for unread,
`accent` when a mention is inside; on an icon it overhangs top-right with a 2 ring. Rail: Inbox carries the one accent
count; network and pinned buttons a dot; the Dock badge equals the Inbox number. `99+` past 99.

**Menu.** One style for context, button and account menus: S§3 Context menu, items 28 single line, never two-line.
A 16 leading slot is reserved in every item when any item has a glyph, so labels align. Current choice: a check in that
slot (no radio circles, no fills). Shortcut right, 12 `text_faint` → `text_soft` on highlight. A menu may start with one
36 Person row as its header, then a separator. The row or message that owns the menu keeps `selected` and its hover
toolbar hides. Destructive items sit last after a separator, `danger` text, `danger_soft` highlight, and are never
the only item (`menu-net` gains "Open in Discord" and "Account settings…" above "Sign out"). **Popover**: S§3, 200–360
wide (pickers declare their own size), 6 from the anchor, no arrow.

**Message action bar.** Hover ownership follows pointer geometry across the
message and its raised action bar. The bar's occlusion of the pane does not
count as leaving it. Moving onto a button or pressing it keeps the same message;
leaving the pane clears hover. Pointer tracking does not consume button events.

**Buttons.** S§3: primary, secondary, ghost, danger, icon. Accept = small primary; Decline = small secondary; no green
buttons, no full-radius controls (Leave, hang up, jump-to-bottom and viewer discs become radius 6 / 8 squares).
In-caption actions ("Retry") are small ghost buttons.

**Banner.** One line, 32 high, inset 8 under the strip of the pane it concerns, `raised` + `hairline`, glyph 16 in its
semantic color, text 13 `text` that truncates (full text in its tooltip), one small ghost action at the right. No tinted
fills. At most one per pane; priority: service > account needs you > signed out > reconnecting > offline.

**Empty state.** S§3: glyph 20 `text_faint`, title 16/600, one line 13 `text_soft`, at most one secondary button,
centered at 45% of the region's height. A list that is empty because of a filter says so ("No mentions"); one that is
empty because nothing is signed in offers the action.

**Dates and times.** One formatter (`gui/time.rs`; `conversation/when.rs` folds into it). 12/24-hour follows macOS.

| Age | Row (11) | Message time | Day divider | Tooltip |
|---|---|---|---|---|
| Today | `10:37 PM` / `22:37` | `10:37 PM` | Today | Tuesday, October 13, 2026 at 10:37 PM |
| Yesterday | Yesterday | `10:37 PM` | Yesterday | same form |
| 2–6 days | `Mon` | `10:37 PM` | Monday | same form |
| This year | `Oct 2` | `10:37 PM` | Friday, October 2 | same form |
| Older | `10/2/25` (locale short) | `10:37 PM` | October 2, 2025 | same form |

No relative times ("1h ago", "Last reply 1h ago" → "Last reply 9:24 PM"). Durations are words ("14 minutes").
Labels roll over once at midnight, not on a minute timer. A pending message keeps its send time.

## 3. Token and spec changes against `docs/design/01-system.md`

| What | Was | Now | Why |
|---|---|---|---|
| Count badge | white 16% fill, `text` | no fill, `text_soft` 600 (`text` on unread) | ruling 5, `05` F1 |
| Mention badge | `@n` in rows | numeral only | audit §1 |
| Muted mention | grey `@n` pill | the accent pill | one look per meaning |
| Network icon in rows | full color 14 | monochrome `text_faint` 14 (`-mono` assets) | `05` F2 |
| `STRIP` **new** | none (02 said 52) | 52 | one title strip |
| `ROW_MID` **new** | none | 36, avatar 24 | members, switcher |
| `SECTION` **new** | menu label 24 | list header 28, first 24; menu label stays 24 | `05` I4 |
| Menu choice | check, radio, fill | check only | audit §1 |
| Row focus ring | 2 outside (S) / 1.5 inset (02) | rows and menu items: 2 inset `focus`; controls: 2 outside after a 2 gap | rows abut |
| `text_step` **new** | none | −1 … +3, default 0 | ⌘+ / ⌘− |
| Tooltip delay | 500 | 500 first; 0 for 1.5 s after one closes; hides on mouse-down | macOS |
| Switcher scrim | `scrim` (02) | none; modal shadow only | it is a popover, not a modal |
| `quiet_fill` | muted mention, quiet rollup | removed from badges | C6 |
| Banner fill | tinted (built) | `raised` + hairline (S, enforced) | audit §1 |

## 4. Overlays stay inside the window

- **Fit.** 8 from every window edge. Preferred side, then flipped, then shifted along the edge; then height shrinks to
  H − 16 and the content scrolls. Width never shrinks: a 360 popover in a 540 window shifts, it does not clip.
- **Context menu**: top-left at the pointer + 2; flips left past x = W − 8, up past y = H − 8. **Button menu**: 6 below,
  leading edges aligned, flips above. Submenus: one level, beside, flipping. **Pickers** (emoji 352 × 400, GIF,
  mentions): above the composer, right-aligned to the trigger, else below, else shrink to ≥ 240 and scroll.
- **Switcher**: min(560, W − 48) wide, centered on the window, top 72, at most min(20 rows, H − 144) tall, no scrim.
- **Modal**: `scrim` over the whole window, rail and strips included (traffic lights stay live); panel centered, 40
  above center. Opening one closes menus, popovers and the switcher.
- **Stacking**, bottom to top: panes · in-pane floats (jump pill, "2 new ↑") · floating sidebar + scrim · toasts ·
  switcher · popovers, pickers, profile card · menus · modal scrim and modal · tooltips. One menu or popover at a time
  (plus its submenu); Esc closes the top one (`07` §6).
- **At 540 × 440**: menus scroll past 424; the switcher is 492; the 440 modal fits; toasts are min(340, W − 32).

## 5. Window-level states

The window never shows a blank canvas or a developer message. Rail and strips are always drawn; the last snapshot
stays on screen whenever there is one. The service is never named: users see "tinyclient".

| State | Trigger | Where | Words | Action |
|---|---|---|---|---|
| Starting | shell launched chatd, no answer yet | sidebar: skeleton rows after 150; nothing else for 3 s | (none for 3 s) then sidebar banner "Starting tinyclient…" | — |
| Keychain wait | `Status` says waiting, or no answer at 3 s with the process alive | conversation region empty state + sidebar banner | Title "Waiting for keychain access". Line "macOS is asking to let tinyclient use its saved sign-ins. Choose Always Allow. The prompt may be behind this window." Banner "Waiting for keychain access" | none (nothing we can press helps) |
| Keychain denied | read refused or locked | same | Title "tinyclient can't read its sign-ins". Line "Your accounts are still signed in. Allow access to continue." | "Try again" |
| Reconnecting | socket dropped mid-session; shell restarts chatd | sidebar banner; composer stays editable, Send waits | "Reconnecting…" · tooltip "Showing what was saved at 10:42" | — |
| Couldn't restart | 3 restarts in 30 s failed, or a version mismatch | sidebar banner (service priority) | "Not receiving messages." | "Try again"; Help › Copy Diagnostics |
| Crash restart | relaunched after a panic | toast, error style, stays until dismissed | "tinyclient quit unexpectedly and reopened." | "Copy report" |
| Account needs you | verification, captcha | that network's home and the Inbox, banner | "Discord wants you to verify your account." | "Open Discord" |
| Signed out (expired) | token rejected | home + Inbox banner; its rows stay readable | "Slack signed you out of Harbor Labs." | "Sign in" |
| Network late | adapter reconnecting > 5 s | home banner; in the Inbox too, naming it | "Slack is reconnecting. Showing what was saved at 10:42." | — |
| All signed out | no account | sidebar empty state; conversation empty state | Title "Sign in to start". Line "Discord, Slack, Telegram and Teams in one small window." | four network buttons (secondary, 32) |

## 6. Mac chrome

**Menu bar** (keys agree with `02` §7 and `07`; system-added items such as Dictation and AutoFill are left as macOS adds them).

| Menu | Items |
|---|---|
| tinyclient | About tinyclient · Check for Updates… · — · Settings… ⌘, · — · Services ▸ · — · Hide tinyclient ⌘H · Hide Others ⌥⌘H · Show All · — · Quit tinyclient ⌘Q |
| File | New Message ⌘N · Add Account… · — · Attach File… ⌘U · — · Close Window ⌘W |
| Edit | Undo ⌘Z · Redo ⇧⌘Z · — · Cut ⌘X · Copy ⌘C · Paste ⌘V · Paste and Match Style ⌥⇧⌘V · Select All ⌘A · — · Find ▸ (Search in Conversation ⌘F · Search Everywhere ⇧⌘F) · — · Emoji & Symbols ⌃⌘Space |
| View | Inbox ⌘1 · People ⌘2 · one item per network and pin in rail order ⌘3–⌘9 · — · Quick Switcher ⌘K · Hide Sidebar ⌘\ · Close Right Pane ⌘. · — · Bigger Text ⌘+ · Smaller Text ⌘− · Actual Size ⌘0 · — · Enter Full Screen ⌃⌘F |
| Conversation | Back ⌘[ · Forward ⌘] · Up a Level ⌘↑ · — · Next ⌥↓ · Previous ⌥↑ · Next Unread ⌥⇧↓ · Previous Unread ⌥⇧↑ · — · Mark as Read · Mark as Unread ⌥⌘U · Mark Server as Read ⇧⎋ · — · Show Details ⌥⌘I · Star · Mute… · — · Start Call ⇧⌘C · Open in Discord ⌥⌘O · Copy Link |
| Window | Minimize ⌘M · Zoom · (macOS tiling items) · — · tinyclient · Bring All to Front |
| Help | Keyboard Shortcuts ⌘/ · Release Notes · — · Copy Diagnostics · Report a Problem… |

Items that do not apply are disabled, never hidden; names follow the open place ("Open in Slack"). Esc is no item.

- **About** opens Settings › About (`08-first-run-settings` §7). **Settings**: own 720 × 560 window, ⌘,. Sections:
  Accounts · General · Notifications · Appearance · Privacy · Audio & video · Storage · About.
- **Close versus quit** (ruling 11): ⌘W and the red button hide the window; chatd keeps receiving and the Dock badge
  counting; the Dock icon or Window › tinyclient shows it unchanged. ⌘Q stops both; in a call it asks "End the call and quit?".
- **Frame**: saved 500 ms after a move or resize with its display; restored if that display exists and half the frame
  is visible, else 1120 × 720 centered on the main display. Minimum 540 × 440. Double-click on a strip does what macOS
  is set to. The window title is the open conversation ("#showcase · Moss & Lichen"). Full screen keeps every strip 52.
- **Text size**: each ⌘+ step adds 1 to every type size and line height, 2 to 28 and 36 rows, 4 to 48 rows; strip,
  icons, avatars, radii, spacing unchanged. Range −1 to +3, saved; Settings' Small / Default / Large = −1 / 0 / +2.
- **Light system appearance**: the window and Settings force the dark aqua appearance, so title bar, traffic lights,
  native scrollbars and file panels are dark; only the menu bar follows the system. Checked by a shot in Light.

## 7. Motion and feedback

- **Rows open on mouse-down** (`07` R1): no `pressed` step; `selected` and the new strip land in the same frame.
  Buttons, menu items and links act on mouse-up, `pressed` while held; dragging off cancels.
- **Animates, only**: menu and popover in 120 / out 80 (opacity + 4 toward anchor); modal and toast in 160 / out 100
  (opacity + 8 rise); scrollbar fade 150 after 800 idle; typing dots stepped at 400. Reduce Motion: all 0. Nothing else.
- **Tooltips**: 500 delay, appear whole, 0 delay for 1.5 s after one closes, gone on mouse-down or leave. Every
  icon-only button has one, with its shortcut in `text_faint`. Text wraps within
  the 240-point tooltip; the shortcut stays on one line beside it. Long failure
  reasons must not paint outside the tooltip background.
- **Focus**, keyboard only: rows and menu items a 2 inset `focus` ring over any fill; controls a 2 gap then a 2 ring;
  fields a `focus` border + 3 halo while focused. The list cursor is the ring, the open conversation the fill (`07` R8).
