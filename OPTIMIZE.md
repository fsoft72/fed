# Optimization TODO

> Generated on 2026-11-12. Items sorted by importance.

## High

- [x] **Saving silently overwrites changes made on disk by other programs** - `Buffer` keeps no modification time or size of the file it loaded or last saved, so when another program (git checkout, a formatter, a second editor) changes the file, `save` replaces it without warning and the open tab keeps showing stale text. Remember the `mtime`/length at load and save, check them in `Buffer::save` (refuse with a dedicated `BufferError` variant unless forced) and offer a reload / overwrite choice in the save dialog flow and for vim `:w` / `:w!`.
  - File(s): `src/buffer.rs`, `src/app.rs` (`save_tab_outcome`), `src/app/dialogs.rs`, `src/app/vim.rs`

- [x] **Replace all and `:s` overflow the edit log and rebuild every cache** - `search::rewrite_lines` applies one `Buffer::replace` per matching line, so replacing on more than `EDIT_LOG_LEN` (1024) lines pushes the older entries out of the log and `dirty_lines_since` reports the whole buffer as replaced: the highlight cache, wrap heights, longest line, find matches and spell caches all restart from scratch, and every line is also stored as a separate `Edit` with its own `removed`/`inserted` copies. Apply the result as one replacement of the span from the first to the last changed line (or log a merged entry for grouped edits).
  - File(s): `src/search.rs` (`rewrite_lines`), `src/buffer.rs` (`apply`, `edit_log`)

- [x] **Replace all reports a count that disagrees with the matches shown** - `rewrite_lines` counts with `re.find_iter(&text).count()`, which includes empty matches, while `line_matches` (used by the find bar and the match highlights) skips them; a pattern such as `a*` reports and replaces far more occurrences than the bar displays. Count and replace with the same rule as `line_matches` (skip empty matches, or refuse patterns that can match the empty string).
  - File(s): `src/search.rs`

- [x] **Typing a find query rescans the whole document on the UI thread** - every change of the pattern (or of the case / regex toggles) misses the `MatchCache` and runs `search::find_all` over every line synchronously, so each keystroke in the find field on a 100 MB file stalls the UI for the full regex pass (only edits to the buffer are incremental). Search on a background thread with a rope snapshot (like `ProseRanges`), show the previous count while it runs, and cancel a stale search when the query changes again.
  - File(s): `src/find_bar.rs` (`matches_in`), `src/search.rs` (`find_all`)

- [x] **Word wrap re-measures every line on each width change** - `WrapCache::update` drops all heights when the wrap width changes, and `measure_line` builds a full galley for every line of the document; while the window or the preview panel is being resized this runs on every frame (hundreds of thousands of layouts for a file just below `LARGE_FILE_BYTES`). Measure only the lines near the viewport first and fill the rest lazily within a frame budget (estimating unmeasured lines as one row), as `HighlightCache` does with `FRAME_PARSE_BUDGET`.
  - File(s): `src/view.rs` (`WrapCache`, `measure_line`)

## Medium

- [x] **A crashed syntax warm-up disables highlighting forever, silently** - `HighlightAssets::ensure_warm` marks the syntax `Pending` and only the warm-up thread sets it to `Ready`; if `warm_up` panics (a syntect regex bug) the entry stays `Pending`, `Tab::show_editor` keeps drawing plain text for every file of that syntax, and nothing is reported. Run the warm-up under `catch_unwind` (like `spell::run_guarded`), mark the syntax `Ready` (or a new `Failed` state) in every case and surface failures in the status bar.
  - File(s): `src/highlight.rs` (`ensure_warm`, `warm_up`)

- [x] **Ad-hoc background job plumbing repeated in six places** - `LoadJob`, `AssetsHandle`, `ProseRanges`, `SpellService`, the font loader and the link opener each spawn a thread with their own channel or `Arc<Mutex<Option<T>>>` slot, their own repaint request and their own (different) panic handling. A small `BackgroundJob<T>` (spawn with a ctx, `poll() -> Option<Result<T, JobError>>`, panic turned into an error) would remove the duplication and give every job the same failure reporting.
  - File(s): `src/buffer.rs` (`LoadJob`), `src/highlight.rs` (`AssetsHandle`), `src/spell/prose.rs`, `src/spell/mod.rs`, `src/fonts.rs`, `src/app/status.rs`

- [x] **The decorator chain is hard-wired to concrete types** - `SpellDecorator` stores a `&mut HighlightDecorator` and calls its `spans()` to find comments and strings, `MatchDecorator` wraps the spell one, and `Tab::show_editor` has to `mem::take` both caches and `drop` the decorators by hand in the right order. Let the `Decorator` trait expose the token kinds of a line (or pass the highlight spans down through `decorate`) so the chain is built from `&mut dyn Decorator` layers and can be reordered or tested in isolation.
  - File(s): `src/tab.rs` (`show_editor`), `src/spell/mod.rs` (`SpellDecorator`), `src/highlight.rs`, `src/find_bar.rs` (`MatchDecorator`), `src/view.rs` (`Decorator`)

- [x] **Two `same_file` functions with different costs** - `app::same_file` canonicalizes both paths on every call (used by vim `:w {file}` and `write_copy_refused`), while `buffer::same_file` compares the canonical path the buffer already keeps. Drop the app version and use `Buffer::is_bound_to` with one `canonicalize` of the target, so there is a single definition of "same file".
  - File(s): `src/app.rs`, `src/app/vim.rs`, `src/buffer.rs`

- [x] **Quadratic char offsets when highlighting long lines** - `parse_line` computes `text[..range.start].chars().count()` for every span, so a line with thousands of tokens (minified code below `LONG_LINE_CHARS`) costs O(n^2) per parse; keep a running byte/char cursor across spans instead.
  - File(s): `src/highlight.rs` (`parse_line`)

- [x] **`view.rs` mixes five responsibilities in 1100 lines** - it holds the `Decorator` trait, key-to-command mapping, the word-wrap cache, longest-line tracking, mouse handling and painting, and `Frame::run` alone is ~100 lines doing layout, scrolling, page anchoring and IME output. Split it into `view/keys.rs` (`key_to_command`, `page_key`), `view/wrap.rs` (`WrapCache`, `RowMap`, `longest_line`) and keep `Frame` for layout and paint; extract the scroll and IME parts of `run` into their own methods.
  - File(s): `src/view.rs`

- [x] **The Markdown preview parses the document on the UI thread** - after each debounce `Preview::show` copies the rope to a `String`, parses it with pulldown-cmark in `rewrite_image_paths`, and egui_commonmark parses it again, all on the UI thread, while `ProseRanges` already parses the same text on a worker. Build the rewritten source on a background thread (same pattern as `ProseRanges`) and keep showing the previous render until it arrives.
  - File(s): `src/markdown.rs` (`Preview::show`), `src/spell/prose.rs`

## Low / Nice to have

- [x] **Preview scroll position is shared by every tab** - `Preview::show` uses `Id::unique(PREVIEW_ID)` for all tabs, so the egui scroll state of the preview carries over when switching between two Markdown tabs; salt the id with the tab id like the editor's `("fed_scroll", tab.id)`.
  - File(s): `src/markdown.rs`, `src/app.rs` (`preview_panel`)

- [x] **`preview_open` and `preview` duplicate one state** - `Tab::preview_open` is true exactly when `preview` is (or is about to be) `Some`, and both are kept in sync by `toggle_preview` and `preview_panel`; keep only `preview: Option<Preview>` (created by the toggle) to remove the invariant.
  - File(s): `src/tab.rs`, `src/app.rs`

- [x] **Syntax detection ignores file names and shebangs** - `syntax_for_path` looks only at the extension, so `Makefile`, `Dockerfile`, `.bashrc` or an extensionless `#!/usr/bin/env python` script are shown as plain text (and spell checked as prose). Fall back to `find_syntax_by_name` on the file name and `find_syntax_by_first_line` on the first line.
  - File(s): `src/highlight.rs` (`syntax_for_path`), `src/tab.rs` (`refresh_syntax`)

- [x] **Find bar match index is a linear scan every frame** - `count_label` runs `matches.iter().position(|m| *m == sel)` each frame the bar is open; the matches are sorted, so use `partition_point` like `next_match`.
  - File(s): `src/find_bar.rs` (`count_label`)

- [x] **Long command dispatch functions** - `editing::apply_command` (~125 lines) and `vim::motion::resolve` (~85 lines) are single large `match` blocks; group the arms by family (movement, deletion, selection) into helpers so each stays readable and testable on its own.
  - File(s): `src/editing.rs`, `src/vim/motion.rs`

- [x] **Malformed doc comment on `dirty_lines_since`** - two sentences were merged into one 150-column line ("...Every caller tracks its own revision. When `since` is older..."), and `first_dirty_line_since` has a single caller (`spell::prose::shift_stale`) that could use `dirty_lines_since(..).map(|d| d.start)` directly.
  - File(s): `src/buffer.rs`, `src/spell/prose.rs`
