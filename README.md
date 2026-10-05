# clip4

A system-tray clipboard manager for Windows 10/11, written in Rust. A ground-up rewrite of
clip2 (see `CLIP4_SPEC.md` for the full specification and the clip2 defects it avoids).

clip4 records everything you copy (text, RTF, HTML, images, file lists), and lets you recall,
search, transform and paste any past item from a keyboard-driven overlay that appears on a
global hotkey. It never opens a socket, never requests elevation, and stores history encrypted
with DPAPI.

## Build, test, run

Requirements: Windows 10 1809+/11, the stable Rust toolchain with the MSVC target, and the
Windows SDK (for `rc.exe`, used by the build script to embed the icon and manifest).

```powershell
.\build.ps1                    # release build; puts clip4.exe in the repo root (checked to carry Cargo.toml's version)
cargo build --release          # -> target\release\clip4.exe (single file, no installer)
cargo test                     # unit tests (pure logic, parsers, fuzz-style loops)
cargo test --test integration -- --ignored --test-threads=1
                               # opt-in real-clipboard/keyboard tests; see "Integration tests"
cargo run --release            # start it; look for the tray icon
```

To publish a GitHub release (needs the GitHub CLI, logged in with `gh auth login`, and a pushed commit):

```powershell
.\publish_github.ps1 v1.2.0 -DryRun   # build, package dist\clip4-v1.2.0-windows-x64.zip (+ .sha256), check everything
.\publish_github.ps1 v1.2.0           # same, then asks before publishing release v1.2.0
.\publish_github.ps1                  # no tag: releases the version Cargo.toml already has
```

The tag (`v1.2.0` or `1.2.0`; a suffix such as `1.2.0-rc.1` makes it a pre-release) is the first
parameter. If it differs from `Cargo.toml`, the script bumps `Cargo.toml` + `Cargo.lock` so the
binary reports the version it ships as, and commits and pushes that bump only after you confirm
(a dry run, a cancel or a failure restores both files). Other options: `-NotesFile`/`-Notes`,
`-Draft`, `-Prerelease`, `-NoBump`, `-Update` (replace the assets of an existing release), `-Yes`.

Tip: if the repository lives in a synced folder (OneDrive), build with
`CARGO_TARGET_DIR=%TEMP%\clip4-target` to keep build output out of the sync.

## Using it

| Action | Default key |
|---|---|
| Toggle the clipboard overlay | `Ctrl + NumPad .` |
| Toggle the snippets overlay | *(unbound)* |
| Copy text from the focused control (UI Automation, then Ctrl+C / Ctrl+A fallback) | `Ctrl + F10` |
| Paste the selected/newest item as keystrokes (clipboard untouched) | `Ctrl + F11` |
| Paste via clipboard swap + Ctrl+V, restoring your clipboard afterwards | `Ctrl + Shift + F11` |

In the overlay (press `?` or `F1` for the in-app sheet):

* Type to fuzzy-search; digits jump to an item number; `↑ ↓ PgUp PgDn Home End` move;
  `Shift+↑↓` / `Ctrl+click` multi-select; `Tab` switches between the main and pinned panes;
  `Ctrl+→/←` switch between *All* and *Snippets*; `Ctrl+F` focuses the search box.
* `Enter` pastes (2+ selected: merged text, or one item at a time for mixed content);
  `Ctrl+Enter`/`P` plain text; `U` strips tracking parameters from a URL; `M` Markdown link
  (2+ selected: merge into a new item); `H` HTML as plain text; `E` edit then paste;
  `X` edit then save as a new item; `Z` Excel fill (2+ selected); `Delete` removes.
* Single-letter commands apply while the *list* has focus; press `Ctrl+F` first to search
  for words that begin with a command letter.
* Right-click an item for pin/unpin, transforms, "paste as", image export, etc. Pinned items
  appear only in the pinned (left) pane and leave the main list; unpinning returns them to it.
  Item numbers (for number jump) count the rows of each pane's own list.
* **Image preview on hover:** rest the mouse on an image row for a moment and a larger copy of the
  image (up to 480 px on its longest side, never enlarged beyond its real size) opens beside the
  overlay — on the right if there is room, else on the left. It never takes the focus or the mouse,
  and disappears when the mouse leaves the row, scrolls, or a key is pressed.
* **Move** the overlay by dragging its header or footer; **resize** it by dragging the outer edges
  (main pane: right edge, bottom edge, corner; pinned pane: left edge, bottom edge, corner — the
  cursor changes over them). Position and size are remembered across runs (per monitor layout; a
  saved spot that is still on a connected monitor is kept after a dock/undock).
* The overlay closes by itself as soon as another window takes the focus (and if it cannot get the
  focus within 3 s).
* Snippets scope: `Enter` pastes, `A` adds, `E` edits; type `*set` + `Enter` for the manager.
  Placeholders: `{{date}} {{time}} {{datetime}} {{year}} {{month}} {{day}} {{hour}}
  {{minute}} {{second}} {{clipboard}}`.

Tray menu: Show clipboard, Copy from focused control, Snippets, Start with Windows,
Expand selected item, Settings, Restart, Exit.

Anything your password manager marks with `ExcludeClipboardContentFromMonitorProcessing`,
`CanIncludeInClipboardHistory = 0` or `Clipboard Viewer Ignore` is never recorded.

## Settings

Tray menu -> *Settings*: hotkeys, theme (15 presets), content font and size, UI size, per-element
colours (click to pick, double-click to reset), history size (10-2000, default 300), expand
selected item, start with Windows, capture sound, and *Clear history*. Theme, font, size and
colour changes apply live; hotkeys and history size apply on *Save*.

## Where things live

| What | Where |
|---|---|
| History (DPAPI-encrypted, CLP4) | `%APPDATA%\clip4\history.dat` (+ `history.dat.bak`) |
| Large payloads (>= 256 KB, DPAPI-encrypted one by one) | `%APPDATA%\clip4\blobs\<sha1>` |
| Settings | `HKCU\Software\clip4` |
| Snippets | `HKCU\Software\clip4\Snippets` |
| Log (1 MB x 3, never contains clipboard content) | `%LOCALAPPDATA%\clip4\clip4.log` |
| Crash dumps | `%LOCALAPPDATA%\clip4\crash\` |

Set the registry DWORD `HKCU\Software\clip4\DebugLog = 1` for debug logging.

The overlay's remembered placement is stored next to the settings: `OverlayPosX`/`OverlayPosY`/
`OverlayPosCfg` (main pane's top-left in pixels + a hash of the monitor layout) and
`OverlayPinnedWidth`/`OverlayMainWidth`/`OverlayHeight` (logical, 96-dpi pixels; clamped on load).
Delete them to get the default centred 972 x 520 layout back.

First run: if a clip2 history exists, clip4 offers to import it (clip2's settings and snippets
are imported automatically, once).

Developer sandbox: `set CLIP4_PROFILE=name` before starting redirects every file, registry
key and the single-instance mutex to a private sandbox (`%TEMP%\clip4-name`,
`HKCU\Software\clip4-name`), and disables the clip2 import. The integration tests use it.

## Architecture (spec section 5)

| Thread | Job |
|---|---|
| UI | overlay windows, tray, dialogs, the hidden main window; plain blocking message loop; never sleeps or does I/O |
| Hook | `WH_KEYBOARD_LL` only; classifies a key and posts a message; re-armed every 60 s |
| Paste | owns clipboard writes; all `SendInput` sequencing and sleeping |
| Capture | opens the clipboard, copies raw bytes, closes it; then builds the item |
| I/O worker | history load/save, blobs, DPAPI, WIC thumbnails, clip2 import |

Key rules (spec section 18): the clipboard lock is held only for raw byte copies; the hook
thread never blocks; every "in progress" state is RAII; registered clipboard formats are stored
by name; preview text comes from all formats; one layout function drives paint, hit-testing,
paging and scrolling.

Notes on deliberate choices:

* Windows synthesises `CF_TEXT`/`CF_OEMTEXT`/`CF_LOCALE` from `CF_UNICODETEXT` and
  `CF_DIB`/`CF_BITMAP` from `CF_DIBV5`, so only one of each family is stored; the others are
  regenerated by Windows at paste time.
* The capture thread waits 25 ms after a change notification before opening the clipboard, so
  OLE/.NET sources can finish their own `OleFlushClipboard` (otherwise their write fails while
  we wait on their delayed rendering).
* Blob files carry a small cleartext header copy of DIB headers only, so image dimensions
  show at startup without decrypting megabytes.
* The overlay is drawn with Direct2D/DirectWrite into the window's paint DC
  (`ID2D1DCRenderTarget`), which keeps the native search `EDIT` controls flicker-free.

## Integration tests

`tests/integration.rs` starts a sandboxed `clip4.exe` (private hotkeys Ctrl+Alt+Shift+F13..F16, private
data and registry key) and drives it through the real Windows clipboard and keyboard: contention
(another process holds the clipboard for 1.5 s), lock time for a 10 MB image, other apps not being
starved by large captures, privacy-format exclusion, clipboard preservation after the swap-paste
hotkey, and keystroke latency while the paste thread is typing. They are `#[ignore]`d because they
overwrite your clipboard and send keystrokes; run them only on a machine you are not using:

```powershell
cargo test --test integration -- --ignored --test-threads=1
```

`tests/clip2_real.rs` (also ignored) checks the clip2 importer against the clip2 history of the current
user and prints counts only.

## Dependencies

`windows` (Win32/COM/WIC/Direct2D/DirectWrite/DPAPI/CNG), `windows-numerics` (the point type of
the Direct2D bindings) and, at build time only, `embed-resource` (compiles the icon + manifest
resource). Nothing else: hashing is CNG, encryption is DPAPI, decoding is WIC, audio is
`PlaySoundW`.

## Developer

**Unnamed10110**

- trojan.v6@gmail.com
- sergiobritos10110@gmail.com
