# fed - design

Date: 2026-11-11
Status: approved

## 1. Intent and why

`fed` is a personal, small and fast text editor written in Rust with egui (via eframe).

It exists because the user needed to open a file quickly and the only editor available was Kate,
which is slow to start, heavy, and drags in the whole Qt dependency. `fed` is meant to be the thing
you launch to look at or edit a file and then close: it must appear almost instantly and stay light.

Typical files: source code and Markdown. Writing languages: Italian and English.

Target platforms: Linux, Windows, macOS.

## 2. Goals

- Near-instant startup: the window appears and is usable before any heavy work (dictionaries,
  syntax sets, font enumeration) is finished. Heavy work is loaded lazily or in background threads.
- Single self-contained binary per platform, no Qt/GTK runtime dependency.
- Multiple tabs, one file per tab.
- Optional vim bindings (core subset, see section 5).
- Syntax highlighting for source code, with multiple color themes.
- Spell check for Italian and English.
- Live side-by-side Markdown preview.
- Settings panel (font size, font family, themes, vim on/off, session restore, key bindings).

## 3. Non-goals

- Plugins, extensions, scripting.
- Project/workspace management, file tree, integrated terminal.
- LSP, autocompletion, linting, debugging.
- Vim macros, named registers, `.vimrc` or any external vim config.
- Split views of code (the only side-by-side view is the Markdown preview).

## 4. Anti-goals (failure even if it "works")

- Becoming "Kate in Rust": feature creep that hurts startup time or memory.
- Startup blocked by loading dictionaries, syntax definitions or fonts.
- Typing lag on normal-sized files (up to a few thousand lines).

## 5. Features

### 5.1 Tabs and files

- `fed file1 file2 ...` opens one tab per file. A path that does not exist opens an empty tab bound
  to that path (created on first save).
- Launch with no arguments: empty window with one empty "untitled" tab.
- Default shortcuts (all reconfigurable, see 5.7):
  - `Ctrl+N` new tab
  - `Ctrl+O` open file (native file dialog)
  - `Ctrl+S` save, `Ctrl+Shift+S` save as
  - `Ctrl+W` close tab (asks confirmation if there are unsaved changes)
  - `Ctrl+Tab` / `Ctrl+Shift+Tab` next / previous tab
  - `Ctrl+,` open settings
  - On macOS, `Cmd` replaces `Ctrl` in the defaults.
- Tab label shows file name and a modified marker. Closing the window with unsaved changes asks for
  confirmation.
- Session restore: setting "Restore previous session tabs", OFF by default. When ON and fed is
  launched with no arguments, it reopens the files that were open at last exit.
- Files are read and written as UTF-8. Line endings are preserved as found (LF or CRLF).
  [builder call: non-UTF-8 files show an error instead of being opened lossily]

### 5.1.1 Standard (non-vim) editing

User decision: the plain editor gets a find/replace bar and standard editing keys. This is the default
experience, since vim mode is OFF by default.

- Find bar (`Ctrl+F`): incremental search, next/previous (`Enter` / `Shift+Enter`, `F3` /
  `Shift+F3`), match count, toggles for case sensitivity and regex.
- Replace (`Ctrl+H`): same bar with a replace field, "Replace" and "Replace all".
- Go to line (`Ctrl+G`).
- Standard keys: `Ctrl+Z` / `Ctrl+Y` (and `Ctrl+Shift+Z`) undo/redo, `Ctrl+A`, `Ctrl+C/X/V`,
  Shift + arrows selection, `Ctrl` + arrows word jumps, `Home` / `End`, `Ctrl+Home` / `Ctrl+End`,
  `PageUp` / `PageDown`, double-click selects a word, triple-click selects a line,
  `Tab` / `Shift+Tab` indent / outdent selection.
- The search/replace engine is shared with vim (`/`, `n`, `:s`), so it lives in its own module,
  not inside `vim`.
- All these shortcuts are part of the reconfigurable keymap (5.7), on macOS with `Cmd`.

### 5.1.2 Instances and large files

- [builder call] No single-instance logic: each `fed` launch is an independent process with its own
  window. Session restore saves the tabs of the last instance to exit.
- [builder call] Large files always open. Above about 5 MB, or when a line exceeds about 10 000
  characters, highlighting, spell check and word wrap are disabled for that tab, with a notice in
  the status bar.

### 5.2 Syntax highlighting

- Language chosen by file extension.
- Library: `syntect` (builder call), using its bundled syntax and theme sets, with the pure-Rust
  regex backend (`fancy-regex`) to avoid C dependencies on Windows/macOS.
- Multiple code color themes, selectable in settings; independent from the UI light/dark choice.
- Highlighting must not cause typing lag: cache highlighted lines and re-highlight from the edited
  line onwards, or only the visible region. [builder call on exact strategy]

### 5.3 Spell check

- Languages: Italian and English, checked together (a word is correct if either dictionary accepts it).
- In source files: only comments and string literals are checked (identified from the syntect scopes).
- In Markdown: all text except code spans and code blocks.
- Misspelled words are underlined (wavy or straight red underline).
- Right-click on a misspelled word shows suggestions and "Add to personal dictionary".
  Personal dictionary is a plain word-per-line file in the config directory.
- Spell check can be toggled globally in settings (default ON).
- Dictionaries are Hunspell format (`.aff` + `.dic`), loaded in a background thread after the window
  is shown. Suggested library: `spellbook` (pure Rust, Hunspell compatible) [builder call].
- Dictionary source (user decision): not bundled in the binary. Downloaded on first use into
  `<config dir>/dictionaries/`, then loaded from there.
  - [builder call] Source: LibreOffice dictionaries repository (`it_IT`, `en_US`), fetched over
    HTTPS with a small pure-Rust client (e.g. `ureq` with rustls), in a background thread.
  - [builder call] English variant: `en_US` only.
  - [builder call] Download happens automatically the first time spell check is active and the files
    are missing; progress/errors shown in the status bar; no retry loop, a "Download dictionaries"
    button in settings retries manually.
  - Fallback: if files are already present in `<config dir>/dictionaries/` (placed by the user),
    no download. On Linux, system Hunspell paths (`/usr/share/hunspell`, `/usr/share/myspell`) are
    also checked before downloading.
  - Licensing of fed is unaffected since dictionaries are not distributed with it.

### 5.4 Markdown preview

- Available for `.md` / `.markdown` tabs.
- Side-by-side panel to the right of the text, updated live while typing.
- Toggled per tab with a shortcut (default `Ctrl+Shift+V`, reconfigurable) and a toolbar/menu entry.
- Rendering in egui; suggested `egui_commonmark` [builder call]. Code blocks in the preview use
  syntect highlighting if cheap to wire, otherwise monospace without colors.
- Scroll sync between editor and preview is nice-to-have, not required.
- [builder call] Rendered: CommonMark plus tables, task lists, footnotes, strikethrough; local images
  with paths relative to the Markdown file. Not rendered: remote images, raw HTML (shown as text or
  skipped). Clicking a link opens it in the system browser.

### 5.5 Vim mode

OFF by default, toggled in settings. When ON:

- Modes: Normal, Insert, Visual (charwise), Visual Line, Command-line.
- Current mode shown in the status bar.
- Motions: `h j k l`, `w b e`, `W B E`, `0 ^ $`, `gg G`, `{count}G`, `f F t T` + `;` `,`,
  `%` (matching bracket), counts on motions.
- Operators combined with motions: `d`, `c`, `y` (e.g. `dw`, `c$`, `y}`), plus `dd`, `cc`, `yy`,
  `x`, `X`, `D`, `C`, `J`, `r{char}`, `>>`, `<<`.
- Basic text objects: `iw aw`, `i" a"`, `i' a'`, `i( a(`, `i{ a{`, `i[ a[` (e.g. `ciw`, `di"`).
- Insert entry: `i a I A o O`, `s S`. `Esc` returns to Normal.
- Paste: `p`, `P` from a single unnamed register.
  [builder call: the unnamed register is also synced with the system clipboard]
- Undo/redo: `u`, `Ctrl+R`. Repeat last change `.`.
- Search: `/pattern`, `?pattern`, `n`, `N`, `*`, `#`.
- Marks: `m{a-z}`, `'{a-z}` (jump to line), `` `{a-z} `` (jump to exact position). Marks are per tab,
  not persisted, and their positions are adjusted when text before them is edited.
- Command-line (`:`):
  - `:w`, `:w {file}`, `:q`, `:q!`, `:wq`, `:x`, `:e {file}`
  - [builder call] Tab semantics: `:e {file}` opens the file in a new tab, but reuses the current
    tab if it is an empty untitled tab. `:q`, `:wq`, `:x` close the current tab; closing the last
    tab exits the app. `:q` refuses on unsaved changes, `:q!` discards. `:w {file}` writes a copy
    and keeps the tab bound to its original path (vim behavior).
  - `:{n}` jump to line
  - Substitution with ranges: `:s/a/b/`, `:%s/a/b/g`, `:{n},{m}s/a/b/[g][i]`, `:1,$s/...`,
    `.` and `$` as range addresses, and `'<,'>` for the visual selection.
  - Patterns use Rust `regex` syntax, not vim regex syntax (user-visible note in docs).
- The command line is shown in a dedicated bar at the bottom of the window, in a monospace font,
  clearly visible (high contrast, slightly larger than the status text). Errors (unknown command,
  pattern not found) are shown in the same bar in an error color.
- When vim mode is ON, the configurable app shortcuts (5.7) still work as long as they do not
  collide with a vim key in the current mode. [builder call on precedence details]

### 5.6 Appearance

- UI theme: Light, Dark, or Follow system (default Follow system).
- Code theme: chosen from syntect themes; separate defaults for light and dark UI.
- Editor font: family chosen from installed monospace and proportional fonts (enumeration done
  lazily when the settings panel is opened, e.g. with `fontdb`), size in points.
- Bundled default monospace font so the editor looks right with no system font lookup at startup.

### 5.7 Settings panel

- Opened with `Ctrl+,` (reconfigurable), as an egui window or side panel.
- Sections:
  - Editor: font family, font size, tab width, insert spaces vs tabs, show line numbers, word wrap.
    [builder call: tab width, spaces, line numbers, wrap are small additions; keep them simple]
  - Appearance: UI theme, code theme (light), code theme (dark).
  - Vim: enable vim mode.
  - Spell check: enable.
  - Session: restore previous session tabs.
  - Key bindings: list of actions with their current shortcut. Clicking "Change" enters a live
    capture state: the next key combination pressed (modifiers + key) is recorded and shown.
    `Esc` cancels the capture. Conflicts with another action are shown and must be resolved
    (replace or cancel). A "Reset to defaults" button exists.
- Changes apply immediately and are saved automatically.
- Persistence: a TOML file in the platform config directory (`directories` crate:
  `~/.config/fed/` on Linux, `%APPDATA%\fed\` on Windows, `~/Library/Application Support/fed/` on
  macOS). Unknown or invalid entries fall back to defaults with a warning, never a crash.

### 5.8 Status bar

- Shows: file path, cursor line:column, line ending, vim mode (when enabled), modified state.

## 6. Constraints

- Language: Rust (stable, current toolchain 1.99). GUI: egui/eframe, latest stable versions.
- Pure-Rust dependencies preferred; avoid C libraries that complicate cross-platform builds.
- Native file dialog: `rfd`; on Linux use the XDG portal backend so GTK is not required.
- All code and comments in English. No em dash characters anywhere.
- Project conventions from the user: `CHANGES.md` at repo root tracking progressive changes; public
  items documented with doc comments; constants in UPPER_CASE; early returns at top of functions.
- Tests: unit tests for non-UI logic (vim state machine and commands, substitution parser, buffer
  edits, mark adjustment, key binding (de)serialization, settings loading, spell-check region
  extraction). UI is verified manually.

## 7. Architecture sketch (builder may refine)

- `app`: eframe `App`, top-level layout (tab bar, editor area, preview, status bar, command bar),
  settings window.
- `buffer`: text storage per tab (rope, e.g. `ropey`), path, modified flag, line ending, undo history.
- `editor`: rendering of a buffer in egui with highlighting and spell underlines, cursor/selection,
  input handling in non-vim mode.
- `vim`: mode state machine, parser for counts/operators/motions/text objects, command-line parser
  and executor. Pure logic operating on `buffer`, testable without egui.
- `highlight`: syntect wrapper with per-line caching.
- `search`: find/replace engine (literal and regex), shared by the find bar and vim.
- `spell`: dictionary download and loading (background), word extraction from checkable regions, suggestions,
  personal dictionary.
- `markdown`: preview panel.
- `keymap`: actions, default bindings, capture, conflict detection, (de)serialization.
- `config`: settings struct, TOML load/save, session state.

Note: egui's built-in `TextEdit` may not be enough for vim mode, wavy underlines and incremental
highlighting. The builder decides whether to use `TextEdit` with a custom layouter or a custom
editor widget; a custom widget is the expected outcome.

## 8. What is left to the builder

- Exact crate choices and versions where marked as suggestions.
- Rendering/highlighting caching strategy.
- Exact menu layout and toolbar presence.
- Exact key precedence between vim and app shortcuts, within the rule in 5.5.

## 9. Performance and delivery targets

- [builder call] Startup target: window visible and editable in under 200 ms on a mid-range laptop
  (release build, one small file). Syntect sets, dictionaries and font enumeration must not block
  the first frame. eframe backend: `glow`.
- [builder call] On Linux, depending on system X11/Wayland, libxkbcommon and OpenGL libraries is
  acceptable; Qt and GTK are not.
- [builder call] v1 delivery: `cargo build --release` on each OS. Windows build uses the GUI
  subsystem (no console window). No signing, notarization, installers or CI in v1. Intermediate
  milestones are verified on Linux; Windows and macOS are checked at the end.

## 10. Open questions

None.
