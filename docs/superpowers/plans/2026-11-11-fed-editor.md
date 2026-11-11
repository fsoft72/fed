# fed Editor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `fed`, a fast, light multi-tab text editor in Rust + egui with optional vim bindings, syntax highlighting, spell check (it/en), live Markdown preview and a settings panel.

**Architecture:** A library crate (`src/lib.rs`) holds all logic in focused modules; `src/main.rs` only parses CLI args and starts eframe. Text logic (buffer, search, editing commands, vim, keymap, config, spell region extraction) is pure Rust with no egui dependency, unit-tested with `cargo test`. The UI is a custom egui editor widget plus app shell, verified manually. Heavy assets (syntect sets, dictionaries, font list) load in background threads and never block the first frame.

**Tech Stack:** Rust 1.99 stable, eframe/egui 0.37 (glow backend), ropey 1.6, syntect 5.3 (`default-fancy`), regex 1.13, egui_commonmark 0.26 + pulldown-cmark 0.13, spellbook 0.4, ureq 3 (rustls), rfd 0.18 (xdg-portal), fontdb 0.24, directories 6, serde + toml 1, thiserror 2, open 5.

**Spec:** `docs/superpowers/specs/2026-11-11-fed-editor-design.md`

## Global Constraints

- All code and comments in English. Never use the em dash character anywhere (code, comments, docs).
- Every public item has a `///` doc comment; private functions get a short `//` or `///` comment too.
- Constants in `UPPER_SNAKE_CASE`. Early-return guard clauses at the top of functions, one line when possible.
- No Qt/GTK dependency: `rfd` with `default-features = false, features = ["xdg-portal", "wayland"]`.
- eframe with `default-features = false` and features `["glow", "default_fonts", "wayland", "x11", "accesskit"]`.
- syntect with `default-features = false, features = ["default-fancy"]` (pure Rust regex).
- Windows: `#![cfg_attr(windows, windows_subsystem = "windows")]` in `main.rs` (release and debug).
- Startup: no blocking load of syntect sets, dictionaries or font enumeration before the first frame.
- Large file thresholds: `LARGE_FILE_BYTES = 5_000_000`, `LONG_LINE_CHARS = 10_000`.
- Files: UTF-8 only; non-UTF-8 produces an error, never lossy decode. Line endings LF/CRLF preserved.
- Config dir via `directories::ProjectDirs::from("", "", "fed")` (`config_dir()`).
- Dictionaries: `it_IT`, `en_US`, downloaded from `https://raw.githubusercontent.com/LibreOffice/dictionaries/master/{it_IT/it_IT,en/en_US}.{aff,dic}`.
- Keep `CHANGES.md` at repo root updated by every task (one bullet per task under the date heading).
- Every task ends with `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test` all green, then a commit with only the task's files. Commit messages have no AI attribution lines. Never use `--no-verify`.
- macOS: default shortcuts use `Cmd` instead of `Ctrl` (egui `Modifiers::command`).

## Review Focus

1. Unsaved changes on window close / tab close / `:q`: user expects a confirmation, never silent data loss. Test in Task 7 (`close_requires_confirmation_when_modified`).
2. CRLF file edited and saved: must stay CRLF, cursor columns must not count `\r`. Test in Task 2 (`crlf_roundtrip_preserves_endings`).
3. Multi-byte text (accented Italian, emoji) in cursor movement, search and `:s`: char/byte index confusion causes panics. Tests in Task 3 (`search_handles_multibyte`) and Task 4 (`move_word_over_accented_text`).
4. Invalid regex typed in find bar or `:s`: must show an error, not panic. Tests in Task 3 (`invalid_regex_is_error`) and Task 10 (`substitute_invalid_regex_reports_error`).
5. Corrupt or hand-edited settings TOML: fall back per field with warnings, never crash. Test in Task 5 (`invalid_field_falls_back_with_warning`).

---

## File Structure

```
Cargo.toml
src/main.rs                 CLI args, eframe startup
src/lib.rs                  module declarations
src/buffer.rs               Buffer (ropey), load/save, line endings, undo, marks, dirty line
src/text_nav.rs             char classes, word boundaries, line helpers (shared by editor and vim)
src/search.rs               SearchQuery, find_next, find_all, substitute over line range
src/editing.rs              EditorState (selection), EditCommand, apply_command (non-vim editing)
src/config.rs               Settings, Session, ConfigPaths, lenient TOML load/save
src/keymap.rs               Action, KeyChord, Keymap, defaults, conflicts
src/tab.rs                  Tab (buffer + editor state + vim + caches)
src/app.rs                  FedApp (eframe::App), layout, tabs, dialogs, shortcut dispatch
src/view.rs                 custom editor widget (rendering, mouse, input translation)
src/find_bar.rs             find/replace and go-to-line bars
src/highlight.rs            HighlightAssets (async), HighlightCache, StyledSpan
src/spell/mod.rs            SpellService (async), SpellChecker
src/spell/words.rs          word tokenizing, checkable ranges (code spans, markdown)
src/spell/dict.rs           dictionary locate + download
src/markdown.rs             preview panel
src/vim/mod.rs              VimState, VimKey, Mode, VimEffect, handle_key
src/vim/motion.rs           Motion enum + resolution
src/vim/textobj.rs          text objects
src/vim/cmdline.rs          ex command parser + executor
src/fonts.rs                FontCatalog (async fontdb scan), apply fonts to egui
src/settings_ui.rs          settings window incl. key capture
tests/                      none; unit tests live in `#[cfg(test)] mod tests` in each file
```

---

### Task 1: Project scaffold and empty window

**Files:** Create `Cargo.toml`, `src/main.rs`, `src/lib.rs`, `src/app.rs`, `.gitignore`, `rustfmt.toml` (`max_width = 100`). Modify `CHANGES.md`.

**Interfaces:**
- Produces: `fed::app::FedApp::new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> FedApp`; `fed::run(files: Vec<PathBuf>) -> eframe::Result<()>` in `lib.rs`.

- [ ] **Step 1:** `cargo init --lib --name fed`, add `[[bin]] name = "fed" path = "src/main.rs"`, all dependencies from Tech Stack with features from Global Constraints (`ureq` with default `rustls`, `egui_commonmark` with features `["load-images", "better_syntax_highlighting"]` only if it compiles with syntect fancy, else `["load-images"]`; `egui_extras` 0.37 with `["file", "image"]`; `image` with `["png", "jpeg", "gif", "webp"]`). `[profile.release] lto = "thin", codegen-units = 1, strip = true`.
- [ ] **Step 2:** `main.rs`: collect `std::env::args_os().skip(1)` into `Vec<PathBuf>`, call `fed::run`. `run` uses `eframe::NativeOptions` with renderer `Glow`, title `"fed"`, initial size 1000x700, and `FedApp::new`.
- [ ] **Step 3:** `FedApp` draws a central panel with the text "fed". Add `#[cfg(test)] fn smoke() { assert!(true) }`-style test only if needed for `cargo test` to run; not required.
- [ ] **Step 4:** Run `cargo build --release` - expected: success. Run `./target/release/fed` - expected: window appears. Run `cargo clippy --all-targets -- -D warnings` - clean.
- [ ] **Step 5:** Update `CHANGES.md`, commit `chore: scaffold fed crate with empty eframe window`.

### Task 2: Buffer

**Files:** Create `src/buffer.rs`. Modify `src/lib.rs`, `CHANGES.md`.

**Interfaces:**
- Produces:
  - `pub enum LineEnding { Lf, Crlf }` (`Default = Lf`, `as_str()`).
  - `#[derive(thiserror::Error, Debug)] pub enum BufferError { #[error("{0}: file is not valid UTF-8")] NotUtf8(PathBuf), #[error("{path}: {source}")] Io { path: PathBuf, source: std::io::Error }, #[error("buffer has no file path")] NoPath }`
  - `pub struct Buffer` with: `new() -> Buffer`, `from_text(text: &str) -> Buffer`, `load(path: &Path) -> Result<Buffer, BufferError>` (missing file => empty buffer bound to path), `path(&self) -> Option<&Path>`, `set_path(&mut self, PathBuf)`, `save(&mut self) -> Result<(), BufferError>`, `save_as(&mut self, path: PathBuf) -> Result<(), BufferError>`, `write_copy(&self, path: &Path) -> Result<(), BufferError>`, `rope(&self) -> &ropey::Rope`, `text(&self) -> String`, `line_ending(&self) -> LineEnding`, `is_modified(&self) -> bool`, `is_large(&self) -> bool`, `revision(&self) -> u64`, `replace(&mut self, range: Range<usize>, text: &str)` (char indices; insert = empty range, delete = empty text), `begin_group(&mut self)`, `end_group(&mut self)`, `undo(&mut self) -> Option<usize>`, `redo(&mut self) -> Option<usize>` (return char index for the cursor), `set_mark(&mut self, name: char, pos: usize)`, `mark(&self, name: char) -> Option<usize>`, `take_dirty_line(&mut self) -> Option<usize>` (lowest line touched since last take), `display_name(&self) -> String` (file name or `"untitled"`).
- Rope content always uses `\n`; CRLF converted on load and restored on save/write. `\r` never appears in the rope.
- Undo: each `replace` outside a group is its own undo step; between `begin_group`/`end_group` (nestable counter) edits merge into one step. Consecutive single-char inserts typed in sequence are merged by the caller via groups, not by the buffer. `is_modified` is false when the history position equals the saved position.
- Marks: on `replace(range, text)`, a mark at `pos >= range.end` shifts by `new_len - old_len`; a mark inside `range` moves to `range.start`.
- `is_large`: computed at load and after each replace is NOT required; compute at load/from_text only.
- Save writes atomically: write to `<path>.fed-tmp` in the same dir, then rename.

- [ ] **Step 1: Write failing tests** in `buffer.rs` `mod tests`, using `std::env::temp_dir()` subdirectories with unique names:
  - `crlf_roundtrip_preserves_endings`: write `"a\r\nb\r\n"`, load, assert `rope().to_string() == "a\nb\n"`, `line_ending() == Crlf`, replace `0..1` with `"x"`, save, file bytes == `"x\r\nb\r\n"`.
  - `non_utf8_is_error`: file bytes `[0xff, 0xfe, 0x00]` => `Err(BufferError::NotUtf8(_))`.
  - `missing_file_opens_empty_with_path`: load nonexistent path => Ok, text empty, path set, not modified.
  - `undo_redo_restores_text_and_modified_flag`: from_text("abc"), replace 3..3 "d", undo => "abc", not modified, redo => "abcd", modified.
  - `group_undoes_as_one_step`: begin_group, two replaces, end_group, one undo restores original.
  - `marks_shift_on_edit`: from_text("hello world"), set_mark('a', 6), replace 0..0 "XX" => mark 8; replace 0..9 "" => mark 0.
  - `dirty_line_reports_lowest_edited_line`: three lines, edit line 2 then line 1, take => Some(1), take again => None.
  - `large_file_flag`: from_text of a 10_001 char single line => is_large.
- [ ] **Step 2:** `cargo test buffer` - expected FAIL (missing items).
- [ ] **Step 3:** Implement `Buffer` in `src/buffer.rs`.
- [ ] **Step 4:** `cargo test buffer` - PASS. fmt, clippy clean.
- [ ] **Step 5:** Update `CHANGES.md`, commit `feat: add rope-backed buffer with undo, marks and line endings`.

### Task 3: Search engine

**Files:** Create `src/search.rs`. Modify `src/lib.rs`, `CHANGES.md`.

**Interfaces:**
- Consumes: `Buffer::{rope, replace, begin_group, end_group}`.
- Produces:
  - `pub struct SearchQuery { pub pattern: String, pub regex: bool, pub case_sensitive: bool }`
  - `#[derive(thiserror::Error, Debug)] pub enum SearchError { #[error("invalid pattern: {0}")] InvalidPattern(String), #[error("pattern not found")] NotFound }`
  - `SearchQuery::compile(&self) -> Result<regex::Regex, SearchError>` (literal => `regex::escape`; case-insensitive => `(?i)` via `RegexBuilder`).
  - `pub fn find_all(rope: &Rope, re: &Regex) -> Vec<Range<usize>>` (char ranges, non-empty matches only).
  - `pub fn find_next(rope: &Rope, re: &Regex, from: usize, forward: bool) -> Option<Range<usize>>` (wraps around; forward finds first match starting at `> from` when `from` is a match start, else `>= from`; backward finds last match starting `< from`).
  - `pub fn substitute(buf: &mut Buffer, re: &Regex, replacement: &str, lines: Range<usize>, global: bool) -> Result<usize, SearchError>` (line range end-exclusive, 0-based; one undo group; returns count; `NotFound` when 0; replacement uses `regex` `$1` syntax).
  - `pub fn replace_all(buf: &mut Buffer, re: &Regex, replacement: &str) -> usize` (whole buffer, one undo group).

- [ ] **Step 1: Failing tests:** `literal_query_escapes_metachars` (`"a.b"` literal does not match `"axb"`), `case_insensitive_matches`, `find_next_wraps`, `find_next_backward`, `search_handles_multibyte` (text `"perché è così"`, find `"è"` => char range `11..12`), `invalid_regex_is_error` (`"("` regex => `InvalidPattern`), `substitute_line_range_global` (lines `"aa\naa\naa"`, lines 1..3, `a`->`b`, global => `"aa\nbb\nbb"`, count 4), `substitute_first_only` (non-global => `"ba\nba\nba"` for 0..3), `substitute_capture_groups` (`(\w+)@` -> `[$1]`), `substitute_single_undo`, `substitute_not_found`.
- [ ] **Step 2:** run, FAIL. **Step 3:** implement (work on `rope.line(i).to_string()` per line; convert byte offsets to char offsets with `str::char_indices` or `rope.byte_to_char`). **Step 4:** PASS, fmt, clippy.
- [ ] **Step 5:** `CHANGES.md`, commit `feat: add search and substitute engine`.

### Task 4: Text navigation and non-vim editing logic

**Files:** Create `src/text_nav.rs`, `src/editing.rs`. Modify `src/lib.rs`, `CHANGES.md`.

**Interfaces:**
- Consumes: `Buffer`.
- Produces in `text_nav.rs`:
  - `pub enum CharClass { Whitespace, Word, Punct }`, `pub fn char_class(c: char, big: bool) -> CharClass` (`big` => everything non-whitespace is `Word`; `Word` = alphanumeric or `_`).
  - `pub fn next_word_start(rope: &Rope, pos: usize, big: bool) -> usize`, `prev_word_start(..)`, `word_end(rope, pos, big) -> usize` (vim `e` semantics: end char index, inclusive), `word_range_at(rope, pos) -> Range<usize>`, `line_start(rope, pos) -> usize`, `line_end(rope, pos) -> usize` (index of the `\n` or len), `first_non_blank(rope, pos) -> usize`, `line_col(rope, pos) -> (usize, usize)`, `pos_at(rope, line, col) -> usize` (col clamped to line length excluding `\n`), `matching_bracket(rope, pos) -> Option<usize>` for `()[]{}`.
- Produces in `editing.rs`:
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)] pub struct Selection { pub anchor: usize, pub head: usize }` with `range() -> Range<usize>`, `is_empty()`, `caret(pos) -> Selection`.
  - `pub struct EditorState { pub selection: Selection, pub preferred_col: Option<usize> }` (Default).
  - `pub enum EditCommand { Left{select: bool}, Right{select}, Up{select}, Down{select}, WordLeft{select}, WordRight{select}, Home{select}, End{select}, DocStart{select}, DocEnd{select}, PageUp{select, lines: usize}, PageDown{select, lines}, SelectAll, SelectWordAt(usize), SelectLineAt(usize), SetCaret{pos: usize, select: bool}, InsertText(String), Newline, Backspace, Delete, Indent, Outdent, Undo, Redo, Cut, Copy, Paste(String) }`
  - `pub struct EditSettings { pub tab_width: usize, pub insert_spaces: bool }`
  - `pub fn apply_command(state: &mut EditorState, buf: &mut Buffer, cmd: EditCommand, settings: &EditSettings) -> Option<String>` (returns clipboard text for `Cut`/`Copy`, `None` otherwise; `Cut`/`Copy` with empty selection act on the whole current line including `\n`).
  - Rules: `Newline` copies the leading whitespace of the current line (auto-indent, builder call). `InsertText` replaces the selection. `Indent`/`Outdent` with a multi-line selection act on every selected line; with no selection `Indent` inserts a tab or spaces to the next tab stop. `Home` toggles between first non-blank and column 0. `Up`/`Down` keep `preferred_col`.

- [ ] **Step 1: Failing tests:** `move_word_over_accented_text` (`"città è bella"`, WordRight from 0 => 6, again => 8), `word_end_vim_semantics`, `matching_bracket_nested`, `insert_replaces_selection`, `newline_keeps_indent`, `backspace_at_line_start_joins`, `indent_multiline_selection_spaces` (tab_width 4), `outdent_removes_up_to_tab_width`, `up_down_keep_preferred_col`, `home_toggles_first_non_blank`, `copy_without_selection_copies_line`, `select_word_at_middle`.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS, fmt, clippy.
- [ ] **Step 5:** `CHANGES.md`, commit `feat: add text navigation and editing commands`.

### Task 5: Config, session and keymap

**Files:** Create `src/config.rs`, `src/keymap.rs`. Modify `src/lib.rs`, `CHANGES.md`.

**Interfaces:**
- Produces in `config.rs`:
  - `pub enum UiTheme { System, Light, Dark }` (serde lowercase, default `System`).
  - `pub struct Settings { pub font_family: Option<String>, pub font_size: f32 /*14.0, clamp 6..=72*/, pub tab_width: usize /*4, clamp 1..=16*/, pub insert_spaces: bool /*true*/, pub line_numbers: bool /*true*/, pub word_wrap: bool /*false*/, pub ui_theme: UiTheme, pub code_theme_light: String /*"InspiredGitHub"*/, pub code_theme_dark: String /*"base16-ocean.dark"*/, pub vim_mode: bool /*false*/, pub spell_check: bool /*true*/, pub restore_session: bool /*false*/, pub keybindings: BTreeMap<String, String> /*action name -> chord string; empty = defaults*/ }` with `Default`, `Serialize`.
  - `Settings::from_toml_str(s: &str) -> (Settings, Vec<String>)`: parse into `toml::Table`; for each known key, try to convert; on type error or out-of-range, keep the default and push a warning naming the key; unknown keys produce a warning; a TOML syntax error yields defaults + one warning.
  - `Settings::to_toml_string(&self) -> String`.
  - `pub struct ConfigPaths { pub dir: PathBuf }`: `ConfigPaths::detect() -> Option<ConfigPaths>`, `settings_file()` (`settings.toml`), `session_file()` (`session.toml`), `dictionaries_dir()` (`dictionaries/`), `personal_dictionary()` (`personal.dic`).
  - `pub fn load_settings(paths: &ConfigPaths) -> (Settings, Vec<String>)` (missing file => defaults, no warning), `pub fn save_settings(paths, &Settings) -> std::io::Result<()>` (creates dir).
  - `#[derive(Serialize, Deserialize, Default)] pub struct Session { pub files: Vec<PathBuf> }`, `load_session(paths) -> Session` (errors => empty), `save_session(paths, &Session) -> io::Result<()>`.
- Produces in `keymap.rs`:
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)] pub enum Action { NewTab, OpenFile, Save, SaveAs, CloseTab, NextTab, PrevTab, OpenSettings, Find, Replace, GoToLine, TogglePreview, Undo, Redo, SelectAll }` with `ALL: &[Action]`, `name(&self) -> &'static str` (snake_case, used in TOML), `label(&self) -> &'static str` (human text), `from_name(&str) -> Option<Action>`.
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)] pub struct KeyChord { pub command: bool, pub ctrl: bool, pub alt: bool, pub shift: bool, pub key: egui::Key }`. `command` means Cmd on macOS and Ctrl elsewhere; `ctrl` is the literal Ctrl key only distinct on macOS. `KeyChord::parse(s: &str) -> Result<KeyChord, String>` accepting `"Ctrl+Shift+S"`, `"Cmd+,"`, `"Ctrl+Tab"`, `"F3"` (`Ctrl` and `Cmd` both map to `command` on non-mac; on mac `Cmd` => command, `Ctrl` => ctrl). `Display` prints platform names (`Ctrl` or `Cmd`). `KeyChord::from_egui(key: egui::Key, mods: egui::Modifiers) -> KeyChord`. Key names via `egui::Key::from_name` / `name()`; `","` must map to `Key::Comma`.
  - `pub struct Keymap { bindings: BTreeMap<Action, KeyChord> }`: `defaults() -> Keymap`, `from_overrides(map: &BTreeMap<String, String>) -> (Keymap, Vec<String>)` (bad entries => default + warning), `to_overrides(&self) -> BTreeMap<String, String>` (only entries that differ from defaults), `chord(&self, a: Action) -> KeyChord`, `lookup(&self, c: &KeyChord) -> Option<Action>`, `set(&mut self, a: Action, c: KeyChord) -> Result<(), Action /*conflicting action*/>`, `force_set(&mut self, a, c)` (removes the chord from the other action, which becomes unbound), `reset(&mut self)`.
  - Defaults: NewTab `Cmd+N`, OpenFile `Cmd+O`, Save `Cmd+S`, SaveAs `Cmd+Shift+S`, CloseTab `Cmd+W`, NextTab `Ctrl+Tab`, PrevTab `Ctrl+Shift+Tab`, OpenSettings `Cmd+,`, Find `Cmd+F`, Replace `Cmd+H`, GoToLine `Cmd+G`, TogglePreview `Cmd+Shift+V`, Undo `Cmd+Z`, Redo `Cmd+Y`, SelectAll `Cmd+A`. (`Cmd` = `command` modifier.) An unbound action is allowed: store as `Option<KeyChord>` internally if simpler, adjust `chord()` to return `Option<KeyChord>`.

- [ ] **Step 1: Failing tests:** config: `defaults_roundtrip_through_toml`, `invalid_field_falls_back_with_warning` (`font_size = "big"` and `tab_width = 0` => defaults + 2 warnings, other valid fields kept), `unknown_key_warns`, `syntax_error_gives_defaults`, `session_roundtrip` (temp dir). keymap: `parse_and_display_roundtrip`, `parse_comma_key`, `lookup_default_save`, `set_conflict_returns_other_action`, `force_set_unbinds_other`, `overrides_only_store_differences`, `bad_override_warns_and_keeps_default`.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS, fmt, clippy.
- [ ] **Step 5:** `CHANGES.md`, commit `feat: add settings, session and keymap`.

### Task 6: Editor widget (non-vim)

**Files:** Create `src/tab.rs`, `src/view.rs`. Modify `src/app.rs`, `src/lib.rs`, `CHANGES.md`.

**Interfaces:**
- Consumes: `Buffer`, `EditorState`, `apply_command`, `EditCommand`, `EditSettings`, `text_nav`.
- Produces:
  - `pub struct Tab { pub id: u64, pub buffer: Buffer, pub editor: EditorState, pub scroll_to_cursor: bool }` with `Tab::new(buffer: Buffer) -> Tab` (unique id from an `AtomicU64`). Later tasks add fields (`vim`, `highlight`, `preview_open`, `spell`); construct only through `Tab::new`.
  - `pub struct ViewOptions { pub font: egui::FontId, pub line_numbers: bool, pub word_wrap: bool, pub edit: EditSettings, pub keys_enabled: bool }`.
  - `pub struct LineDecor { pub spans: Vec<(Range<usize> /*char range within line*/, egui::TextFormat)> }` and a trait `pub trait Decorator { fn decorate(&mut self, line_idx: usize, line: &str, base: &egui::TextFormat) -> Vec<(Range<usize>, egui::TextFormat)>; }` (default impl in view: single span with base format). Highlight and spell plug in through this.
  - `pub struct ViewOutput { pub clipboard: Option<String>, pub context_word: Option<(Range<usize>, String)> /*right-clicked word, char range in buffer*/, pub changed: bool }`.
  - `pub fn show_editor(ui: &mut egui::Ui, tab: &mut Tab, opts: &ViewOptions, deco: &mut dyn Decorator, key_handler: Option<&mut dyn FnMut(&egui::Event, &mut Tab) -> bool>) -> ViewOutput`. If `key_handler` returns `true` for an event, the widget does not process it (vim hook).
- Rendering approach (builder call recorded in spec): `egui::ScrollArea::both()` (vertical only when wrap is on) with `show_viewport`; total height = lines x row height when wrap is off; lay out only visible lines, each with `LayoutJob` from the decorator; draw selection rects, caret (blinking not required), gutter with line numbers in weak color. With wrap on, cache wrapped galley heights per line keyed by (revision, width). Focus: the widget requests focus on click and when created; handles `Event::Text`, `Event::Key`, `Event::Copy/Cut/Paste`, mouse click/drag/double/triple click, mouse wheel via ScrollArea. Translate to `EditCommand`. Typing consecutive chars goes into one undo group until a non-text command (use `begin_group` on first char, `end_group` on the next non-text command).
- App changes: `FedApp` holds `tabs: Vec<Tab>`, `active: usize`; opens one tab per CLI path (errors shown in a bottom error label), or one empty tab. Show only the active tab with `show_editor`. Clipboard writes via `ctx.copy_text`.

- [ ] **Step 1:** Implement `tab.rs`, `view.rs`, wire into `app.rs`.
- [ ] **Step 2:** Manual check: `cargo run -- Cargo.toml src/lib.rs` opens first file; typing, selection with mouse and Shift+arrows, Ctrl+arrows, Home/End, PageUp/Down, double/triple click, copy/cut/paste, Backspace/Delete, Tab/Shift+Tab, scrolling all work; a 20 000 line file scrolls without lag (`seq 1 20000 > /tmp/... ` use scratch file).
- [ ] **Step 3:** fmt, clippy, test green. `CHANGES.md`, commit `feat: add custom editor widget`.

### Task 7: App shell (tabs, files, dialogs, shortcuts, status bar, session)

**Files:** Create `src/app/` split if `app.rs` exceeds ~400 lines: `src/app.rs` + `src/app/dialogs.rs`. Modify `src/lib.rs`, `CHANGES.md`.

**Interfaces:**
- Consumes: `Tab`, `show_editor`, `Keymap`, `Action`, `KeyChord::from_egui`, `config::*`.
- Produces:
  - Pure helper `pub fn close_requires_confirmation(tab: &Tab) -> bool` and `pub fn tabs_needing_confirmation(tabs: &[Tab]) -> Vec<usize>` in `app.rs` (tested).
  - `FedApp::dispatch(&mut self, ctx: &egui::Context, action: Action)` executing every `Action`.
  - `FedApp::open_path(&mut self, path: PathBuf)` (if already open, activate that tab; if the active tab is an empty unmodified untitled tab, replace it).
  - `pub fn status_text(tab: &Tab) -> String` like `"src/lib.rs  12:5  LF"` (1-based line:col), plus `"  [+]"` when modified.
- Behavior:
  - Tab bar at top (`TopBottomPanel::top`): selectable labels `name` with ` ●` when modified and a small `x` button; middle-click closes.
  - Shortcuts: consume matching chords with `ctx.input_mut(|i| i.consume_key(mods, key))` before the editor runs, iterating `Action::ALL`. `Undo/Redo/SelectAll` map to `EditCommand`s on the active tab.
  - Open uses `rfd::FileDialog::new().pick_files()` (blocking is acceptable for native dialog). Save on an untitled tab => Save As dialog.
  - Modal confirmation (`egui::Modal`) for closing a modified tab: "Save", "Discard", "Cancel". Window close: intercept `ctx.input(|i| i.viewport().close_requested())`, send `ViewportCommand::CancelClose` if any tab is modified and show a modal listing modified files with "Save all", "Discard all", "Cancel".
  - Closing the last tab exits the app (`ViewportCommand::Close`).
  - Status bar (`TopBottomPanel::bottom`): `status_text`; right side shows transient messages (errors in `ui.visuals().error_fg_color`, cleared after 5 s) and `"large file: highlighting off"` when `is_large`.
  - Settings loaded at startup via `load_settings` (warnings shown as a status message), keymap from overrides. Settings saved whenever changed (Task 13 provides UI).
  - Session: on exit, if `restore_session`, `save_session` with all tab paths; on startup with no CLI files and `restore_session`, open those files (silently skip missing).
  - Apply `ui_theme`: `ctx.set_theme(ThemePreference::{System, Light, Dark})`.

- [ ] **Step 1: Failing tests:** `close_requires_confirmation_when_modified` (modified tab => true, saved => false), `tabs_needing_confirmation_lists_modified_only`, `status_text_format` (`"untitled  1:1  LF"`, CRLF variant, `[+]`).
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS.
- [ ] **Step 5:** Manual check: new/open/save/save as/close with and without changes, Ctrl+Tab cycling, close window with unsaved changes asks, session restore toggled by editing `settings.toml` by hand.
- [ ] **Step 6:** fmt, clippy. `CHANGES.md`, commit `feat: add tabs, file dialogs, shortcuts and status bar`.

### Task 8: Find/replace and go-to-line bars

**Files:** Create `src/find_bar.rs`. Modify `src/app.rs`, `src/view.rs` (highlight matches through decorator composition), `CHANGES.md`.

**Interfaces:**
- Consumes: `SearchQuery`, `find_next`, `find_all`, `replace_all`, `Buffer::replace`, `Action::{Find, Replace, GoToLine}`.
- Produces:
  - `pub enum BarMode { Hidden, Find, Replace, GoToLine }`, `pub struct FindBar { pub mode: BarMode, pub query: SearchQuery, pub replacement: String, pub goto: String, pub error: Option<String>, ... }`.
  - `FindBar::show(&mut self, ui: &mut egui::Ui, tab: &mut Tab) -> Option<String> /*status message*/`.
  - `pub fn parse_goto(input: &str, line_count: usize) -> Result<usize, String>` (1-based input, returns 0-based clamped line; non-number => error).
  - `FindBar::matches(&self, tab: &Tab) -> Vec<Range<usize>>` cached by (buffer revision, query) for highlighting matches with a translucent background.
- Behavior: bar is a panel above the status bar. Opening Find pre-fills with the current selection if single-line. Typing searches incrementally from the caret, selecting the match and scrolling to it. `Enter`/`F3` next, `Shift+Enter`/`Shift+F3` previous, `Esc` closes and returns focus to the editor. Shows `"3 of 12"` or `"no matches"`; invalid regex shows the error in error color. Checkboxes `Aa` (case) and `.*` (regex). Replace: "Replace" replaces current match if selected then finds next; "Replace all" uses `replace_all` (single undo step) and reports the count. Go to line: input + Enter moves the caret to first non-blank of that line.

- [ ] **Step 1: Failing tests:** `parse_goto_valid`, `parse_goto_clamps`, `parse_goto_rejects_text`.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS. **Step 5:** manual check of find/replace/goto incl. invalid regex and accented text.
- [ ] **Step 6:** fmt, clippy. `CHANGES.md`, commit `feat: add find/replace and go-to-line bars`.

### Task 9: Syntax highlighting

**Files:** Create `src/highlight.rs`. Modify `src/tab.rs`, `src/app.rs`, `CHANGES.md`.

**Interfaces:**
- Consumes: `Decorator`, `Buffer::{take_dirty_line, is_large, path}`, `Settings::{code_theme_light, code_theme_dark}`.
- Produces:
  - `pub struct HighlightAssets { pub syntaxes: SyntaxSet, pub themes: ThemeSet }`; `pub struct AssetsHandle` with `AssetsHandle::spawn(ctx: egui::Context) -> AssetsHandle` (loads `SyntaxSet::load_defaults_newlines()` and `ThemeSet::load_defaults()` in a `std::thread`, then `ctx.request_repaint()`), `get(&self) -> Option<Arc<HighlightAssets>>` (non-blocking, via `OnceLock` or `Arc<Mutex<Option<..>>>`).
  - `HighlightAssets::theme_names(&self) -> Vec<String>`.
  - `#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum SpanKind { Comment, String, Other }`, `pub fn span_kind(stack: &syntect::parsing::ScopeStack) -> SpanKind` (any scope starting with `comment` => Comment, `string` => String; Comment wins).
  - `#[derive(Clone, Debug)] pub struct StyledSpan { pub range: Range<usize> /*char range in line*/, pub color: egui::Color32, pub italic: bool, pub bold: bool, pub underline: bool, pub kind: SpanKind }`.
  - `pub struct HighlightCache` with `HighlightCache::new() -> Self`, `invalidate_from(&mut self, line: usize)`, `reset(&mut self)` (theme or syntax change), `line_spans(&mut self, assets: &HighlightAssets, syntax_name_or_ext: &str, theme: &str, rope: &Rope, line: usize) -> &[StyledSpan]` (parses forward from the last valid cached line state up to `line`; stores `(ParseState, HighlightState)` per line; parsing via `ParseState::parse_line` + `HighlightIterator` on `rope.line(i)` text incl. `\n`).
  - `pub fn syntax_for_path(assets: &HighlightAssets, path: Option<&Path>) -> String` (syntax name; extension lookup, fallback `"Plain Text"`).
  - `Tab` gains `pub highlight: HighlightCache`, `pub syntax: Option<String>`.
- Integration: a `HighlightDecorator<'a>` implementing `Decorator` built per frame in app; when assets are not ready, or `is_large`, plain text. App calls `tab.highlight.invalidate_from(line)` with `take_dirty_line()` each frame before drawing (Task 10 will also need the dirty line: app takes it once and forwards it to both). Editor background/foreground colors come from the theme (`settings.background`, `settings.foreground`) when available. Theme chosen by current dark/light visuals.

- [ ] **Step 1: Failing tests** (load assets synchronously in tests): `span_kind_detects_comment_and_string` (Rust line `let s = "hi"; // note` has String and Comment spans covering `"hi"` and `// note`), `cache_invalidation_reparses_from_edited_line` (Rust `/*` opened on line 0 makes line 1 a comment; editing line 0 to remove it and invalidating => line 1 no longer Comment), `syntax_for_path_extension` (`.rs` => `"Rust"`, `.md` => `"Markdown"`, unknown => `"Plain Text"`), `spans_cover_multibyte_line`.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS. **Step 5:** manual check: Rust, Python, Markdown, JSON files colored; typing a `/*` recolors following lines; theme switch by editing settings; startup still instant (window appears before colors).
- [ ] **Step 6:** fmt, clippy. `CHANGES.md`, commit `feat: add incremental syntect highlighting`.

### Task 10: Spell check

**Files:** Create `src/spell/mod.rs`, `src/spell/words.rs`, `src/spell/dict.rs`. Modify `src/app.rs`, `src/view.rs` (wavy underline + context menu), `src/lib.rs`, `CHANGES.md`.

**Interfaces:**
- Consumes: `StyledSpan`, `SpanKind`, `HighlightCache::line_spans`, `ConfigPaths::{dictionaries_dir, personal_dictionary}`, `ViewOutput::context_word`, `Decorator`.
- Produces in `words.rs`:
  - `pub fn words(text: &str) -> Vec<Range<usize>>` (char ranges of alphabetic runs, apostrophes split words: `"l'albero"` => `"l"`, `"albero"`; skips runs shorter than 2 chars, runs containing digits or `_`, ALL-CAPS runs of length <= 5, camelCase words containing an inner uppercase).
  - `pub fn code_checkable(spans: &[StyledSpan]) -> Vec<Range<usize>>` (ranges of Comment/String spans merged).
  - `pub fn markdown_checkable(text: &str) -> Vec<Range<usize>>` (char ranges over the whole doc of `Event::Text` outside code blocks, excluding inline code, HTML, link URLs and autolinks; use `pulldown_cmark::Parser::new_ext(text, Options::all()).into_offset_iter()`; convert byte to char offsets).
- Produces in `dict.rs`:
  - `pub const LANGS: [&str; 2] = ["it_IT", "en_US"]`, `pub const DICT_BASE_URL: &str = "https://raw.githubusercontent.com/LibreOffice/dictionaries/master"`, `fn remote_path(lang) -> &'static str` (`it_IT/it_IT`, `en/en_US`).
  - `pub fn locate(lang: &str, dict_dir: &Path, system_dirs: &[PathBuf]) -> Option<(PathBuf, PathBuf)>` (aff, dic; config dir first, then system dirs; `system_dirs()` returns `/usr/share/hunspell`, `/usr/share/myspell`, `/usr/share/myspell/dicts` on Linux and empty elsewhere).
  - `pub fn download(lang: &str, dict_dir: &Path) -> Result<(PathBuf, PathBuf), String>` (ureq GET both files, write via temp + rename).
- Produces in `mod.rs`:
  - `pub struct SpellChecker` with `SpellChecker::new(dicts: Vec<spellbook::Dictionary>, personal: HashSet<String>) -> Self`, `check(&self, word: &str) -> bool` (any dict or personal; also tries lowercase of a capitalized word), `suggest(&self, word: &str) -> Vec<String>` (merged, max 8, dedup).
  - `pub fn load_personal(path: &Path) -> HashSet<String>`, `pub fn append_personal(path: &Path, word: &str) -> std::io::Result<()>`.
  - `pub enum SpellStatus { Disabled, Loading, Downloading, Ready, Failed(String) }`; `pub struct SpellService` with `SpellService::spawn(ctx, paths: ConfigPaths, allow_download: bool) -> SpellService` (thread: locate or download each lang, parse with `spellbook::Dictionary::new(aff, dic)`, load personal, request repaint), `status(&self) -> SpellStatus`, `checker(&self) -> Option<Arc<RwLock<SpellChecker>>>`, `add_word(&self, word: &str)` (personal set + file), `retry(&mut self)`.
  - Word result cache `HashMap<String, bool>` inside `SpellChecker` behind the lock, cleared on `add_word`.
- Integration: when `settings.spell_check` is on, a `SpellDecorator` wraps the highlight decorator: for code tabs it checks words inside `code_checkable` ranges of each visible line; for Markdown tabs (`syntax == "Markdown"`) it uses `markdown_checkable` computed once per buffer revision (cached on the tab as `pub spell_md: Option<(u64, Vec<Range<usize>>)>`); for Plain Text all words. Misspelled ranges get `underline: Stroke(1.0, red)` (egui `TextFormat::underline`; wavy is optional, straight is acceptable). Right-click on a misspelled word (view returns `context_word`) opens a context menu with suggestions (click replaces the word as one undo step) and "Add to dictionary". Status bar shows `SpellStatus` while Downloading/Failed. Disabled for `is_large` buffers.

- [ ] **Step 1: Failing tests:** `words_split_apostrophe`, `words_skip_identifiers_and_numbers`, `words_multibyte_ranges` (`"perché così"` => `[0..6, 7..11]`), `code_checkable_merges_comment_and_string_spans`, `markdown_excludes_code` (text with fenced code, inline code and a link => URL and code words not in ranges, link text included), `locate_prefers_config_dir` (temp dirs with fake files), `personal_roundtrip`, `checker_uses_personal_and_capitalization` (build a tiny `spellbook::Dictionary` from inline aff `"SET UTF-8\n"` and dic `"2\nciao\nhello\n"`; `check("Ciao")` true, `check("xyz")` false, after personal insert `check("xyz")` true).
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS. **Step 5:** manual check: fresh config dir (`XDG_CONFIG_HOME=$(mktemp -d)`) downloads dictionaries in background without blocking the window; misspelled words in comments/Markdown underlined; identifiers not; suggestions and add-to-dictionary work.
- [ ] **Step 6:** fmt, clippy. `CHANGES.md`, commit `feat: add Italian and English spell check`.

### Task 11: Markdown preview

**Files:** Create `src/markdown.rs`. Modify `src/tab.rs`, `src/app.rs`, `CHANGES.md`.

**Interfaces:**
- Consumes: `Action::TogglePreview`, `Tab::syntax`, `Buffer::{rope, path, revision}`.
- Produces:
  - `pub fn is_markdown_path(path: Option<&Path>) -> bool` (`md`, `markdown`, `mdown`, case-insensitive).
  - `pub fn rewrite_image_paths(text: &str, base_dir: &Path) -> String` (relative image URLs `![a](img/x.png)` become `file:///abs/path/img/x.png`; `http(s)://` images are replaced by their alt text in brackets so they never load; absolute `file://` left as is). Uses pulldown-cmark offsets to find image destinations only.
  - `pub struct Preview { cache: egui_commonmark::CommonMarkCache, source: (u64 /*revision*/, String) }` with `Preview::show(&mut self, ui: &mut egui::Ui, buf: &Buffer)` (re-render text only when revision changes).
  - `Tab` gains `pub preview_open: bool`, `pub preview: Option<Preview>`.
- Behavior: `TogglePreview` on a Markdown tab toggles a `SidePanel::right` (resizable, default half the width) with a `ScrollArea` and the rendered preview; on non-Markdown tabs it shows a status message "preview is only available for Markdown". `egui_extras::install_image_loaders(&ctx)` once at startup. Link clicks use egui's default `OpenUrl` handling (opens system browser on native). A "Preview" toggle button also appears in the tab bar area for Markdown tabs.

- [ ] **Step 1: Failing tests:** `is_markdown_path_cases`, `relative_image_becomes_file_url`, `remote_image_replaced_by_alt`, `non_image_links_untouched`.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS. **Step 5:** manual check with a Markdown file containing tables, task list, footnote, code block, relative image, remote image, link.
- [ ] **Step 6:** fmt, clippy. `CHANGES.md`, commit `feat: add live Markdown preview`.

### Task 12: Vim core (normal, insert, visual, motions, operators, marks)

**Files:** Create `src/vim/mod.rs`, `src/vim/motion.rs`, `src/vim/textobj.rs`. Modify `src/lib.rs`, `CHANGES.md`.

**Interfaces:**
- Consumes: `Buffer`, `EditorState`, `Selection`, `text_nav::*`, `search::{SearchQuery, find_next}`.
- Produces:
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Mode { Normal, Insert, Visual, VisualLine, CommandLine }`.
  - `#[derive(Clone, Debug, PartialEq, Eq)] pub enum VimKey { Char(char), Esc, Enter, Backspace, Tab, Ctrl(char), Left, Right, Up, Down }`.
  - `#[derive(Clone, Debug, PartialEq)] pub enum VimEffect { SetClipboard(String), Message(String), Error(String), Write(Option<PathBuf>), Quit { force: bool }, WriteQuit, Edit(PathBuf), GotoLine(usize) }` (`GotoLine` unused by core, reserved for cmdline).
  - `pub struct VimState` with `pub fn new() -> Self`, `pub fn mode(&self) -> Mode`, `pub fn cmdline(&self) -> &str` (text after `:`/`/`/`?`, prefix included, e.g. `":1,$s/a/b/"`), `pub fn pending(&self) -> &str` (pending normal keys for status bar, e.g. `"2d"`), `pub fn handle_key(&mut self, key: VimKey, buf: &mut Buffer, ed: &mut EditorState) -> Vec<VimEffect>`.
  - Cursor model: in Normal mode `ed.selection` is a caret on a char (never past the last char of a non-empty line, never on `\n` unless the line is empty). Visual mode: `selection.anchor` and `head` both inclusive in vim terms; the view renders selection as `min..max+1` (VisualLine: whole lines). Expose `pub fn visual_range(&self, buf: &Buffer, ed: &EditorState) -> Option<Range<usize>>` for rendering.
  - `motion.rs`: `pub enum Motion { Left, Right, Up, Down, WordStart{big: bool}, WordBack{big}, WordEnd{big}, LineStart, FirstNonBlank, LineEnd, DocStart, GotoLine(usize), DocEnd, FindChar{c: char, forward: bool, till: bool}, RepeatFind{reverse: bool}, MatchBracket, ParagraphNext, ParagraphPrev, MarkLine(char), MarkExact(char), SearchNext{reverse: bool} }`, `pub struct MotionResult { pub pos: usize, pub linewise: bool, pub inclusive: bool }`, `pub fn resolve(m: &Motion, count: usize, buf: &Buffer, pos: usize, state: &MotionContext) -> Option<MotionResult>` where `MotionContext` holds last find char and last search.
  - `textobj.rs`: `pub fn text_object(kind: char /*w ( { [ " '*/, inner: bool, buf: &Buffer, pos: usize) -> Option<Range<usize>>` (`b` = `(`, `B` = `{`, `)` = `(`, `}` = `{`, `]` = `[`).
  - Unnamed register: `struct Register { text: String, linewise: bool }`; every yank/delete/change writes it and emits `SetClipboard(text)`.
- Supported keys (spec 5.5): motions `h j k l w b e W B E 0 ^ $ gg G {n}G f F t T ; , % { }`, counts; operators `d c y` + motion or text object, `dd cc yy`, `x X D C J r{c} >> <<` (indent by tab_width 4 spaces default; pass `EditSettings` if simpler: add param `settings: &EditSettings` to `handle_key`); insert `i a I A o O s S`; `p P`; `u` / `Ctrl('r')`; `.` repeats last change (record the key sequence from the change start until return to Normal); `/ ? n N * #`; marks `m{a-z}`, `'{a-z}`, `` `{a-z} `` (via `Buffer::set_mark/mark`; `'`/`` ` `` also usable as motions for operators); `v V` toggle, in Visual: motions, `d x c y > < o` (swap ends), `:` enters CommandLine with `'<,'>` prefilled, `Esc`. Insert mode: text chars, `Enter` (auto-indent like `EditCommand::Newline`), `Backspace`, `Tab`, arrows, `Esc` (cursor moves left one like vim). Insert session is one undo group. `:` from Normal enters CommandLine; in CommandLine `Enter` calls `cmdline::execute` (Task 13; until then, a stub that returns `Error("not an editor command")`), `Esc` cancels, `Backspace` on empty returns to Normal. `/` and `?` use CommandLine with the search prefix; `Enter` runs regex search with `SearchQuery { regex: true, case_sensitive: true }` and stores it.
  - Unknown sequences reset pending state silently.

- [ ] **Step 1: Failing tests** (helper `fn run(text: &str, cursor: usize, keys: &str) -> (String, usize, Mode)` that feeds `VimKey::Char` per char, with `<Esc>`, `<CR>`, `<C-r>` escapes parsed): `motions_basic` (`w`, `b`, `e`, `$`, `0`, `^`, `gg`, `G`, `3G`), `counts_on_motions` (`3w`), `dw_deletes_word`, `cw_changes_to_word_end` (vim special case), `ciw_and_di_quote`, `dd_with_count_and_p_linewise`, `yy_P_pastes_above`, `x_and_dot_repeat`, `undo_redo_ctrl_r`, `visual_line_delete`, `visual_char_yank_emits_clipboard`, `find_char_and_semicolon`, `percent_matches_bracket`, `marks_jump_line_and_exact` (`ma`, move, `'a` goes to first non-blank of line, `` `a `` to exact pos), `marks_follow_edits`, `search_forward_and_n`, `insert_esc_moves_left`, `o_opens_line_with_indent`, `r_replaces_char`, `J_joins_lines`, `indent_shift_right`, `accented_text_motions` (`"è già qui"`, `w` twice => on `q`).
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS, fmt, clippy.
- [ ] **Step 5:** `CHANGES.md`, commit `feat: add vim core state machine`.

### Task 13: Vim command line and UI integration

**Files:** Create `src/vim/cmdline.rs`. Modify `src/vim/mod.rs`, `src/tab.rs`, `src/view.rs`, `src/app.rs`, `CHANGES.md`.

**Interfaces:**
- Consumes: `VimState`, `VimEffect`, `search::{substitute, SearchQuery}`, `Buffer`, app tab operations from Task 7 (`open_path`, close tab with confirmation, save).
- Produces in `cmdline.rs`:
  - `#[derive(Debug, PartialEq, Clone, Copy)] pub enum Address { Line(usize) /*1-based*/, Current, Last, Mark(char) }`, `pub struct LineRange { pub start: Address, pub end: Address }`.
  - `#[derive(Debug, PartialEq)] pub enum ExCommand { Write(Option<PathBuf>), Quit { force: bool }, WriteQuit, Edit(PathBuf), Goto(Address), Substitute { range: Option<LineRange>, pattern: String, replacement: String, global: bool, ignore_case: bool } }`.
  - `pub fn parse(input: &str) -> Result<ExCommand, String>` (input without leading `:`; supports `w`, `w file`, `q`, `q!`, `wq`, `x`, `e file`, `{n}`, `$`, `s/p/r/[gi]`, `%s/...`, `{a},{b}s/...` with addresses `n`, `.`, `$`, `'<`, `'>`, `'a`; any delimiter char after `s`; `\/` escapes the delimiter; empty replacement allowed; missing trailing delimiter allowed).
  - `pub fn resolve_range(range: Option<LineRange>, buf: &Buffer, cursor_line: usize, visual_marks: Option<(usize, usize)>) -> Result<std::ops::Range<usize>, String>` (0-based end-exclusive; `None` => current line; `%` => all; reversed ranges error `"backwards range"`).
  - `pub fn execute(cmd: ExCommand, buf: &mut Buffer, ed: &mut EditorState, visual_marks: Option<(usize, usize)>) -> Vec<VimEffect>` (Substitute runs `substitute`, reports `"N substitutions"` or `Error("pattern not found: p")` / invalid regex error; Goto moves caret to first non-blank; file commands become effects).
  - VimState stores last visual selection lines for `'<`/`'>`.
- UI integration:
  - `Tab` gains `pub vim: VimState`.
  - `pub fn egui_event_to_vim_key(ev: &egui::Event) -> Option<VimKey>` in `view.rs` (Text => one `Char` per char; Key Escape/Enter/Backspace/Tab/arrows; Ctrl+letter => `Ctrl`). When `settings.vim_mode`, app passes a `key_handler` to `show_editor` that routes events to `vim.handle_key` and returns `true`, except in Insert mode for keys that the handler maps identically (simplest: route everything when vim is on). App shortcuts (Task 7) keep priority because they are consumed before the editor runs; but in vim mode, chords without `command`/`ctrl` modifiers are never treated as app shortcuts (precedence rule from spec 5.5), and `Cmd+R`/`Ctrl+R` is not consumed by Redo when vim is on (vim owns `Ctrl+R`).
  - Effects: `SetClipboard` => `ctx.copy_text`; `Write(None)` => save (untitled => Save As dialog); `Write(Some(p))` => `write_copy`; `Quit{force:false}` => refuse with `Error("No write since last change (add ! to override)")` if modified, else close tab; `Quit{force:true}` => close discarding; `WriteQuit` => save then close; `Edit(p)` => `open_path(p)` (relative to the current file's dir, else cwd); `Message`/`Error` => command bar text.
  - Command bar: when vim is on, a dedicated `TopBottomPanel::bottom` above the status bar, height for one line, monospace font at `font_size + 2`, strong text color on `ui.visuals().extreme_bg_color`, showing `cmdline()` with a block cursor while in CommandLine mode, otherwise the last message/error (error color) until the next key. Status bar shows the mode: `-- NORMAL --`, `-- INSERT --`, `-- VISUAL --`, `-- VISUAL LINE --`, plus `pending()`.
  - Caret: block caret in Normal/Visual, thin caret in Insert.

- [ ] **Step 1: Failing tests:** `parse_write_quit_variants`, `parse_substitute_full_range` (`"1,$s/aaa/bbb/"` => range Line(1)..Last, pattern "aaa", replacement "bbb", not global), `parse_substitute_percent_global_ignorecase`, `parse_substitute_custom_delimiter_and_escape` (`"s#a/b#c#g"`, `"s/a\\/b/c/"`), `parse_visual_range`, `parse_goto_line`, `parse_rejects_unknown` (`"foo"` => Err containing `"not an editor command"`), `resolve_backwards_range_errors`, `substitute_invalid_regex_reports_error`, `execute_substitute_reports_count`, `quit_effect_on_modified_buffer` (execute `q` returns `Quit{force:false}`; refusal logic is app-level and tested via `close_requires_confirmation`).
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS.
- [ ] **Step 5:** Manual check with `vim_mode = true` in settings: modes, command bar visibility, `:1,$s/aaa/bbb/`, `:%s/x/y/g`, `:w`, `:q` refusal, `:q!`, `:wq`, `:e other.txt`, `ma`/`'a`, Ctrl+S still saves, typing `n` in insert mode inserts `n`.
- [ ] **Step 6:** fmt, clippy. `CHANGES.md`, commit `feat: add vim command line and UI integration`.

### Task 14: Settings panel, fonts and key capture

**Files:** Create `src/fonts.rs`, `src/settings_ui.rs`. Modify `src/app.rs`, `CHANGES.md`.

**Interfaces:**
- Consumes: `Settings`, `save_settings`, `Keymap::{set, force_set, reset, to_overrides, chord}`, `Action::{ALL, label}`, `KeyChord::from_egui`, `AssetsHandle::get` + `theme_names`, `SpellService::{status, retry}`, `Action::OpenSettings`.
- Produces in `fonts.rs`:
  - `pub struct FontCatalog` with `FontCatalog::spawn(ctx) -> FontCatalog` (thread: `fontdb::Database::load_system_fonts`, collect family names sorted and dedup, flag monospace), `families(&self) -> Option<Vec<(String, bool /*monospace*/)>>`, `load_family_bytes(&self, family: &str) -> Option<Vec<u8>>` (regular weight face data).
  - `pub fn apply_editor_font(ctx: &egui::Context, family_name: Option<&str>, bytes: Option<Vec<u8>>)`: registers the font as `FontFamily::Name("editor")` with egui's default monospace as fallback; `None` => `"editor"` aliases the default monospace (bundled Hack). The editor view always uses `FontId::new(size, FontFamily::Name("editor".into()))`.
  - Font catalog spawn happens lazily the first time the settings window opens; at startup, if `settings.font_family` is set, a background thread loads just that family's bytes via fontdb and applies it when ready (default monospace used until then).
- Produces in `settings_ui.rs`:
  - `pub struct SettingsWindow { pub open: bool, capture: Option<Action>, pending_conflict: Option<(Action, KeyChord, Action)> }`.
  - `pub enum SettingsEvent { Changed, FontChanged, KeymapChanged, RetryDictionaries }`.
  - `SettingsWindow::show(&mut self, ctx: &egui::Context, settings: &mut Settings, keymap: &mut Keymap, fonts: &FontCatalog, theme_names: &[String], spell: &SpellStatus) -> Vec<SettingsEvent>`.
  - `pub fn capture_chord(events: &[egui::Event]) -> Option<CaptureResult>` with `pub enum CaptureResult { Chord(KeyChord), Cancel }`: first `Event::Key { pressed: true, .. }` that is not a bare modifier; `Escape` without modifiers => `Cancel`. Pure, tested.
- Behavior: `egui::Window` "Settings" (collapsible false, resizable, default 520x600) with sections: Editor (font family combo with search filter and "monospace only" checkbox default on, font size `DragValue` 6..72, tab width 1..16, insert spaces, line numbers, word wrap), Appearance (UI theme radio System/Light/Dark, code theme light/dark combos from `theme_names`, disabled until assets loaded), Vim (enable), Spell check (enable, status line, "Download dictionaries" button when Failed or missing), Session (restore), Key bindings (grid: action label, chord text, "Change" button; while capturing, the button shows "Press keys... (Esc to cancel)" and the app does not dispatch shortcuts; captured chord conflicting with another action shows inline "Already used by X" with "Replace" and "Cancel"; "Reset to defaults" button). Every change emits `Changed` so the app saves immediately with `save_settings` and applies (font, theme, vim on/off: switching vim off resets each tab's vim state to Normal and caret selection).
  - While capture is active, app must read raw events (`ctx.input(|i| i.events.clone())`) before shortcut dispatch and skip dispatch for that frame.

- [ ] **Step 1: Failing tests:** `capture_ignores_bare_modifiers`, `capture_escape_cancels`, `capture_returns_chord_with_modifiers` (Ctrl+Shift+K => command+shift+K on non-mac).
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS.
- [ ] **Step 5:** Manual check: `Ctrl+,` opens settings; change font family and size live; switch themes; enable vim; rebind New Tab to `Ctrl+T` by live capture; conflict prompt when choosing `Ctrl+S`; restart keeps everything; delete `settings.toml` field types by hand to see warnings.
- [ ] **Step 6:** fmt, clippy. `CHANGES.md`, commit `feat: add settings panel with fonts and key capture`.

### Task 15: Performance pass, docs and release build

**Files:** Create `README.md`. Modify `CHANGES.md`, any file needing perf fixes.

- [ ] **Step 1:** Measure startup: add `FED_TRACE_STARTUP=1` env check in `run` printing elapsed ms from process start to the end of the first `update` (use `std::time::Instant` captured in `main`). Run release build 5 times with a small file; expected: under 200 ms to first frame on this machine. If above, profile and move work off the first frame.
- [ ] **Step 2:** Typing lag check on a 5 000 line Rust file with highlighting and spell check on: no visible lag. Check memory with 5 tabs open (`/proc/<pid>/status` VmRSS) and record the number in `README.md`.
- [ ] **Step 3:** `README.md`: what fed is, build (`cargo build --release`), usage (`fed [files...]`), default shortcuts table, vim support list incl. note that `:s` patterns use Rust regex syntax and `$1` replacements, settings file location per OS, dictionaries download behavior and manual placement, large file behavior.
- [ ] **Step 4:** `cargo build --release` (Linux). `cargo check --target x86_64-pc-windows-gnu` only if the target is installed; otherwise note in `CHANGES.md` that Windows/macOS builds are pending verification on those systems.
- [ ] **Step 5:** fmt, clippy, test green. `CHANGES.md`, commit `docs: add README and startup tracing`.
