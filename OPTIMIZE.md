# Optimization TODO

> Generated on 2026-11-12. Items sorted by importance.

## Critical

- [ ] **Saves are not durable (no fsync before rename)** - `write_copy` writes the temp file with `fs::write` and renames it over the original without `File::sync_all` (and without syncing the parent directory), so a crash or power loss right after a save can leave a truncated or empty file on file systems that reorder data and metadata; this is the one place an editor must not lose data.
  - File(s): `src/buffer.rs` (`Buffer::write_copy`)

## High

- [ ] **Tab rendering disagrees with tab-stop columns** - the view draws every `\t` as a fixed `tab_width` spaces (`expand_tabs`, `display_col`, `buffer_col`, `display_width`), while `text_nav::display_col` (used by `indent` and vim insert) advances to the next tab stop; text like `ab\tx` is drawn at a different column than the editing logic assumes and columns of mixed tabs/spaces never line up. Make the view use tab stops (one shared helper) so layout, hit testing, selection rects and indentation agree.
  - File(s): `src/view.rs`, `src/text_nav.rs`, `src/editing.rs`, `src/vim/insert.rs`

- [ ] **Find matches rescan the whole document on every edit, on the UI thread** - `FindBar::matches` calls `search::find_all` over every line whenever the revision changes, so each keystroke with the find bar open costs a full regex pass (seconds on a large file, which is still searchable); it is also computed in `FedApp::ui` before `show_editor` applies the frame's keys, so the match highlights are one revision stale in the edit frame. Update the cache incrementally with `Buffer::dirty_lines_since` (rescan only the changed lines and shift the rest) and compute it after the keyboard edits (e.g. in `MatchDecorator::prepare`).
  - File(s): `src/find_bar.rs`, `src/search.rs`, `src/app.rs`

- [ ] **Two parallel dirty-line mechanisms in `Buffer`** - `take_dirty_line` mutates a `Cell` through `&Buffer` and can only have one consumer (the highlight decorator of the frame that happens to run `prepare`), while `dirty_lines_since` already gives every consumer its own revision-based view; `HighlightDecorator::last_dirty` is dead code left over from it. Move `HighlightCache` to `dirty_lines_since` with its own revision and delete `dirty_line`, `take_dirty_line` and `last_dirty`.
  - File(s): `src/buffer.rs`, `src/highlight.rs`

- [ ] **Saving copies the whole text two or three times** - `write_copy` builds `rope.to_string()` and then a second `String` with `replace('\n', "\r\n")` for CRLF files, so saving a 256 MB file allocates over 500 MB; stream `rope.chunks()` through a `BufWriter`, converting line endings on the fly.
  - File(s): `src/buffer.rs` (`Buffer::write_copy`)

## Medium

- [ ] **`is_large` is never updated after creation** - the flag is computed in `from_text` only, so pasting megabytes or a 50k-char line into a small buffer keeps syntax highlighting, spell check and word wrap on and the UI stalls; re-evaluate it in `Buffer::apply` (byte length threshold plus the length of the edited lines).
  - File(s): `src/buffer.rs`, `src/app.rs` (`view_options`), `src/tab.rs`

- [ ] **Hit testing re-lays out and re-decorates a line on every drag frame** - `Frame::hit_test` calls `layout_line` (highlight + spell + match decorators and a new galley) for each press, drag, double/triple and right click, and `run` lays out the visible lines again afterwards; since the selection does not change the galley, lay out the visible lines first and hit-test against them (falling back to a plain layout only for off-screen lines).
  - File(s): `src/view.rs` (`Frame::run`, `Frame::hit_test`, `Frame::handle_mouse`)

- [ ] **Per-frame allocations in the render path** - each visible line is copied to a `String` by `line_text` up to three times per frame (`layout_line`, `paint`, `caret_rect`), then `build_layout_job` collects a `Vec<char>`, allocates a `String` per span and another one in `expand_tabs`; borrow the line like `search::line_text` (`Cow`), compute it once per line in `Frame::run` and append byte slices to the `LayoutJob`.
  - File(s): `src/view.rs`

- [ ] **Syntax highlighting reparses everything after an edit** - `HighlightCache::invalidate_from` drops all spans after the edited line, so editing near the top of a long file reparses up to the viewport on every keystroke (spread over frames, with lines drawn plain meanwhile); stop reparsing when the parser state at the next checkpoint equals the old one and reuse the remaining spans shifted by the line delta from `dirty_lines_since`.
  - File(s): `src/highlight.rs`

- [ ] **Duplicated line and column helpers** - `line_text` exists in `view.rs` (owned `String`) and `search.rs` (borrowed `Cow`) and is inlined again in `find_bar.rs` (`expand_match`); `display_col` exists in both `view.rs` and `text_nav.rs` with different semantics; `vim::motion::is_empty_line` overlaps `text_nav::is_empty_line_at`. Keep one version of each in `text_nav`.
  - File(s): `src/view.rs`, `src/search.rs`, `src/find_bar.rs`, `src/text_nav.rs`, `src/vim/motion.rs`

- [ ] **Vim key translation lives in the editor widget** - `egui_event_to_vim_keys`, `egui_event_to_vim_key` and `key_to_vim_key` are in `view.rs`, so the generic widget depends on `vim::VimKey` and `app/vim.rs` reaches into `view` for vim logic; move them to a `vim::keys` module next to `VimKey`.
  - File(s): `src/view.rs`, `src/app/vim.rs`, `src/vim/mod.rs`

- [ ] **Vim Insert-mode paste is replayed one key at a time** - `route_event` turns a pasted string into one `VimKey::Char` per character, so pasting a large text runs `handle_key` and a rope insert per char; add a text-insert path (e.g. `VimKey::Text(String)` or a `VimState::insert_text`) that applies the paste as one edit.
  - File(s): `src/app/vim.rs`, `src/vim/insert.rs`

- [ ] **`FedApp::ui` central-panel closure does too much** - besides drawing it polls the syntax assets (`self.loaded_assets = self.assets.get()`), picks the code theme, builds the `EditorEnv`, routes vim keys and records the context click; extract an `editor_panel` method returning a small result struct and poll the assets at the top of the frame with the other background jobs (fonts, loads, links).
  - File(s): `src/app.rs`

- [ ] **The application shell is hard to test** - `FedApp::new` calls `ConfigPaths::detect`, spawns the asset and spell threads, and save/open paths call `rfd` dialogs directly (`save_dialog`, `open_dialog`), so the tab/session/save logic in `app.rs` and `app/*.rs` can only be tested through free helper functions; inject the config paths and a small file-dialog trait so `FedApp` can be built in tests.
  - File(s): `src/app.rs`, `src/app/dialogs.rs`, `src/app/vim.rs`

## Low / Nice to have

- [ ] **Shortcut aliases are re-parsed on every key press** - `shortcut_action` runs `KeyChord::parse` for every `SHORTCUT_ALIASES` entry each time a key is not in the keymap; parse them once into a `LazyLock<Vec<(KeyChord, Action)>>`.
  - File(s): `src/app.rs`

- [ ] **The whole input event list is cloned every frame** - `handle_keyboard` does `ui.input(|i| i.events.clone())` even when no key was pressed; clone only the keyboard events (or return early when there are none).
  - File(s): `src/view.rs` (`handle_keyboard`)

- [ ] **Spell checker lock is always taken for writing on the UI thread** - `SpellChecker::is_correct` needs `&mut self` only for its word cache, so `SpellDecorator::misspelled` takes the `RwLock` write lock and can wait on `add_word`; make the cache interior (`Mutex<HashMap>`) or use a plain `Mutex` and drop the `RwLock`.
  - File(s): `src/spell/mod.rs`

- [ ] **Inconsistent lock poisoning policy** - `SpellService::status` recovers from poisoning with `PoisonError::into_inner`, while `checker()`, `AssetsHandle::get`, `ensure_warm` and the decorators `expect` on the same kind of locks, so a panic in a background thread turns into a UI crash on the next frame; pick one policy (recover and report) for all shared slots.
  - File(s): `src/spell/mod.rs`, `src/highlight.rs`

- [ ] **`same_file` canonicalizes every open tab on each open** - `try_open` calls `canonicalize` for both paths of every tab and every pending load, so restoring a session of N files makes O(N^2) file system calls; store the canonical path in `Buffer` when it is loaded or saved and compare that.
  - File(s): `src/app.rs`, `src/app/vim.rs`, `src/buffer.rs`

- [ ] **Closed previews keep their memory** - `Tab::preview` (a `CommonMarkCache` plus a full copy of the source) stays allocated after the preview is toggled off; set it to `None` when `preview_open` becomes false.
  - File(s): `src/app.rs` (`toggle_preview`), `src/tab.rs`
