# Vendored crates

## egui_commonmark_backend 0.26.0

Copy of the crates.io release, wired in with `[patch.crates-io]` in the root `Cargo.toml`.
egui_commonmark has no setting for the line spacing of the rendered text, so the copy adds
one (every change is marked `fed patch` in `src/misc.rs`):

- `LINE_SPACING_ID`, `set_line_spacing(ctx, factor)` and `line_spacing(ui)`: a line spacing
  factor kept in the egui temp data.
- `Style::to_richtext` sets `RichText::line_height` to the font row height times the factor
  (paragraphs, headings, lists, links, quotes and inline code; not code blocks).

When egui_commonmark is updated, copy the new backend release here and re-apply the patch.
