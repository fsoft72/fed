# Changes

## 2026-11-11

- Added design spec `docs/superpowers/specs/2026-11-11-fed-editor-design.md` (approved).
- Added implementation plan `docs/superpowers/plans/2026-11-11-fed-editor.md`.
- Task 1: scaffolded the `fed` crate (all dependencies, rustfmt, release profile) with an empty eframe window showing "fed".
- Task 2: added rope-backed `Buffer` (`src/buffer.rs`) with CRLF handling, atomic save, grouped undo/redo, marks and dirty-line tracking.
- Task 3: added search engine (`src/search.rs`) with literal/regex queries, wrapping `find_next`, `find_all`, line-range `substitute` and `replace_all` (single undo step).
- Task 4: added `src/text_nav.rs` (char classes, word motions, line helpers, bracket matching) and `src/editing.rs` (`Selection`, `EditorState`, `EditCommand`, `apply_command` for non-vim editing).
