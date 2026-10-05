# clip4 — Implementation Specification

> **For the implementing model.** This document specifies **clip4**, a Windows clipboard
> manager written in Rust. It is a ground-up rewrite of an existing C++ application,
> **clip2**, whose behaviour, data formats and hard-won bug fixes are captured here.
> Read the whole document before writing code. Sections marked **MUST** are
> non-negotiable; **SHOULD** is strongly recommended; **MAY** is optional.
>
> Section 18 ("Lessons from clip2") is the most important part: every item in it is a
> real defect that shipped in clip2 and was diagnosed in production. Do not reintroduce
> any of them.

---

## Contents

1. Product summary
2. Goals and non-goals
3. Performance budgets
4. Platform, toolchain and dependencies
5. Process and threading architecture
6. Clipboard capture
7. Data model
8. Persistence (and clip2 import)
9. Search
10. Overlay UI
11. Keyboard, hotkeys and input
12. Paste engine
13. Transforms and smart paste
14. Snippets
15. Copy from focused control
16. Settings, themes and configuration
17. Tray, single instance, startup, sound
18. Lessons from clip2 — defects that MUST NOT recur
19. Robustness, error handling and diagnostics
20. Security and privacy
21. Compatibility matrix
22. Testing and acceptance criteria
23. Deliverables and milestones

---

## 1. Product summary

clip4 is a system-tray clipboard manager for Windows 10/11. It records everything the
user copies, and lets them recall, search, transform and paste any past item from a
keyboard-driven overlay that appears on a global hotkey.

Core loop:

1. The user copies something in any application. clip4 captures every useful clipboard
   format (text, RTF, HTML, images, file lists) into history, silently and instantly.
2. The user presses the overlay hotkey (default **Ctrl+NumPadDot**). A dark overlay
   appears near the top of the screen with the history list and a separate pinned list.
3. The user types to fuzzy-search, arrows to select, and presses **Enter**. clip4 puts the
   item back on the clipboard, returns focus to the window they were in, and pastes.

Beyond that core: pinned favourites, multi-select merge, smart paste modes (clean URL,
Markdown link, plain text, HTML→text, edit-before-paste, Excel cell fill), a snippets
library with placeholders, typing an item as keystrokes for apps that block paste,
copying text out of controls that never touch the clipboard, themes, and encrypted
on-disk history.

The overlay is **two windows**: a **pinned** pane on the left and the **main** list on
the right, moved together as a pair.

---

## 2. Goals and non-goals

### 2.1 Goals (priority order)

1. **Never interfere with the user's own clipboard.** clip4 must never cause another
   application's Ctrl+C or Ctrl+V to fail, never wipe the user's clipboard on an error,
   and never leave it holding something the user did not choose.
2. **Never interfere with the user's keyboard.** No global input lag, no stuck modifier
   keys, no hotkeys that silently stop working.
3. **Never lose a copy.** Every distinct copy the user makes is recorded, including
   under contention, from slow apps, over RDP, and from apps that clear the clipboard.
4. **Never crash, never hang.** Corrupt history, malformed clipboard data, a 100 MB copy,
   a missing font, a failed allocation — none of these may take the process down or
   freeze the overlay.
5. **Fast.** See section 3. The overlay must feel instant.
6. **Feature parity with clip2**, plus the fixes in section 18.

### 2.2 Non-goals

- Cloud sync, accounts, or any network access. clip4 MUST NOT open sockets.
- Cross-platform support. clip4 is Windows-only by design.
- A plugin system, scripting, or theming beyond the presets and per-element colours
  described here.
- Replacing Windows' own Win+V clipboard history. clip4 coexists with it.

---

## 3. Performance budgets

These are acceptance criteria, measured on a mid-range 2022 laptop with 2,000 history
items (the maximum), 10% of them images.

| Operation | Budget |
|---|---|
| Process start → tray icon present | < 300 ms |
| History load (2,000 items, cold) | < 400 ms, on a background thread; tray usable immediately |
| Hotkey press → overlay first frame | < 50 ms p95 |
| Keystroke in overlay → repaint | < 16 ms p95 |
| Search, per keystroke, 2,000 items | < 10 ms p95 |
| Clipboard lock held during capture (text) | < 5 ms |
| Clipboard lock held during capture (10 MB image) | < 30 ms |
| Copy → item visible in history | < 50 ms, not on the UI thread |
| Low-level keyboard hook callback | < 1 ms, and its thread is **never** blocked |
| Idle CPU | 0% (no polling loops; event-driven only) |
| Idle working set, 300 items | < 40 MB (large payloads live on disk, see 8.3) |
| History save | Debounced 1.5 s after last change; background thread; never blocks UI |

There MUST be no busy-waiting and no periodic timers faster than 1 Hz while idle.

---

## 4. Platform, toolchain and dependencies

- **OS:** Windows 10 1809+ and Windows 11. x86-64 required; ARM64 SHOULD build.
- **Language:** Rust, edition 2021, stable toolchain.
- **Win32 access:** the `windows` crate (windows-rs). All Win32, COM, WIC, Direct2D,
  DirectWrite, DPAPI and CNG calls go through it.
- **Dependencies:** keep them minimal. The OS already provides crypto, image decoding,
  rendering and sound. Do **not** add crates for:
  - SHA-1 / hashing → use CNG (`BCryptHash`).
  - Encryption at rest → use DPAPI (`CryptProtectData`).
  - Image decode/encode → use WIC.
  - Rendering → use Direct2D + DirectWrite.
  - Audio → use `PlaySoundW` with an embedded WAV.
  Acceptable extra crates: none required. A small fuzzy-matching implementation is
  written in-house (section 9). If a crate is added, justify it in a comment.
- **Binary:** a single `clip4.exe`, `windows_subsystem = "windows"`. Resources (icon,
  click sound WAV, manifest) embedded via a build script.
- **Manifest MUST declare:** `PerMonitorV2` DPI awareness, `asInvoker`, and Common
  Controls v6.
- **Lints:** `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` in all
  non-test code. Runtime data never `unwrap()`s.

---

## 5. Process and threading architecture

clip2 ran everything on one thread, and most of its worst bugs came from that thread
being blocked (section 18). clip4 MUST use the following thread model.

| Thread | Owns | Rules |
|---|---|---|
| **UI thread** | Overlay windows, tray icon, settings dialog, the hidden main window that receives `WM_CLIPBOARDUPDATE` and `WM_HOTKEY`. | MUST never sleep, never do file I/O, never decode images, never hold the clipboard longer than a raw byte copy. Message loop is a plain blocking `GetMessageW` loop. |
| **Hook thread** | The `WH_KEYBOARD_LL` hook, and its own message loop. | Does **nothing** except classify a key and `PostMessageW` to the UI thread. Never touches application state beyond an atomic snapshot of the active hotkey bindings. |
| **Paste thread** | A message-only window used to own clipboard writes; all `SendInput` sequencing and inter-key delays. | All sleeping in the paste engine happens here, never on the UI or hook thread. One paste at a time (a queue of depth 1; a new request while busy is dropped with a trace). |
| **Worker pool** (1–2 threads) | History load/save, DPAPI, blob file I/O, thumbnail decode (WIC), search index build for new items. | Communicate results to the UI thread by `PostMessageW` with an owned payload. |

### 5.1 State ownership

- The **history store** is owned by the UI thread. Workers receive immutable snapshots
  (`Arc<[ItemSnapshot]>`) for saving, and send back new items as owned values.
- Hotkey bindings shared with the hook thread live in an `ArcSwap`-style atomic pointer
  or a `RwLock` that the hook thread only ever `try_read`s. The hook MUST NOT block.
- No raw `bool` re-entrancy flags. Any "operation in progress" state is an RAII guard
  whose `Drop` clears it (lesson 18.3).

### 5.2 Why the hook gets its own thread

A `WH_KEYBOARD_LL` callback runs on the thread that installed the hook, and Windows
waits up to `LowLevelHooksTimeout` (default 300 ms) for it **for every keystroke on the
machine**. If that thread is busy, all typing system-wide stalls, and after the timeout
Windows silently stops calling the hook. Putting the hook on a thread that does nothing
else makes this structurally impossible.

---

## 6. Clipboard capture

### 6.1 Change detection

- Register with `AddClipboardFormatListener` on the hidden main window. **Check the
  return value.** On failure, retry once after 150 ms; if it still fails, show a
  one-time warning that history will not record (lesson 18.13).
- On `WM_CLIPBOARDUPDATE`, read `GetClipboardSequenceNumber()`. If it equals a sequence
  number clip4 itself produced (section 6.5), ignore it.
- Do **not** use the legacy `SetClipboardViewer` chain.

### 6.2 The capture rule: lock only for the byte copy

This is the single most important capture rule (lesson 18.1).

1. `OpenClipboard(hwnd)` with a short retry ladder: up to 8 attempts, 5 ms apart, **no
   sleep after the final attempt**.
2. Choose the primary format and copy the raw bytes of up to 12 formats into owned
   `Vec<u8>`s. Nothing else happens inside the lock: no hashing, no parsing, no
   indexing, no comparison, no disk access, no painting, no thumbnails.
3. `CloseClipboard()` — implemented as the `Drop` of a `ClipboardGuard`, so it cannot be
   skipped on any return path.
4. Post the snapshot to a worker, which builds the item, computes its preview, indexes
   it, de-duplicates it, and sends it back to the UI thread.

Only after the snapshot is safely copied is the sequence number marked as consumed
(lesson 18.4).

### 6.3 Retry when the clipboard is busy

Some applications hold the clipboard open briefly while writing; RDP redirection can be
slow. If the open fails:

- Schedule a reconcile retry with backoff (50, 100, 150, 200 ms).
- Retries are **per sequence number** and bounded: after 10 failed attempts on the same
  sequence, drop it with a trace line. A new sequence resets the counter.
- Retrying MUST NOT sleep on the UI thread; use a timer.

### 6.4 Formats

**Primary-format priority** (first available wins; the primary drives the row icon):

`CF_HDROP`, `CF_UNICODETEXT`, `CF_TEXT`, `CF_DIBV5`, `CF_DIB`, `CF_BITMAP`,
`CF_ENHMETAFILE`, `CF_METAFILEPICT`

`CF_DIBV5`/`CF_DIB` come before `CF_BITMAP` deliberately: Windows synthesises one from
another, and two `WM_CLIPBOARDUPDATE` events for the same image would otherwise produce
different bytes and a duplicate entry.

**Handle-based formats** (`CF_BITMAP`, `CF_PALETTE`, `CF_METAFILEPICT`,
`CF_ENHMETAFILE`, `CF_DSPBITMAP`, `CF_DSPENHMETAFILE`, `CF_DSPMETAFILEPICT`) are not
`HGLOBAL`s and MUST NOT be read with `GlobalLock`. `CF_BITMAP` is converted to DIB bytes
(`GetDIBits`) and stored as `CF_DIB`; the others are skipped.

**Capture limits:**

| Limit | Value |
|---|---|
| Formats per item | 12 |
| Bytes per format | < 5 MB (primary may be larger, up to 100 MB for images) |
| Total bytes per item | 10 MB (excluding the primary image) |
| DIB images | reject if computed size > 256 MB or dimensions are inconsistent |

**Formats that MUST be captured when present:** `CF_UNICODETEXT`, `CF_TEXT`,
`CF_HDROP`, `CF_DIB`/`CF_DIBV5`, `"HTML Format"`, `"Rich Text Format"`, `"PNG"`,
`"Preferred DropEffect"` (so cut-vs-copy of files survives).

### 6.5 Self-echo suppression

When clip4 writes the clipboard (to paste), the resulting `WM_CLIPBOARDUPDATE` must not
create a history entry.

- After each write, record the new `GetClipboardSequenceNumber()` in a small ring of
  "own sequences" (last 8). Ignore updates whose sequence is in the ring.
- Some targets (notably Office) read the clipboard and **write it back**, producing a
  new sequence. Also keep the plain text of the last paste with a timestamp, and ignore
  an identical-text capture **within 3 seconds** only. After 3 s it MUST expire
  (lesson 18.5).
- Never use an `is_pasting` boolean checked at message-dispatch time — see lesson 18.6.

### 6.6 Exclusions (privacy)

clip4 MUST honour the formats applications set to opt out of clipboard managers
(section 20.1). If any are present, discard the snapshot without recording it, without
playing the capture sound, and without logging its content.

### 6.7 De-duplication

A new item is a duplicate of the **current top item** (not the whole history) if:

- all format byte sets are identical, or
- their normalised text is identical (trim, CRLF→LF), or
- both are images with identical DIB bytes, or near-identical arriving within 750 ms.

Duplicates are dropped. A duplicate of an older (non-top) item is **moved to the top**
with its timestamp refreshed, preserving its pinned state.

---

## 7. Data model

```text
ClipboardItem
  id            : u64                // stable, monotonic; never reused
  timestamp     : SystemTime (UTC)
  pinned        : bool
  primary       : FormatKey          // see below
  formats       : Vec<(FormatKey, Payload)>
  preview       : String             // up to 300 chars, see 7.2
  kind          : Text | Image | Files | Other
  search_index  : TrigramBloom(1024 bytes) + CharBloom(32 bytes)
  thumbnail     : lazily built, cached, evictable

FormatKey = Standard(u32)            // CF_* constants < 0xC000
          | Registered(String)       // the format NAME, never its numeric id
Payload   = Inline(Vec<u8>) | OnDisk { sha1: [u8; 20], len: u64 }
```

### 7.1 Format identity

Registered clipboard formats (ids ≥ `0xC000`: HTML, RTF, PNG, …) are assigned **per
Windows session** by `RegisterClipboardFormat`. The numeric id for "HTML Format" can
differ after a reboot. clip4 MUST store and persist registered formats by **name**
(`GetClipboardFormatNameW`) and re-resolve the id with `RegisterClipboardFormatW` when
pasting (lesson 18.15).

### 7.2 Preview text

The preview is what a row shows. It MUST show content, never a placeholder, whenever any
textual content exists (lesson 18.14):

1. `CF_UNICODETEXT` — first 300 UTF-16 code units, stopping at the first NUL.
2. else `CF_TEXT` — first 300 bytes (ANSI code page → UTF-16).
3. else `CF_HDROP` — file paths joined with `", "`, first 300 chars.
4. else `"HTML Format"` — tags stripped, entities decoded, first 300 chars.
5. else `"Rich Text Format"` — plain text extracted, first 300 chars.
6. else a label: `"Image 1920×1080"` for images, otherwise the format name.

Rules:

- Compute the preview **after all formats are known**, not from the primary alone.
- Never refuse to preview a large payload; reading the first 300 chars costs the same
  regardless of total size.
- Never append a literal `"..."`. The renderer ellipsises at the real column width.
- Replace CR, LF and TAB with spaces for the row; keep them for the expanded card.

### 7.3 History limits

- `max_items`: user setting, **10–2000**, default **300**.
- Pinned items are never evicted and do not count toward the limit.
- When over the limit, evict the oldest **unpinned** item.

---

## 8. Persistence (and clip2 import)

### 8.1 Locations

| What | Where |
|---|---|
| History | `%APPDATA%\clip4\history.dat` |
| Previous generation | `%APPDATA%\clip4\history.dat.bak` |
| Large payloads | `%APPDATA%\clip4\blobs\<sha1-hex>` |
| Settings | `HKCU\Software\clip4` |
| Snippets | `HKCU\Software\clip4\Snippets` |
| Diagnostics log | `%LOCALAPPDATA%\clip4\clip4.log` (rotating, see 19.3) |

### 8.2 history.dat format (CLP4)

```text
File       := "CLP4" u32:version(=1) u32:blobLen  DPAPI(Inner)
Inner      := u32:itemCount  Item*
Item       := u64:id  i64:unixMillis  u8:flags(bit0=pinned)  u32:formatCount  Format*
Format     := FormatKey  Payload
FormatKey  := u8:tag(0=standard,1=registered)
              tag0: u32:cfId
              tag1: u16:nameLen  utf16[nameLen]:name
Payload    := u8:tag(0=inline,1=blob)
              tag0: u32:len  bytes[len]
              tag1: u64:len  u8[20]:sha1
```

All integers little-endian. DPAPI is `CryptProtectData` with the description
`"clip4 history"`, current-user scope.

### 8.3 Blob sidecars

- Any payload ≥ **256 KB** is written to `blobs\<sha1-hex>` (content-addressed; identical
  payloads share one file) and stored in the record as a blob reference.
- In memory, blob payloads are **dehydrated** (not resident) until needed for paste,
  preview or duplicate comparison, then hydrated by reading the file.
- After a successful save, delete any blob file not referenced by the saved history.
- Blob reads/writes happen on workers only. Hydration needed for a paste happens on the
  paste thread, **before** the clipboard is opened (lesson 18.1).

### 8.4 Write safety

Every save:

1. Serialise the snapshot on a worker.
2. Write to `history.dat.tmp`, `FlushFileBuffers`, close.
3. Rename the current `history.dat` to `history.dat.bak` (replacing it).
4. `MoveFileExW(tmp, history.dat, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`.

Saves are debounced 1.5 s after the last change, and flushed synchronously on clean exit
(tray Exit, `WM_ENDSESSION`, `WM_QUERYENDSESSION`). A crash loses at most 1.5 s of
history.

### 8.5 Load safety

- Bounds-check every length against the remaining buffer before reading. Corrupt or
  truncated input MUST yield "skip this item" or "stop here, keep what was read" —
  never a panic, never an out-of-bounds read.
- Per-format cap 16 MB, per-item cap 16 MB, item count cap 2,000 + pinned.
- If `history.dat` fails to decrypt or parse, try `history.dat.bak`. If both fail,
  start empty, keep the broken files renamed `*.corrupt-<timestamp>`, and log it.
- A missing blob file degrades that one format, not the item or the load.

### 8.6 Importing clip2 history (one-time, first run)

If `%APPDATA%\clip4\history.dat` does not exist and `%APPDATA%\clip2\history.dat` does,
offer to import it. clip2's format:

```text
Outer (current):  "CLP3" u32:blobLen  DPAPI(Inner)        // description "clip2 history"
Outer (legacy):   Inner, plaintext
Inner:            "CLP2" u32:version u32:count  Record*
  version 1:      u32:fmt  u32:size  bytes                 // text only
  version 2:      u8:pinned  u32:formatCount  (u32:fmt  u32:size  payload)*
                  if size == 0xFFFFFFFF: payload is a 20-byte SHA-1;
                  the bytes live in %APPDATA%\clip2\blobs\<sha1-hex>
```

clip2 stored **raw format ids**, so registered formats (≥ `0xC000`) cannot be trusted.
When importing, resolve them by **sniffing the payload**:

| Signature | Format |
|---|---|
| Starts with `Version:` and contains `StartHTML:` | `"HTML Format"` |
| Starts with `{\rtf` | `"Rich Text Format"` |
| Starts with `\x89PNG\r\n\x1a\n` | `"PNG"` |
| Anything else ≥ `0xC000` | drop the format, keep the item |

clip2 also persisted no timestamps; assign imported items descending synthetic
timestamps so their order is preserved. Also import clip2's registry settings and
snippets (section 16.6).

---

## 9. Search

- Search covers the full text of each item, up to **500 KB** indexed per item, plus
  file paths for file items. Images are matched by their label only.
- **Candidate gate:** each item has a 1,024-byte trigram Bloom filter and a 32-byte
  character Bloom filter, built off the UI thread when the item is created or loaded.
  For a query of ≥ 3 chars, an item is a candidate only if every query trigram is in its
  trigram filter; for 1–2 chars, every character must be in its character filter.
  Only candidates are scored.
- **Scoring:** case-insensitive fuzzy subsequence match with bonuses for contiguous runs,
  word-boundary starts and matches near the start of the preview; an exact substring
  scores highest. Ties break by recency.
- The query survives switching between the history and snippets scopes.
- Typing **digits** with the list focused is *number jump*, not search: it selects the
  item with that number (section 10.6).
- MUST meet the 10 ms p95 budget at 2,000 items. If it cannot, search on a worker with a
  generation counter and discard stale results; never block typing.

---

## 10. Overlay UI

### 10.1 Windows and layout

- Two top-level popup windows (`WS_EX_TOPMOST | WS_EX_TOOLWINDOW`, no taskbar button):
  - **Pinned** pane: 320 × 520 logical px, on the left.
  - **Main** pane: 640 × 520 logical px, on the right.
  - 12 px gap. The pair moves together; dragging either moves both.
- All dimensions are **logical pixels** scaled by the monitor's DPI (`GetDpiForWindow`),
  and re-laid-out on `WM_DPICHANGED`.
- Default position: horizontally centred as a pair on the monitor containing the
  foreground window, 50 px from the top of its work area. The last dragged position is
  remembered per monitor configuration and clamped to the current work area on show.
- Windows have rounded corners (DWM `DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND` on
  Windows 11; a rounded region fallback on Windows 10).
- The pinned pane is shown **without activation** so keyboard focus stays on the main
  list.

### 10.2 Visual design ("Soft")

The design is a modern dark UI: layered near-black surfaces instead of borders, a UI sans
for chrome, monospace only for clipboard content, and the accent colour used sparingly.

**Fonts**

| Role | Face | Size |
|---|---|---|
| Content (row text, card body) | User setting, default **Consolas** | `contentSize` (setting, 10–24, default 14). Card body is `contentSize + 3`. |
| Chrome (labels, buttons, key caps, counts, ages) | Segoe UI Variable Text → Segoe UI → Tahoma | `uiSize` (setting, 10–28, default 16). Semibold for titles. |
| Snippet names | Chrome face | `contentSize` (snippet names are content) |

**Colours** are derived from the theme (section 16.2), where `BG` is the background,
`ACCENT` is the theme accent, and `mix(a, b, n)` is a linear blend of `a` toward `b` by
`n/255`:

| Token | Value | Used for |
|---|---|---|
| `surfaceHover` | `mix(BG, white, 12)` | row hover |
| `surfaceField` | `mix(BG, white, 18)` | search field, expanded card, key caps |
| `surfaceRaised` | `mix(BG, white, 30)` | buttons, active scope segment |
| `hairline` | `mix(BG, white, 26)` | window edge |
| `cardEdge` | `mix(BG, white, 38)` | expanded card outline |
| `inkHigh` | `mix(BG, white, 214)` | primary text |
| `inkMid` | `mix(BG, white, 120)` | labels, item number |
| `inkLow` | `mix(BG, white, 86)` | ages, hints, secondary meta |
| `selectedRow` | `mix(BG, ACCENT, 40)` (inactive pane: 20) | selected row fill |
| `badge` | `mix(BG, ACCENT, 46)` fill, `ACCENT` text | expanded card's kind badge |
| primary button | `ACCENT` fill, `mix(ACCENT, black, 200)` text | Paste button |

Text is **neutral**; the accent appears only on the selection, the badge, the pin rail
and the primary button.

**Metrics** (logical px; all scale with the two font sizes)

| Element | Value |
|---|---|
| Row height | `contentSize + 26` |
| Header height | `max(contentSize, uiSize) + 40` |
| Search field | `max(contentSize, uiSize) + 20` tall, 12 px from top, radius 9 |
| Footer height | `uiSize + 22` |
| List side inset | 8 |
| Icon gutter | `contentSize + 12` |
| Age column | `3 × contentSize` |
| Radii | row 9, card 11, field 9, button 7, key cap 5 |
| Expanded card | `2 × uiSize + 68` + `lines × (contentSize + 9)`, max 4 body lines |

### 10.3 Header

- **Main pane:** a filled, rounded search field with a magnifier icon, then a segmented
  scope control: **All `N`** | **Snippets `N`**. The active segment is `surfaceRaised`.
  Clicking a segment switches scope; `Ctrl+→` / `Ctrl+←` do the same.
- **Pinned pane:** a search field only (cue text "Search pinned").
- The search field is a real edit control positioned inside the painted pill. Its text
  and cue banner are drawn by the control; the renderer draws only the pill and icon.
  Its background MUST match `surfaceField`, its text `inkHigh` (lesson 18.24).

### 10.4 Rows

Each row: a type icon (text / image / files) in the gutter, the preview in the content
font, the relative age right-aligned (`now`, `12s`, `5m`, `3h`, `2d`, `1w`).

- Pinned items show a 2 px `ACCENT` rail at the row's left edge.
- Hover fills `surfaceHover`. Selection fills `selectedRow` with **light** text — never
  inverse video.
- Image rows MAY show a lazily-decoded thumbnail in place of the icon; thumbnails decode
  on a worker and only for visible rows of the active pane.
- At least 12 px between the end of the preview and the age column.

### 10.5 The expanded card (optional)

When **Expand selected item** is enabled (default on), the selected item in the main
list opens in place as a raised card:

- **Meta row** (chrome font): the item number `#N`, a kind badge (`Text` / `Image` /
  `Files`), the line count, and the age.
- **Body:** up to 4 lines of the item's real content in the content font at
  `contentSize + 3`, each line ellipsised.
- **Action buttons** (chrome font): `↵ Paste` (primary), `U Clean URL` (only for URLs),
  `P Plain`, `E Edit`, `M Merge`. Each is clickable and also names a working key.

Layout rules that MUST hold (lessons 18.17–18.19):

- Height = reserved chrome + line count × line height. Reserved chrome MUST cover the
  meta row, the body-to-buttons gap, the buttons and both paddings; body and buttons
  MUST NOT overlap at any font size.
- The card is shown **whole or not at all.** If it does not fit, the list scrolls until
  it does; if the window cannot hold it, the item renders as a normal selected row.
- When expansion is disabled, the selected row is still visibly selected.

### 10.6 Number jump

Typing digits with the list focused shows `#<digits>` and selects the item with that
original 1-based number; **Enter** pastes it. **Backspace** edits the number; any
non-digit key clears it. While digits are being typed, rows show their numbers in the
icon gutter.

### 10.7 Footer

Key caps for the essential actions only: `↵ Paste`, `Tab Pinned`, `? Shortcuts` (pinned
pane: `Tab List`, `↵ Paste`). Each key cap is a small `surfaceField` chip.

### 10.8 Shortcut sheet

`?` or `F1` overlays a sheet listing every binding (section 11.3). Any key or click
dismisses it.

### 10.9 Layout engine (shared)

Row heights vary (the card), so positions cannot be computed from an index. There MUST
be exactly one layout function that produces the visible bands:

```text
layout(scroll_offset, viewport_height) -> Vec<Band { top, height, item_index, expanded }>
```

The renderer, mouse hit-testing, keyboard paging and "ensure selection visible" all
consume this one function (lesson 18.17). `ensure_selection_visible()` advances the
scroll offset until the selected band is present **and fully laid out** (including the
card when expansion is on).

### 10.10 Rendering

- Direct2D render target + DirectWrite text, per-monitor-DPI aware, antialiased.
  Rounded rectangles MUST be antialiased (GDI `RoundRect` is not).
- Render on demand only (on input, data change or timer-driven animation); no frame loop
  while idle.
- Handle `D2DERR_RECREATE_TARGET` by recreating device resources.
- Text layouts MAY be cached per item and invalidated on font/size/theme change.

### 10.11 Focus and dismissal

- Showing the overlay remembers the previously focused window (`GetForegroundWindow`).
- The overlay takes the foreground reliably, including over the Start menu. clip2 used
  `AttachThreadInput` + `SPI_SETFOREGROUNDLOCKTIMEOUT` + a phantom Alt press; clip4
  SHOULD use `AttachThreadInput` with the foreground thread first and fall back to the
  phantom Alt only if `SetForegroundWindow` fails. Any system-wide setting changed for
  this MUST be restored by an RAII guard and MUST NOT be broadcast with
  `SPIF_SENDCHANGE` (lesson 18.10).
- The overlay hides when it loses the foreground (after first acquiring it), on Esc, or
  on paste. While it has not yet acquired the foreground, keep trying for up to 3 s
  before giving up.

---

## 11. Keyboard, hotkeys and input

### 11.1 Global hotkeys

| Action | Default |
|---|---|
| Toggle clipboard overlay | **Ctrl+NumPadDot** |
| Toggle snippets overlay | *(unbound)* |
| Copy from focused control | **Ctrl+F10** |
| Paste as keystrokes (no clipboard change) | **Ctrl+F11** |
| Paste via clipboard swap + Ctrl+V | **Ctrl+Shift+F11** |

- All five are rebindable in Settings. Register each with `RegisterHotKey(MOD_NOREPEAT)`
  and **report failures** (a binding already taken by another app) in the Settings UI.
- The overlay toggle is **also** caught by the `WH_KEYBOARD_LL` hook, because
  `RegisterHotKey` does not fire in some fullscreen or elevated contexts. When both fire
  for one press, de-duplicate with a 250 ms window.
- The hook MUST ignore injected input (`LLKHF_INJECTED`, `LLKHF_LOWER_IL_INJECTED`), so
  clip4's own `SendInput` never retriggers it.
- The hook MUST prefilter on the virtual-key code before querying modifier state, so
  uninteresting keys cost one comparison.

### 11.2 Hook health

- Re-install the hook from scratch when it may have been dropped: install the new hook
  first, then unhook the old handle (never guard re-install on "handle is non-null" —
  a dropped hook leaves a stale non-null handle; lesson 18.11).
- Re-arm unconditionally every 60 s as a backstop.
- With the hook on its own idle thread (section 5), drops should never happen; the
  keepalive is defence in depth.

### 11.3 Overlay bindings (list focused)

| Key | Action |
|---|---|
| ↑ ↓ / PgUp PgDn / Home End | Move selection |
| Shift + ↑ ↓ | Extend multi-selection |
| Ctrl + click | Toggle item in multi-selection |
| Enter / double-click | Paste selection (or multi-selection) |
| Ctrl + Enter | Paste as plain text |
| Tab | Switch focus between main and pinned pane |
| Ctrl + → / Ctrl + ← | Snippets scope / clipboard scope |
| Ctrl + F | Focus search |
| digits | Number jump (10.6) |
| Delete | Delete selected item(s) |
| U | Paste with tracking parameters stripped |
| M | 2+ selected: merge into a new top item. Otherwise: paste as Markdown link |
| P | Paste as plain text. 2+ selected: also add the plain merge as a new top item |
| H | Paste HTML as plain text |
| E | Edit, then paste (history unchanged) |
| X | Edit, then save as a new item |
| Z | Excel fill (2+ selected), section 12.5 |
| ? / F1 | Shortcut sheet |
| Esc | Close sheet, else close overlay |

Typing printable characters (other than the single-letter commands above and digits)
goes to the search field. Single-letter commands only apply when the list, not the
search field, has focus.

### 11.4 Context menu (right-click on an item)

Pin / Unpin, Copy as plain text, Open URL (http/https only), Save image to file…,
Keystroke paste, Transform text ▸ (UPPER, lower, Title Case, remove line breaks, trim,
plain text), Paste as ▸ (the smart modes), Edit & Paste…, Edit & Save as new…, Merge
selection into new item, Paste & merge as plain text, Paste into Excel, Delete,
Clear list (unpinned only, with confirmation).

---

## 12. Paste engine

All paste sequencing runs on the **paste thread** (section 5). The UI thread only
enqueues a `PasteRequest` and hides the overlay.

### 12.1 The standard paste

1. **Hydrate** any on-disk payloads for the item (file reads) — *before* opening the
   clipboard.
2. **Prepare** every `HGLOBAL` (allocate, lock, copy, unlock) — *before* opening the
   clipboard. If preparation fails, abort with the user's clipboard untouched.
3. **Open** the clipboard with the paste thread's message-only window as owner (never
   `NULL` — `EmptyClipboard` after `OpenClipboard(NULL)` makes `SetClipboardData` fail).
   Retry up to 24 × 8 ms.
4. `EmptyClipboard`, then `SetClipboardData` for each format. On a failed
   `SetClipboardData`, free that `HGLOBAL` (ownership did not transfer). Close.
5. Record the new sequence number as an own-echo (6.5).
6. Restore focus to the previously focused window: `AttachThreadInput` to its thread,
   `SetForegroundWindow`, detach.
7. Wait until the foreground is actually the target (poll up to 300 ms, 10 ms steps).
   If it never becomes the target, **abort** — do not inject Ctrl+V into clip4's own
   window (lesson 18.9).
8. Release any modifier keys still physically held from the hotkey (send key-up for the
   specific L/R virtual keys that `GetAsyncKeyState` reports down).
9. Send Ctrl+V as **one** `SendInput` batch: LCtrl down, V down, V up, LCtrl up. Check the
   return count.

Starting delays (tunable constants, measured in clip2): 40 ms after focus restore, 20 ms
after modifier release, 420 ms settle after Ctrl+V before the paste thread accepts the
next request.

### 12.2 Plain-text paste

As 12.1 but publish only `CF_UNICODETEXT` (from the best text source, section 7.2).

### 12.3 Multi-paste (Enter with 2+ selected)

- **All text/RTF/HTML:** merge into one payload — Unicode joined with `\r\n`, RTF
  paragraphs, HTML `<br>` — and paste once. Also insert the merged payload as a new top
  history item.
- **Mixed (includes images/files):** paste each item sequentially using a clipboard swap
  per item, inserting a `\r\n` text paste after non-text items. Back up the user's
  clipboard first and restore it afterwards (12.6).

### 12.4 Keystroke paste (Ctrl+F11)

Types the item's Unicode text into the focused control with `SendInput`
`KEYEVENTF_UNICODE`, without touching the clipboard.

- `\r\n` → one `VK_RETURN`; lone `\n`/`\r` → `VK_RETURN`; `\t` → `VK_TAB`.
- Surrogate pairs sent as two `KEYEVENTF_UNICODE` events.
- Batches of ~24 inputs; if `SendInput` returns fewer than requested, retry the
  remainder up to 8 times with a short pause.
- For the newest item if the overlay is closed, the selected item if it is open.
- Release all modifiers before typing.

### 12.5 Excel fill (Z, 2+ selected)

For each selected item, top to bottom: `F2` (enter cell edit) → paste the item's
**Unicode text only** → `Enter` (commit and move down).

- Unicode only, because Excel's in-cell editor refuses sheet/rich formats pasted while
  editing.
- Before the first keystroke, wait for the physical `Z`, `Ctrl` and `Shift` keys to be
  released (up to 1.2 s each), else Excel receives Ctrl+Z (undo). This wait happens on
  the paste thread.
- Starting delays: 30 ms after set, 180 ms after F2, 220 ms after Ctrl+V, 150 ms after
  Enter.
- If the foreground leaves Excel mid-run, stop and still restore the clipboard.

### 12.6 Clipboard swap and restore

Any mode that temporarily replaces the clipboard (multi-paste mixed, Excel, Ctrl+Shift+F11)
MUST:

1. Back up every format of the user's current clipboard first (bytes; `CF_BITMAP` as DIB).
2. Perform the paste(s).
3. Restore the backup, retrying the open up to 30 × 8 ms, then once more after 60 ms.
4. If restore still fails, log `CLIPBOARD RESTORE FAILED` and show a non-modal notice —
   the user's previous clipboard content is otherwise silently lost (lesson 18.8).
5. Never `EmptyClipboard` when the backup is empty.

---

## 13. Transforms and smart paste

| Mode | Behaviour | Applies to |
|---|---|---|
| **Clean URL (U)** | Remove query parameters starting `utm_`, and these exact names (case-insensitive): `fbclid gclid gclsrc dclid msclkid mc_eid mc_cid igshid igsh si ref_src ref_url _ga _gl yclid wbraid gbraid vero_id oly_anon_id oly_enc_id s_kwcid spm scid mkt_tok twclid ttclid`. Preserve fragment and parameter order. | Items whose text is a single http(s) URL |
| **Markdown link (M)** | `[title](url)` using the HTML title or the URL host as text; for non-URLs, wrap as inline code. | Text items |
| **Plain (P)** | Best text source as `CF_UNICODETEXT` only. | Any item with text |
| **HTML → text (H)** | Strip tags, decode entities, convert `<br>`/block ends to newlines, collapse whitespace. | Items with `HTML Format` |
| **Edit & paste (E)** | Modal editor (multi-line, Ctrl+Enter to confirm), paste the result, history unchanged. | Text items or multi-selection (joined) |
| **Edit & save (X)** | Same editor; confirm adds the result as a new top item and puts it on the clipboard. | Same |
| **Transform ▸** | UPPER, lower, Title Case, remove line breaks, trim, plain — replaces the item's text in place and puts it on the clipboard. | Text items |

Modes that do not apply to the selected item do nothing (no error dialog).

---

## 14. Snippets

- A library of named, reusable text blocks, shown in the main pane's **Snippets** scope.
- Each snippet: `name`, `content` (plain or RTF), optional `content_plain`.
- **Placeholders**, expanded at paste time: `{{date}}` `{{time}}` `{{datetime}}`
  `{{year}}` `{{month}}` `{{day}}` `{{hour}}` `{{minute}}` `{{second}}`
  `{{clipboard}}` (current clipboard text, read with a short open/copy/close).
  Format date/time with the user's locale.
- **Paste** publishes RTF (when present) plus `CF_UNICODETEXT` (from `content_plain`, or
  text extracted from the RTF). An RTF snippet MUST NOT expose raw RTF source as
  Unicode text.
- In the Snippets scope: **Enter**/click pastes, **A** adds, **E** edits, typing filters,
  `*set` + Enter opens the manager. `*set` is reserved and cannot be a snippet name.
- A **Manage snippets** dialog: list, add, edit (rich editor), delete, reorder.
- Optional dedicated hotkey opens the overlay directly in the Snippets scope.

Storage: `HKCU\Software\clip4\Snippets`, value `Count` (DWORD) and `N0`, `N1`, … (REG_SZ)
each holding `name + U+0001 + content [+ U+0002 + content_plain]`. Values ≥ 32,767
chars are not stored; the editor MUST warn before that limit.

---

## 15. Copy from focused control (Ctrl+F10)

For applications whose text never reaches the clipboard:

1. Using UI Automation on the focused element, try in order: `TextPattern` (selection,
   else document range), `ValuePattern`, `LegacyIAccessiblePattern` value.
2. If UIA yields nothing, fall back to a synthetic copy: send Ctrl+C, wait for a
   clipboard sequence change (≤ 400 ms); if none, send Ctrl+A then Ctrl+C.
3. Add the captured text to history; on the UIA path also put it on the clipboard.

UIA calls run on a dedicated STA thread with a timeout; a hung provider MUST NOT freeze
the UI.

---

## 16. Settings, themes and configuration

### 16.1 Settings dialog

A single dialog ("clip4 Settings"), DPI-aware, keyboard-navigable:

| Setting | Control | Range / default | Applies |
|---|---|---|---|
| Five hotkeys | Click field, press combo | defaults in 11.1 | On Save |
| Overlay theme | Dropdown | 15 presets, default Neon Green | Live |
| Content font | Dropdown of installed families | default Consolas | Live |
| Content font size | Dropdown | 10–24, default 14 | Live |
| UI text size | Dropdown | 10–28, default 16 | Live |
| Per-element colours | Six swatches: Background, Text, Accent, Selected text, Border, Dim | click = pick, double-click = reset | Live |
| History size | Number | 10–2000, default 300 | On Save |
| Expand selected item | Checkbox (also in tray menu) | default on | Live |
| Start with Windows | Checkbox (also in tray menu) | default off | Immediate |

**Defaults** resets everything above. **Save** persists; closing without Save keeps live
changes already applied (theme/font) but reverts hotkeys and history size.

### 16.2 Themes

A theme preset supplies `text`, `accent`, `border` and `dim`. The background defaults to
pure black (AMOLED); selected-text defaults to `inkHigh`.

Presets (keep this order; the selected index is persisted, so **append new presets
only** — lesson 18.21):

| # | Name | text | accent | border | dim |
|---|---|---|---|---|---|
| 0 | Neon Green (AS/400) | 00FF66 | 00FF66 | 00C850 | 008028 |
| 1 | Neon Red | FF2040 | FF2040 | DC1432 | 780818 |
| 2 | Neon Blue | 2878FF | 2878FF | 1E64DC | 10388C |
| 3 | Neon Cyan | 00F0FF | 00F0FF | 00C8E6 | 006478 |
| 4 | Neon Purple | C83CFF | C83CFF | AA28DC | 5A1482 |
| 5 | Neon Yellow | FFE600 | FFE600 | DCC800 | 786E00 |
| 6 | Neon Orange | FF8000 | FF8000 | DC6E00 | 823C00 |
| 7 | Neon White | F0F0F0 | F0F0F0 | C8C8C8 | 6E6E6E |
| 8 | Slate | B6D5F4 | 83BDF8 | 466C93 | 233A51 |
| 9 | Teal | A7DDDC | 57CCCC | 277676 | 104040 |
| 10 | Moss | BBDBBB | 8CC98E | 4D744E | 273F28 |
| 11 | Ember | E9CBAB | E3AB6A | 856136 | 493319 |
| 12 | Clay | F3C4BF | F39D95 | 8F5753 | 4F2D2B |
| 13 | Violet | D3CBF2 | BCAAF4 | 6C6090 | 3A334F |
| 14 | Graphite | D1D1D1 | B7B7B7 | 696969 | 383838 |

Selecting a preset MUST clear all per-element colour overrides first, or the preset has
no visible effect (lesson 18.20). Re-selecting the current preset in the dropdown MUST
also apply it (handle the "no change event" case).

When per-element overrides are active, the Settings dialog SHOULD show
"Custom colours active — Reset".

### 16.3 Live apply

Any theme, font or colour change invalidates cached brushes/text formats/layouts and
repaints both panes immediately. Font-size changes recompute every metric in 10.2 and
reposition the search edit controls.

### 16.4 Registry layout (`HKCU\Software\clip4`)

| Value | Type | Meaning |
|---|---|---|
| `ThemeId` | DWORD | Preset index |
| `ThemeFontFace` | SZ | Content font family |
| `ThemeFontSize` | DWORD | Content size |
| `UiFontSize` | DWORD | Chrome size |
| `ColorBG` `ColorTXT` `ColorSELBG` `ColorSELFG` `ColorBORDER` `ColorDIM` | DWORD | `0x00BBGGRR`, or `0xFF000000` = follow preset |
| `MaxItems` | DWORD | History size |
| `ExpandSelected` | DWORD | 0/1 |
| `OverlayPosX` `OverlayPosY` | DWORD | Last main-pane position |
| `<Action>Modifiers` `<Action>VkCode` | DWORD | For `Hotkey`, `Snippets`, `CopyFocused`, `PasteFocused`, `PasteClipboard` |

All reads MUST validate and clamp; a malformed value falls back to its default.

### 16.5 Startup

"Start with Windows" writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\clip4`
with the quoted exe path.

### 16.6 Importing clip2 settings

On first run, if `HKCU\Software\clip2` exists, import the values above (same names) and
the `Snippets` subkey, then stop reading clip2's keys.

---

## 17. Tray, single instance, startup, sound

- **Single instance:** a named mutex `Local\clip4-single-instance`. A second launch finds
  the running instance's hidden window by class name and posts it "show overlay", then
  exits.
- **Tray icon:** left-click toggles the overlay; right-click opens the menu: Show
  clipboard, Copy from focused control, Snippets ▸ (paste one / Manage…), Start with
  Windows ✓, Expand selected item ✓, Settings, Restart, Exit.
- Re-add the tray icon on `TaskbarCreated` (Explorer restart).
- **Restart** flushes history synchronously, releases the mutex, then launches a new
  instance.
- **Capture sound:** a short click on each recorded capture. Embed a WAV resource and
  play it with `PlaySoundW(SND_MEMORY | SND_ASYNC | SND_NODEFAULT)`. No runtime decoding.
  The sound is fire-and-forget and MUST never delay capture (lesson 18.7). A setting to
  mute it is OPTIONAL.

---

## 18. Lessons from clip2 — defects that MUST NOT recur

Each of these shipped in clip2 and was diagnosed from real user reports. The "Rule" is
the requirement for clip4.

### 18.1 Holding the clipboard lock across slow work
**Symptom:** "Ctrl+C does nothing" in *other* applications, intermittently.
**Cause:** the clipboard is one global exclusive lock. clip2 kept it open while building
the item, indexing 500 KB of text, reading blob files for duplicate checks, and
repainting (including shell thumbnail extraction that could hit a network share).
Every other process's `OpenClipboard` failed meanwhile.
**Rule:** the lock is held only for raw byte copies. Hydrate and prepare before opening;
close before processing. Enforce structurally with a scoped `ClipboardGuard`.

### 18.2 Blocking the thread that services the keyboard hook
**Symptom:** system-wide typing lag during pastes; Ctrl+V and hotkeys "globally broken
until restart".
**Cause:** the LL hook lived on the UI thread, which slept for up to seconds inside paste
sequences. Windows waited up to 300 ms per keystroke, then dropped the hook.
**Rule:** hook on its own idle thread; all paste sleeping on the paste thread.

### 18.3 Latched re-entrancy flags
**Symptom:** history silently stops recording until restart.
**Cause:** a raw `isProcessingClipboard` bool set at the top of capture; an exception from
an allocation escaped before it was cleared.
**Rule:** RAII guards only. In Rust, any guard must clear in `Drop`, and panics in workers
are caught at the thread boundary (19.1).

### 18.4 Consuming the sequence number before capture succeeded
**Symptom:** occasional copies never appear.
**Cause:** the "last seen sequence" was updated on notification, before the snapshot
succeeded; a failed capture could never be retried.
**Rule:** mark a sequence consumed only after its bytes are safely copied.

### 18.5 Echo suppression that never expired
**Symptom:** copying text you previously pasted from clip4 is silently ignored.
**Cause:** "last pasted text" was cleared only when an echo matched; with no echo it lived
forever.
**Rule:** echo suppression is time-boxed to 3 s and sequence-based first.

### 18.6 A "pasting" flag checked at dispatch time
**Cause:** `WM_CLIPBOARDUPDATE` is posted; the flag was checked when the message was
dispatched, which (because the thread was sleeping) was always after the flag cleared.
**Rule:** suppress by sequence number (6.5), never by a flag sampled at dispatch.

### 18.7 Slow work before the capture
**Cause:** the click sound was decoded from MP3 via Media Foundation on the capture path,
and the decode was cached only on success — a broken install re-decoded on every copy,
before the snapshot.
**Rule:** capture first, sound after, and the sound is a pre-embedded WAV.

### 18.8 Destroying the user's clipboard on failure
**Cause:** `EmptyClipboard` ran before payload allocation; restore after a swap used a
single non-retrying open and its failure was ignored.
**Rule:** prepare first, empty second; restore with retries and a visible failure.

### 18.9 Pasting into the wrong window
**Symptom:** "nothing pasted" — the Ctrl+V went into clip4's own search box.
**Cause:** focus was restored with fixed sleeps and never verified.
**Rule:** verify the foreground is the target before injecting; abort otherwise.

### 18.10 System-wide settings left changed
**Cause:** `SPI_SETFOREGROUNDLOCKTIMEOUT` set to 0 and restored only on the happy path,
with `SPIF_SENDCHANGE` broadcasting to every window ~20×/s during focus retries.
**Rule:** RAII restore; no broadcast flag.

### 18.11 Hook re-install blocked by a stale handle
**Cause:** `if (hook == null) install()` — a dropped hook leaves a non-null handle.
**Rule:** install new, then unhook old; periodic re-arm.

### 18.12 Sleeping after the final retry
**Cause:** the open-retry loop slept after the last failed attempt, always burning the
full budget.
**Rule:** no sleep after the final attempt.

### 18.13 Unchecked listener registration
**Cause:** `AddClipboardFormatListener`'s return was ignored; on failure clip2 silently
recorded nothing.
**Rule:** check, retry, warn.

### 18.14 Placeholder previews
**Symptom:** rows showing `[Unknown Format]` or truncated `...`.
**Cause:** the preview came from the primary format only, refused payloads over 10,000
chars, and appended `"..."` after 50 chars.
**Rule:** section 7.2.

### 18.15 Persisting registered-format ids
**Cause:** clip2 wrote raw format ids; ids ≥ `0xC000` change between Windows sessions, so
saved HTML/RTF/PNG could be pasted under the wrong format after a reboot.
**Rule:** persist registered formats by name (7.1).

### 18.16 Ignoring password-manager opt-outs
**Cause:** clip2 recorded passwords copied from password managers.
**Rule:** section 20.1.

### 18.17 Two derivations of row geometry
**Symptom:** clicks selecting the row below the one clicked; the selected item vanishing
at the bottom of the list.
**Cause:** paint and hit-testing each computed positions; scrolling assumed uniform rows.
**Rule:** one layout function (10.9) for everything.

### 18.18 Drawing a clipped card
**Cause:** at the bottom of the list the card was squashed to the remaining space; its
buttons (bottom-anchored) and body (top-anchored) overlapped and the body spilled onto
the footer.
**Rule:** whole card or plain row.

### 18.19 Under-reserved card height
**Cause:** the card reserved less height than its meta row + buttons + paddings, so body
text touched the buttons on every card.
**Rule:** derive reserved height from the same constants used to draw; add a check
(22.2).

### 18.20 Overrides silently shadowing presets
**Cause:** per-element colour overrides applied last; once any was set, the theme
dropdown did nothing visible. Re-selecting the current preset fired no change event.
**Rule:** section 16.2.

### 18.21 Inserting into a persisted enum
**Rule:** the theme index is persisted; new presets are appended only.

### 18.22 `OpenClipboard(NULL)` + `EmptyClipboard`
**Rule:** always open with a real owner window. With `NULL`, `EmptyClipboard` succeeds but
`SetClipboardData` then fails.

### 18.23 Proportional fonts in a column layout
**Cause:** fixed pixel columns assumed monospace; the user's font was proportional.
**Rule:** content is measured with DirectWrite; never assume a cell width.

### 18.24 Edit control painting over the field
**Cause:** the search edit control painted the window background inside a raised pill,
and its text in the accent colour.
**Rule:** match the control's background and text to the field (10.3).

### 18.25 Silent failure everywhere
**Cause:** ~50 empty `catch (...)` blocks and no logging; no failure could be diagnosed.
**Rule:** section 19.3.

---

## 19. Robustness, error handling and diagnostics

### 19.1 Panics and crashes

- No `unwrap`/`expect`/`panic!` on runtime data (enforced by lints, section 4).
- Every thread entry point wraps its body in `std::panic::catch_unwind`. A panicking
  worker logs, drops its task, and is restarted. A panic on the hook thread re-installs
  the hook on a fresh thread.
- Install a panic hook that writes the message, location and backtrace to the log.
- Install an unhandled-exception filter (`SetUnhandledExceptionFilter`) that writes a
  minidump (`MiniDumpWriteDump`) to `%LOCALAPPDATA%\clip4\crash\` and restarts clip4
  once (not in a loop: no restart if the previous crash was < 60 s ago).
- A failed allocation for an oversized clipboard payload drops that payload, not the
  process: check sizes against limits *before* allocating.

### 19.2 FFI safety

- Every Win32 call is wrapped in a small safe function that checks the documented
  failure value and returns `Result`.
- RAII wrappers for: open clipboard, `HGLOBAL` (with explicit `into_raw` when ownership
  transfers to `SetClipboardData`), GDI/D2D/DWrite objects, hooks, registry keys, file
  handles, COM initialisation, `AttachThreadInput`, system-setting changes.
- `GlobalSize` is an upper bound, not the payload length; text formats must stop at the
  first NUL.
- All clipboard payloads are untrusted input: parse HTML Format offsets, DIB headers and
  `DROPFILES` with full bounds checks.

### 19.3 Logging

- Structured log at `%LOCALAPPDATA%\clip4\clip4.log`, 1 MB × 3 rotating files.
- Levels: error, warn, info, debug (debug off by default; toggled by a registry value).
- Log every failed `OpenClipboard` with the current owner (`GetOpenClipboardWindow` →
  process name), every abandoned capture retry, every aborted paste and why, every
  restore failure, every hook re-install, and the time the clipboard lock was held when
  above 20 ms.
- **Never log clipboard content.** Log lengths and format names only.

### 19.4 Watchdogs

- If any "operation in progress" state is older than its budget (capture 30 s, paste
  60 s), log and clear it.

---

## 20. Security and privacy

### 20.1 Clipboard exclusion formats (MUST)

Do not record, play a sound for, or log the content of a clipboard snapshot when any of
these registered formats is present:

- `ExcludeClipboardContentFromMonitorProcessing` — any value.
- `CanIncludeInClipboardHistory` — DWORD value `0`.
- `Clipboard Viewer Ignore` — any value (legacy convention).

Also honour `CanUploadToCloudClipboard` = 0 by never syncing (clip4 never syncs anyway).

### 20.2 At rest

- History encrypted with DPAPI (current-user scope). Blob sidecar files SHOULD also be
  DPAPI-encrypted individually; if not, document that large payloads are stored in clear.
- The settings dialog offers **Clear history** (unpinned and, with confirmation, pinned),
  which also deletes blob files.

### 20.3 Process

- Runs `asInvoker`. Never requests elevation.
- No network access of any kind.
- Synthetic input into an elevated window is blocked by UIPI; detect a failed paste into
  a higher-integrity target and tell the user, rather than failing silently.

---

## 21. Compatibility matrix

clip4 MUST be verified against:

| Source / target | Notes |
|---|---|
| Notepad, Windows Terminal, VS Code | Plain text, Unicode, very long lines |
| Word, Outlook | RTF + HTML; Office writes the clipboard back after paste (echo, 6.5) |
| Excel | Range copies (many formats, delayed rendering); Z mode; in-cell editor rejects rich paste |
| Chrome, Edge, Firefox | `HTML Format` with fragment offsets; images |
| File Explorer | `CF_HDROP`, `Preferred DropEffect` (cut vs copy) |
| Paint, Snipping Tool, PrintScreen | `CF_DIB`/`CF_DIBV5`, `PNG`, `CF_BITMAP`-only sources |
| KeePassXC, Bitwarden, 1Password | Exclusion formats (20.1) |
| Remote Desktop | Slow, contended clipboard; retries (6.3) |
| Elevated apps (admin terminal) | UIPI blocks injection (20.3) |
| Fullscreen games / video | `RegisterHotKey` may not fire; LL hook fallback |
| Win+V clipboard history | Coexists; no interference |
| Multi-monitor, mixed DPI | Overlay on the right monitor, correct scale |
| Explorer restart | Tray icon returns |
| Sleep/resume, fast user switching | Listener and hook survive; re-register if needed |

---

## 22. Testing and acceptance criteria

### 22.1 Unit tests (`cargo test`)

- Persistence round-trip: write → read → identical items, including registered formats
  by name, blob references and pinned flags.
- Corrupt input fuzzing: truncated files, bad lengths, random bytes — load must never
  panic. Add a `cargo fuzz` target for the history parser and the clip2 importer.
- clip2 import: a fixture file in each clip2 variant (CLP3, legacy CLP2 v1, v2 with
  blob markers) imports correctly, with format sniffing.
- Preview rules (7.2) for each source format, including > 10,000-char text and HTML-only
  items.
- URL cleaning: every parameter in section 13, fragments and ordering preserved.
- Search: ranking order on a fixed corpus; Bloom gate never excludes a true match.
- Layout engine: for random item sets, font sizes and selections — bands never overlap,
  never exceed the viewport, and the selected band is present and whole after
  `ensure_selection_visible()`.
- Card metrics: at every content/UI size combination, the body-to-buttons gap is ≥ 8 px.

### 22.2 Integration tests (Windows, real clipboard)

- **Contention:** a helper process holds the clipboard for 1.5 s; a copy made during that
  time is recorded after it releases.
- **Lock time:** capturing a 10 MB image holds the clipboard < 30 ms.
- **Other apps unaffected:** while clip4 captures large images in a loop, a second
  process's copy/paste loop never fails `OpenClipboard` for more than its retry window.
- **Hook liveness:** during a 20-item Excel fill, a test process's keystrokes reach their
  target with < 5 ms added latency, and the overlay hotkey works afterwards.
- **Exclusion:** content copied with `ExcludeClipboardContentFromMonitorProcessing` never
  appears in history or the log.
- **Clipboard preservation:** after every swap-based mode, the user's prior clipboard is
  restored byte-identically.
- **Echo:** pasting into Word does not create a duplicate; re-copying the same text 5 s
  later does create an entry.

### 22.3 Manual acceptance

- Each smart mode and context-menu action against the matrix in section 21.
- Theme switching, including re-selecting the current preset with overrides active.
- Every font-size combination at 100%, 150% and 200% DPI: no clipping, no overlap.
- Arrow to the last item with expansion on: the card is whole and above the footer.

---

## 23. Deliverables and milestones

Deliver as a Cargo workspace (single binary crate is fine), with a `README.md` covering
build, run, settings and data locations, and `cargo test` passing.

Suggested order (each milestone is usable on its own):

1. **Capture + persistence.** Tray icon, single instance, listener, snapshot-and-close
   capture, exclusions, de-dup, history store, CLP4 save/load, logging, crash handling.
2. **Overlay + standard paste.** Two-pane overlay with the Soft design, layout engine,
   search, number jump, keyboard navigation, the paste thread and standard/plain paste.
3. **Hotkeys and hook.** Global hotkeys, LL hook on its own thread, re-arm, settings for
   bindings.
4. **Multi-paste and smart modes.** Multi-select, merge, U/M/P/H/E/X, transforms,
   context menu, expanded card.
5. **Excel fill, keystroke paste, clipboard swap.** Including backup/restore.
6. **Snippets** and the manager dialog.
7. **Copy from focused control** (UIA).
8. **Settings dialog, themes, fonts, sizes**, live apply.
9. **clip2 import** (history, settings, snippets).
10. **Hardening pass:** the full test plan in section 22, fuzzing, DPI and compatibility
    matrix.

When a requirement here conflicts with clip2's observed behaviour, **this document wins**
— clip2's behaviour is described only where clip4 should match it.
