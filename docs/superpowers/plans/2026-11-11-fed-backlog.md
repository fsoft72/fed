# fed backlog (deferred review findings)

Minor findings from per-task reviews, deferred during implementation. The final review marked them non-blocking.

- **Task 1**: egui_commonmark keeps default features (regex, pulldown_cmark) - no onig, ok
- **Task 1**: #[allow(dead_code)] on FedApp.files must go once files are used (Task 6/7)
- **Task 2**: mixed LF/CRLF file is saved all-CRLF
- **Task 2**: is_large long-line check counts trailing \n (off by one); LARGE_FILE_BYTES branch untested
- **Task 2**: write_copy leaves .fed-tmp on fs::write failure; save_as binds new path before write succeeds
- **Task 2**: undo/redo not guarded while a group is open; set_mark not clamped
- **Task 2**: test gaps (nested groups, marks on undo, multibyte delete)
- **Task 2**: write_copy ignores fs::metadata error silently; _normalize_newlines underscore naming inconsistent; from_text doc comment stale
- **Task 3**: find_next scans whole doc per call (perf on big files)
- **Task 3**: substitute counts empty matches while find_all skips them (undocumented); whole-line replace moves marks inside edited lines to line start
- **Task 3**: test gaps (multibyte substitute, find_next None)
- **Task 4**: EditCommand variants lack /// docs; indent tab-stop ignores tab expansion; char not grapheme movement
- **Task 4**: next_word_start skips empty lines (vim `w` stops on them) - check in Task 12
- **Task 4**: test gaps (Redo, Delete, SelectLineAt, PageDown, Cut with selection)
- **Task 5**: duplicate override chord resolution by Action::ALL order (user override may lose); warning wording
- **Task 5**: save_settings non-atomic write
- **Task 5**: no roundtrip test over all egui::Key names
- **Task 6**: wrap mode relayouts whole file on width change (should disable wrap for is_large); WrapCache never pruned; RowMap tops cloned per frame
- **Task 6**: hit_test re-lays out and re-decorates per drag frame; Copy sets scroll_to_cursor; ropey unicode_lines breaks on FF/VT/U+2028
- **Task 6**: no tests for selection_rects / WrapCache::update; view.rs ~1000 lines (input mapping could be own module)
- **Task 6**: interaction rect not widened with galley widening; h-scroll extent varies with visible lines
- **Task 7**: no tests for close flow / shortcut ordering; settings warning overwritten by open error; status expiry repaint late; duplicated modal scaffolding; app.rs ~610 lines; last-tab path kept in session; Save As onto a path open in another tab
- **Task 8**: Replace skips multi-line regex matches; invalid regex recompiled each frame; count_label O(n); editor_has_focus true when nothing focused (untested gating)
- **Task 9**: Highlighter::new + key String allocs per line_spans call even on cache hit; syntax_for_path every frame; span_kind to_string per scope
- **Task 9**: parse_line errors silently produce empty spans; deep-scroll reparse in one frame (no budget)
- **Task 9**: no tests for prepare/last_dirty and theme reset; decorator_is_plain_when_disabled misnamed
- **Task 10**: no test for markdown line-mapping slice; remote_path maps unknown lang to en_US; non-UTF-8 system dic not falling back to download; corrupt downloads not re-fetched; unbounded word cache; loader panic leaves Loading
- **Task 11**: image dest rfind collides with identical title; pre-encoded %20 double-encoded; preview reparses per keystroke (no size cap)
- **Task 12**: `.` with count after counted change multiplies counts; Visual changes not repeatable with `.`; j/k preferred col counts chars not display cols; SearchNext with big count rescans; missing W B E tests
- **Task 13**: vim_message global across tabs; `:,5s` empty start address unsupported; last_match caret stale with newline replacements; backward drag includes anchor char; app.rs ~1280 lines (split app/vim.rs); vim_quit/vim_write untested; paste in Insert mode unverified manually
- **Task 13**: refused :q ends frame; :x! rejected
- **Task 14**: Download dictionaries button only on Failed; Esc in open combo closes window; app.rs ~1540 lines (split app/settings.rs); tabs[active] direct index
- **Task 15**: README shortcuts table alignment; syntax take/put-back fragile; no test for large-file wrap off
- **Task 15**: open_links passes untrimmed URL; file: links open arbitrary local files with default handler (security: should drop file:)

Also from the final review (not fixed):
- vim: an app shortcut bound to Ctrl+R overrides vim redo (spec 5.5 collision rule)
- find bar: F3 / Shift+F3 are hard-coded, not in the configurable keymap
- vim: Delete/Home/End/PageUp/PageDown inactive in vim mode
- vim: Insert-mode paste goes through handle_key per char (slow for big pastes)
- buffer: atomic save overwrites read-only files and breaks hard links
- markdown: absolute image paths like /x/z.png not converted to file:// URLs
