# fed

`fed` is a small, fast text editor written in Rust with [egui](https://github.com/emilk/egui)
(via eframe). It is the editor you launch to look at or change a file and then close: the window
appears almost instantly and stays light. It has tabs, syntax highlighting, Italian and English
spell check, a live Markdown preview, optional vim key bindings and a settings window with
reconfigurable shortcuts.

There are no plugins, no file tree, no LSP and no split views on purpose.

## Build

Requires a current stable Rust toolchain.

```sh
cargo build --release
```

The binary is `target/release/fed`. It is self-contained: syntax definitions, themes and the
default font are built in, and there is no Qt or GTK dependency (the native file dialogs use the
XDG desktop portal on Linux). On Linux it needs the usual X11 or Wayland, libxkbcommon and
OpenGL libraries. On Windows the binary uses the GUI subsystem, so no console window opens.

## Usage

```sh
fed [files...]
```

- Each file opens in its own tab. A path that does not exist opens an empty tab bound to that
  path; the file is created on the first save.
- Without arguments `fed` opens one empty "untitled" tab, or, when "Reopen files from the last
  session" is on, the files that were open when the last `fed` window closed.
- Every launch is an independent process with its own window.
- Files are read and written as UTF-8 only: a file that is not valid UTF-8 shows an error instead
  of being opened.
- Line endings: the file is saved with its majority line ending, CRLF when CRLF lines outnumber
  bare LF lines and LF otherwise (a tie or a new file is LF), so a mostly-LF file with one stray
  CRLF is saved as all LF. A lone CR (old Mac ending) is read as LF. Only LF breaks lines: form
  feed, vertical tab, NEL and the Unicode line/paragraph separators are ordinary characters.
- The status bar shows the file path, `line:column`, the line ending, the vim mode (when on) and
  `[+]` for unsaved changes. Closing a tab or the window with unsaved changes asks first.

### Default shortcuts

On macOS `Cmd` replaces `Ctrl` in every shortcut below, except the `Ctrl+Tab` and `Ctrl+Shift+Tab` aliases.
The shortcuts in the first table can be changed in the settings window.

| Action          | Shortcut                                 |
| --------------- | ---------------------------------------- |
| New tab         | `Ctrl+N`                                 |
| Open file       | `Ctrl+O`                                 |
| Save            | `Ctrl+S`                                 |
| Save as         | `Ctrl+Shift+S`                           |
| Close tab       | `Ctrl+W`                                 |
| Next tab        | `Ctrl+2` (also `Ctrl+Tab`, fixed)        |
| Previous tab    | `Ctrl+1` (also `Ctrl+Shift+Tab`, fixed)  |
| Open settings   | `Ctrl+,`                                 |
| Find            | `Ctrl+F`                                 |
| Replace         | `Ctrl+H`                                 |
| Go to line      | `Ctrl+G`                                 |
| Toggle Markdown preview | `Ctrl+Shift+V`                   |
| Undo            | `Ctrl+Z`                                 |
| Redo            | `Ctrl+Y` (also `Ctrl+Shift+Z`, fixed)    |
| Select all      | `Ctrl+A`                                 |

Fixed editing keys:

| Keys                                   | Effect                                     |
| -------------------------------------- | ------------------------------------------ |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V`         | copy / cut / paste                         |
| Arrows, `Shift` + arrows               | move, extend the selection                 |
| `Ctrl` + Left / Right                  | word jumps (`Alt` on macOS)                |
| `Home` / `End`                         | line start / end                           |
| `Ctrl+Home` / `Ctrl+End`               | document start / end                       |
| `PageUp` / `PageDown`                  | page up / down                             |
| `Tab` / `Shift+Tab`                    | indent / outdent                           |
| Double click / triple click            | select word / line                         |
| `Enter` / `Shift+Enter` in the find bar | next / previous match (also `F3` / `Shift+F3`) |
| `Esc` in the find bar                  | close it                                   |

The find bar has toggles for case sensitivity (`Aa`) and regular expressions (`.*`) and shows the
match count. Replace offers "Replace" and "Replace all" (one undo step); in regex mode the
replacement may use groups such as `$1`.

## Vim mode

Off by default; turn it on in Settings > Vim. The status bar shows `-- NORMAL --`,
`-- INSERT --`, `-- VISUAL --` or `-- VISUAL LINE --`, and a command bar above the status bar
shows the `:` command line and messages or errors.

Supported:

- Modes: Normal, Insert, Visual (charwise), Visual Line, Command-line.
- Motions: `h j k l`, `w b e`, `W B E`, `0 ^ $`, `gg G`, `{count}G`, `f F t T` with `;` `,`,
  `%` (matching bracket), `{ }`, counts on motions.
- Operators with motions: `d`, `c`, `y`, `>`, `<` (for example `dw`, `c$`, `y}`), plus `dd`, `cc`,
  `yy`, `>>`, `<<`, `x`, `X`, `D`, `C`, `J`, `r{char}`.
- Text objects: `iw aw`, `i" a"`, `i' a'`, `i( a(`, `i{ a{`, `i[ a[` (for example `ciw`, `di"`).
- Insert: `i a I A o O`, `s S`; `Esc` returns to Normal.
- Visual: `v`, `V`, then `d x c y > < o`, or `:` for a command on the selected lines.
- Paste: `p`, `P` from one unnamed register, kept in sync with the system clipboard.
- Undo / redo: `u`, `Ctrl+R`. Repeat the last change: `.`.
- Search: `/pattern`, `?pattern`, `n`, `N`, `*`, `#`.
- Marks: `m{a-z}`, `'{a-z}` (line), `` `{a-z} `` (exact position). Marks are per tab, not saved,
  and follow edits made before them.
- Commands:
  - `:w`, `:w {file}` (writes a copy, the tab stays bound to its file), `:w! {file}` (overwrite
    an existing file), `:q`, `:q!`, `:wq`, `:x`, `:e {file}` (opens a tab; relative paths start
    from the current file's directory).
  - `:q`, `:wq` and `:x` close the current tab; closing the last tab exits. `:q` refuses unsaved
    changes, `:q!` discards them.
  - `:{n}`, `:$`, `:'a` jump to a line.
  - Substitution: `:s/a/b/`, `:%s/a/b/g`, `:{n},{m}s/a/b/[g][i]`, `:1,$s/...`, with `.`, `$`,
    `'a` and `'<,'>` (Visual selection) as addresses.

**Patterns use Rust [`regex`](https://docs.rs/regex) syntax, not vim regex syntax.** For example
write `(foo|bar)` instead of `\(foo\|bar\)`, and use `$1` (or `${1}`) in the replacement for the
first capture group instead of `\1`.

Not supported: macros, named registers, `.vimrc` or any other vim configuration.

With vim mode on, the app shortcuts still work when they use `Ctrl` (`Cmd` on macOS), except
Undo, Redo and Select all, which belong to vim (`u`, `Ctrl+R`, ...).

## Settings

Open the settings window with `Ctrl+,`. Changes apply immediately and are saved automatically
(font size and tab width are saved when you stop dragging, or at most every 500 ms while
dragging). Sections:

- Editor: font family (system fonts, scanned the first time the window opens), font size, tab
  width, insert spaces instead of tabs, line numbers, word wrap.
- Appearance: UI theme (System, Light, Dark), code theme for light and for dark mode.
- Vim: enable vim key bindings.
- Spell check: enable, status, "Download dictionaries" after a failed download.
- Session: reopen the files of the last session.
- Key bindings: click "Change", then press the new key combination (`Esc` cancels). A shortcut
  needs `Ctrl`, `Cmd` or `Alt`, unless it is a function key `F1` to `F20`. A combination already
  used by another action shows a prompt to replace it or cancel. "Reset to defaults" restores the
  table above.

Settings are stored in `settings.toml` (with `session.toml` and the spelling files next to it)
in the config directory:

| OS      | Config directory                                        |
| ------- | ------------------------------------------------------- |
| Linux   | `$XDG_CONFIG_HOME/fed/`, usually `~/.config/fed/`       |
| Windows | `%APPDATA%\fed\config\`                                 |
| macOS   | `~/Library/Application Support/fed/`                    |

The file can be edited by hand. Unknown or invalid entries fall back to their defaults with a
warning in the status bar. Key bindings are overridden in a `[keybindings]` table with action
names as keys, for example:

```toml
[keybindings]
save = "Ctrl+S"
toggle_preview = "Ctrl+Alt+P"
select_all = ""   # an empty string leaves the action unbound
```

Action names: `new_tab`, `open_file`, `save`, `save_as`, `close_tab`, `next_tab`, `prev_tab`,
`open_settings`, `find`, `replace`, `go_to_line`, `toggle_preview`, `undo`, `redo`,
`select_all`.

## Spell check

Italian and English are checked together: a word is correct when either dictionary accepts it.
In source files only comments and string literals are checked; in Markdown everything except
code spans and code blocks; in plain text everything. Misspelled words are underlined in red.
Right click one for suggestions and "Add to dictionary" (a word-per-line `personal.dic` in the
config directory). Spell check is on by default and can be turned off in the settings.

Dictionaries (Hunspell `.aff` + `.dic`) are not bundled. They load on a background thread after
the window is shown, looking in this order:

1. `<config dir>/dictionaries/` (`it_IT.aff`, `it_IT.dic`, `en_US.aff`, `en_US.dic`);
2. on Linux, `/usr/share/hunspell`, `/usr/share/myspell` and `/usr/share/myspell/dicts`;
3. otherwise they are downloaded once from the
   [LibreOffice dictionaries repository](https://github.com/LibreOffice/dictionaries)
   into `<config dir>/dictionaries/`.

The status bar shows the download and any error. There is no automatic retry: use "retry" in
the status bar or "Download dictionaries" in the settings. To work offline, place the four files
in `<config dir>/dictionaries/` yourself.

## Markdown preview

For `.md`, `.markdown` and `.mdown` tabs, `Ctrl+Shift+V` or the "Preview" button in the tab bar
opens a live preview panel on the right. It renders CommonMark plus tables, task lists, footnotes
and strikethrough, and local images relative to the file. Remote images are not fetched (their
alt text is shown) and raw HTML is not rendered. Code blocks are shown in monospace without
colors. Clicking an `http`, `https` or `mailto` link opens it with the system handler
(the default browser for web links); other links, such as relative ones and `file:` links
(which could launch programs), are ignored. If the
system cannot open it, the error shows in the status bar.

## Large files

Files up to 256 MB open; bigger ones are refused with an error in the status bar. Files of
1 MB or more are read on a background thread, so the window stays responsive and the tab
appears when the file is loaded. When a file is 5 MB or more, or has a line longer than 10 000
characters, syntax highlighting, spell check and word wrap are turned off for that tab and the
status bar says so.

## Performance

Syntax definitions, dictionaries and the font list load on background threads, so nothing heavy
runs before the first frame. Highlighting is cached per line and re-parsed only from the edited
line, and only up to the visible lines.

Startup can be traced with `FED_TRACE_STARTUP=1`, which prints the time from process start to
the end of the first frame on stderr:

```sh
FED_TRACE_STARTUP=1 fed notes.txt
# fed: first frame after 64.9 ms
```

Measured on the development machine (Linux, X11, release build, one small file, 5 runs):

| Configuration      | First frame (ms)                   |
| ------------------ | ---------------------------------- |
| Spell check off    | 63.5, 64.9, 75.6, 67.2, 63.2       |
| Spell check on     | 67.3, 65.5, 69.0, 65.2, 66.4       |

Memory (VmRSS from `/proc/<pid>/status`, release build, spell check on):

| Situation                                                        | VmRSS   |
| ---------------------------------------------------------------- | ------- |
| One small file                                                   | ~82 MB  |
| One small file, spell check off                                  | ~71 MB  |
| Five 5 000-line Rust files, each tab shown and scrolled to the end | ~152 MB |

Typing in a 5 000-line Rust file with highlighting and spell check on shows no visible lag
(about 1.3 ms of CPU per key at the end of the file, 3.6 ms at the top).
