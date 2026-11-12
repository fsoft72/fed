# Optimization TODO

> Generated on 2026-11-12. Items sorted by importance.

## Critical

- [x] **Markdown spell ranges rebuilt from the whole document on every keystroke** - `SpellDecorator::prepare` calls `words::markdown_checkable(&buffer.text())` whenever the revision changes, so each keystroke copies the whole rope into a `String` and re-parses it with pulldown-cmark on the UI thread; recompute only from the first dirty line (or debounce / move it to the spell service thread).
  - File(s): `src/spell/mod.rs`, `src/spell/words.rs`
- [x] **Markdown preview copies and re-parses the whole document** - `Preview::show` does `buf.rope().to_string()` + `rewrite_image_paths` on every revision and hands the full source to `CommonMarkViewer::show` every frame (which parses it again); debounce the source refresh while typing and avoid rendering the whole document when only the visible part is needed.
  - File(s): `src/markdown.rs`
- [x] **Bare-key shortcuts accepted from `settings.toml`** - `chord_has_modifier` is only enforced by the settings UI capture, not by `Keymap::from_overrides`, so `save = "K"` in the config file gets dispatched by `shortcut_action` on every keystroke and swallows typing; move the rule into `keymap.rs` and apply it at load time with a warning.
  - File(s): `src/keymap.rs`, `src/settings_ui.rs`

## High

- [x] **Vim `n`/`N`/`*`/`#` scan the whole document per press** - `Motion::SearchNext` recompiles the regex on each press and calls `search::find_next` `count` times, and `find_next` runs `find_all` (one `String` per line, every match collected) just to return one hit; make `find_next` stop at the first match from `from` (wrapping once) and compile the regex once in `last_search`.
  - File(s): `src/vim/motion.rs`, `src/search.rs`, `src/vim/mod.rs`
- [x] **Word wrap recomputes every line top on every keystroke** - `WrapCache::update` walks the whole rope and allocates a `String` per line on each revision change, `Frame::run` clones `wrap.tops` (one `f32` per line) every frame, and `heights` grows without bound; update tops incrementally from the buffer's dirty lines, borrow `tops` instead of cloning, and prune the height map.
  - File(s): `src/view.rs`
- [x] **Longest-line scan is O(document) per keystroke without wrap** - `Frame::max_line_cols` rescans every line whenever the revision changes; update it incrementally from the dirty line range (a full rescan is only needed when the longest line shrinks).
  - File(s): `src/view.rs`
- [x] **Spell checking re-runs for every visible line every frame** - `SpellDecorator::decorate` re-tokenizes each visible line, takes `checker.write()` and allocates a `String` per word on every frame, with no per-line result cache, and `SpellChecker::cache` grows without limit; cache the misspelled ranges per `(line, revision)` and bound the word cache.
  - File(s): `src/spell/mod.rs`, `src/spell/words.rs`
- [ ] **Syntax highlighting can stall a frame on big files** - `HighlightCache::line_spans` parses every line from the last cached one up to the requested one inside one frame (jumping to the end of a ~5 MB file parses all of it), and keeps a cloned `ParseState` + `HighlightState` for every line; add a per-frame line budget with `request_repaint` and checkpoint states every N lines instead of every line.
  - File(s): `src/highlight.rs`
- [ ] **`FedApp` is a god object** - `src/app.rs` is ~1900 lines and `FedApp` holds ~27 fields covering tabs, fonts, spell, settings persistence, vim messages, links and dialogs; extract cohesive pieces (e.g. a `FontManager` owning the two `FontLoad`/`LoadedFont` pairs, a `StatusBar` owning status + link errors, vim effect handling into `app/vim.rs`) like `app/dialogs.rs` and `app/menu.rs` already do.
  - File(s): `src/app.rs`
- [ ] **Editor rendering juggles tab internals inside `ui`** - the `CentralPanel` closure in `FedApp::ui` (~95 lines) moves `syntax`, `highlight`, `spell_md` and `spell_marks` out of the `Tab`, builds the decorator stack in two duplicated branches and puts them back by hand (`tab.syntax = assets.map(|_| syntax)` silently drops the syntax when assets are missing); move this into a `Tab::show_editor` method that builds the decorator chain once and keep those `Tab` fields private.
  - File(s): `src/app.rs`, `src/tab.rs`
- [ ] **Files are loaded on the UI thread with no size cap** - `Buffer::load` runs synchronously from `FedApp::try_open`, holds the file up to three times in memory (bytes, normalized copy, rope) and `LARGE_FILE_BYTES` only disables features; refuse or confirm above a hard limit and load big files on a background thread.
  - File(s): `src/buffer.rs`, `src/app.rs`

## Medium

- [ ] **Font work duplicated and partly on the UI thread** - startup runs up to three full `fontdb` system scans (`FontLoad::spawn` for editor and preview, then `FontCatalog::spawn`), and `FontCatalog::load_family_font` copies the whole font file on the UI thread from `FedApp::load_font`; share one scanned database and load the chosen face in the background. Also, if the catalog is still scanning, `load_font` reports a false "font not found".
  - File(s): `src/fonts.rs`, `src/app.rs`
- [ ] **Find bar recompiles regexes every frame** - `FindBar::matches` (called every frame from `FedApp::ui`) only caches on successful compilation, so an invalid pattern is recompiled each frame; `step` and `replace_current` also compile twice on a cache miss. Cache the compile result (including the error) keyed by the query.
  - File(s): `src/find_bar.rs`
- [ ] **Visual `>`/`<` shifts one line too many** - `apply_operator` maps `OpRange::Chars(r)` to lines with `char_to_line(r.end)` on an exclusive end, so a selection ending on `\n` (`v$>`, `v>` on an empty line) also indents the next line.
  - File(s): `src/vim/mod.rs`
- [ ] **Visual-mode changes are not repeatable with `.`** - `visual_operator` never sets `last_change` / `recording`, so `.` after `vjd` replays an older Normal-mode change; add tests for it.
  - File(s): `src/vim/mod.rs`, `src/vim/tests.rs`
- [ ] **`:'<,'>` uses stale line numbers** - `leave_visual` stores both buffer marks `<`/`>` (which follow edits) and `last_visual` line numbers (which do not), and only `last_visual` is read by the command line; resolve the range from the marks and drop the duplicate state.
  - File(s): `src/vim/mod.rs`, `src/vim/cmdline.rs`
- [ ] **Huge paste counts can abort the process** - `put` builds `register.text.repeat(count)` with counts up to 99,999, which can try to allocate gigabytes for a large register; cap the resulting size and report an error.
  - File(s): `src/vim/normal.rs`
- [ ] **Typing undo groups never close on their own** - `Tab::apply` keeps one undo group open for all consecutive `InsertText`, so minutes of typing undo in one step; close the group on a pause or a newline. `Buffer::undo` / `redo` / `save` also do not assert that no group is open.
  - File(s): `src/tab.rs`, `src/buffer.rs`
- [ ] **Session errors swallowed** - `config::load_session` turns read and parse errors into an empty session silently, and `FedApp::save_session` only writes failures to stderr; surface both like `load_settings` does.
  - File(s): `src/config.rs`, `src/app.rs`
- [ ] **`:s` reads every line twice** - `substitute_lines` copies every line of the range into a `String` just to find the last matching one, then `search::substitute` reads and allocates them again; let `substitute` return the last changed line.
  - File(s): `src/vim/cmdline.rs`, `src/search.rs`

## Low / Nice to have

- [ ] **Per-frame allocations in settings and dialogs** - `show_settings` rebuilds `theme_names()` (a `Vec<String>`) every frame, `family_combo` lowercases every font name every frame while open, and `close_window_dialog` rebuilds the file name list every frame; compute these once when the window/dialog opens.
  - File(s): `src/app.rs`, `src/highlight.rs`, `src/settings_ui.rs`, `src/app/dialogs.rs`
- [ ] **Duplicated constants and helpers** - `FONT_SIZE_RANGE` / `TAB_WIDTH_RANGE` are defined in both `config.rs` and `settings_ui.rs`; `confirm_close_tab` / `confirm_close_window` repeat the same modal; the Escape/Enter field handling is copied three times in the find bar; `motion::next_word_start` / `prev_word_start` copy `text_nav`; `editing::insert_text` re-implements `normalize_newlines`; Markdown detection exists both as `is_markdown_path` and `SpellMode::for_syntax`.
  - File(s): `src/config.rs`, `src/settings_ui.rs`, `src/app/dialogs.rs`, `src/find_bar.rs`, `src/vim/motion.rs`, `src/editing.rs`, `src/markdown.rs`, `src/spell/mod.rs`
- [ ] **Dead code and a dropped style** - `text_nav::matching_bracket` and `words::words` are only used by tests, and `StyledSpan::bold` is computed but never applied in `HighlightDecorator::decorate`, so bold theme styles are lost.
  - File(s): `src/text_nav.rs`, `src/spell/words.rs`, `src/highlight.rs`
- [ ] **Undo history clones whole steps and is unbounded** - `Buffer::undo` / `redo` clone `history[position]` instead of borrowing it (a `replace_all` step duplicates the whole text), and the history has no size cap.
  - File(s): `src/buffer.rs`
- [ ] **Small vim correctness gaps** - `2.` after `d3w` deletes 6 words instead of 2 (`with_count` only swaps a leading count); Insert-mode soft tab uses the char column instead of the display column on lines containing tabs; `enclosing_open` walks back char by char with `rope.char(i)` to the start of the file.
  - File(s): `src/vim/normal.rs`, `src/vim/mod.rs`, `src/vim/textobj.rs`
- [ ] **Vim module layout and long functions** - Insert, Visual, command-line and operator handling all sit in `vim/mod.rs` while Normal mode is in `normal.rs`; `insert_key` (~70 lines) and `simple` (~65 lines) should be split, and `Operator` / `Target::Object` lack doc comments.
  - File(s): `src/vim/mod.rs`, `src/vim/normal.rs`
- [ ] **Magic numbers in UI drawing** - the About dialog painting (`draw_logo`, `draw_version_pill`, `show_about`) and the repeated filter field width `140.0` in the settings sections use bare literals; promote them to named constants like the rest of the code base.
  - File(s): `src/app/menu.rs`, `src/settings_ui.rs`
