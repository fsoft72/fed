//! State machine tests for the vim core, driven by key strings.

use super::*;

const SET: EditSettings = EditSettings {
    tab_width: 4,
    insert_spaces: true,
};

/// Parses a key string: plain chars plus `<Esc>`, `<CR>`, `<BS>`, `<Tab>`, arrows and `<C-x>`.
fn keys(s: &str) -> Vec<VimKey> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '<'
            && let Some(close) = rest.find('>')
        {
            let name = &rest[1..close];
            let key = match name {
                "Esc" => Some(VimKey::Esc),
                "CR" => Some(VimKey::Enter),
                "BS" => Some(VimKey::Backspace),
                "Tab" => Some(VimKey::Tab),
                "Left" => Some(VimKey::Left),
                "Right" => Some(VimKey::Right),
                "Up" => Some(VimKey::Up),
                "Down" => Some(VimKey::Down),
                _ => name
                    .strip_prefix("C-")
                    .map(|x| VimKey::Ctrl(x.chars().next().unwrap())),
            };
            if let Some(key) = key {
                out.push(key);
                rest = &rest[close + 1..];
                continue;
            }
        }
        out.push(VimKey::Char(c));
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// A vim state bound to a buffer, collecting effects.
struct Harness {
    vim: VimState,
    buf: Buffer,
    ed: EditorState,
    fx: Vec<VimEffect>,
}

impl Harness {
    /// Creates a harness with `text` and the caret at `cursor`.
    fn new(text: &str, cursor: usize) -> Harness {
        Harness {
            vim: VimState::new(),
            buf: Buffer::from_text(text),
            ed: EditorState {
                selection: Selection::caret(cursor),
                preferred_col: None,
            },
            fx: Vec::new(),
        }
    }

    /// Feeds a key string and keeps the effects.
    fn feed(&mut self, s: &str) -> &mut Self {
        for key in keys(s) {
            let fx = self.vim.handle_key(key, &mut self.buf, &mut self.ed, &SET);
            self.fx.extend(fx);
        }
        self
    }

    /// Returns the caret position.
    fn caret(&self) -> usize {
        self.ed.selection.head
    }
}

/// Runs `keys` on `text` from `cursor`; returns text, caret and mode.
fn run(text: &str, cursor: usize, keys: &str) -> (String, usize, Mode) {
    let mut h = Harness::new(text, cursor);
    h.feed(keys);
    (h.buf.text(), h.caret(), h.vim.mode())
}

/// Returns only the caret after `keys`.
fn caret(text: &str, cursor: usize, keys: &str) -> usize {
    run(text, cursor, keys).1
}

const NAV: &str = "foo bar.baz\n  qux end\nlast";

#[test]
fn motions_basic() {
    assert_eq!(caret(NAV, 0, "w"), 4);
    assert_eq!(caret(NAV, 0, "ww"), 7);
    assert_eq!(caret(NAV, 8, "b"), 7);
    assert_eq!(caret(NAV, 4, "b"), 0);
    assert_eq!(caret(NAV, 0, "e"), 2);
    assert_eq!(caret(NAV, 0, "$"), 10);
    assert_eq!(caret(NAV, 9, "0"), 0);
    assert_eq!(caret(NAV, 18, "^"), 14);
    assert_eq!(caret(NAV, 23, "gg"), 0);
    assert_eq!(caret(NAV, 0, "G"), 22);
    assert_eq!(caret(NAV, 0, "3G"), 22);
    assert_eq!(caret(NAV, 0, "2G"), 14);
    assert_eq!(caret(NAV, 0, "jj"), 22);
    assert_eq!(caret(NAV, 9, "jk"), 9);
    assert_eq!(caret(NAV, 0, "$j"), 20);
    assert_eq!(caret(NAV, 0, "l"), 1);
    assert_eq!(caret(NAV, 10, "l"), 10);
    assert_eq!(caret("foo\n\nbar", 0, "w"), 4);
}

#[test]
fn counts_on_motions() {
    assert_eq!(caret(NAV, 0, "3w"), 8);
    assert_eq!(caret(NAV, 0, "2j"), 22);
    assert_eq!(caret(NAV, 0, "10l"), 10);
    let mut h = Harness::new(NAV, 0);
    h.feed("2d");
    assert_eq!(h.vim.pending(), "2d");
    h.feed("<Esc>");
    assert_eq!(h.vim.pending(), "");
    assert_eq!(h.buf.text(), NAV);
}

#[test]
fn dw_deletes_word() {
    assert_eq!(
        run("foo bar baz", 0, "dw"),
        ("bar baz".into(), 0, Mode::Normal)
    );
    assert_eq!(
        run("foo bar\nbaz", 4, "dw"),
        ("foo \nbaz".into(), 3, Mode::Normal)
    );
    assert_eq!(run("foo bar\n  baz", 4, "dw").0, "foo \n  baz");
    assert_eq!(run("a b c d", 0, "2dw").0, "c d");
    assert_eq!(run("a b c d", 0, "d2w").0, "c d");
}

#[test]
fn cw_changes_to_word_end() {
    assert_eq!(
        run("foo bar", 0, "cwxy<Esc>"),
        ("xy bar".into(), 1, Mode::Normal)
    );
    assert_eq!(run("a b c", 0, "c2wz<Esc>").0, "z c");
    assert_eq!(run("foo bar", 3, "cwx<Esc>").0, "fooxbar");
}

#[test]
fn ciw_and_di_quote() {
    assert_eq!(run("say hello now", 5, "ciwbye<Esc>").0, "say bye now");
    assert_eq!(
        run("x = \"abc\" y", 6, "di\""),
        ("x = \"\" y".into(), 5, Mode::Normal)
    );
    assert_eq!(run("x = \"abc\" y", 0, "da\"").0, "x = y");
    assert_eq!(run("f(a, b)", 3, "ci(z<Esc>").0, "f(z)");
}

#[test]
fn dd_with_count_and_p_linewise() {
    let mut h = Harness::new("a\nb\nc\nd", 0);
    h.feed("2dd");
    assert_eq!((h.buf.text(), h.caret()), ("c\nd".into(), 0));
    assert_eq!(h.fx, vec![VimEffect::SetClipboard("a\nb\n".into())]);
    h.feed("p");
    assert_eq!((h.buf.text(), h.caret()), ("c\na\nb\nd".into(), 2));
    assert_eq!(run("a\nb", 2, "dd"), ("a".into(), 0, Mode::Normal));
    assert_eq!(run("a\nb\n", 2, "dd").0, "a\n");
    assert_eq!(run("a\nb", 2, "yyp").0, "a\nb\nb");
}

#[test]
fn yy_p_pastes_above() {
    let mut h = Harness::new("one\ntwo", 4);
    h.feed("yyP");
    assert_eq!((h.buf.text(), h.caret()), ("one\ntwo\ntwo".into(), 4));
    assert_eq!(h.fx, vec![VimEffect::SetClipboard("two\n".into())]);
}

#[test]
fn x_and_dot_repeat() {
    assert_eq!(run("abcdef", 0, "x.."), ("def".into(), 0, Mode::Normal));
    assert_eq!(run("abcdef", 0, "2x.").0, "ef");
    assert_eq!(run("abcdef", 0, "x3.").0, "ef");
    assert_eq!(run("foo bar baz", 0, "cwxx<Esc>w.").0, "xx xx baz");
    assert_eq!(run("a\nb\nc", 0, "Ax<Esc>j.").0, "ax\nbx\nc");
    assert_eq!(run("abc", 2, "x").1, 1);
}

#[test]
fn undo_redo_ctrl_r() {
    let mut h = Harness::new("abc", 0);
    h.feed("x");
    assert_eq!(h.buf.text(), "bc");
    h.feed("u");
    assert_eq!(h.buf.text(), "abc");
    h.feed("<C-r>");
    assert_eq!(h.buf.text(), "bc");
    h.feed("uu");
    assert_eq!(
        h.fx.last(),
        Some(&VimEffect::Message("Already at oldest change".into()))
    );
    assert_eq!(run("abc", 0, "ifoo<CR>bar<Esc>u").0, "abc");
    assert_eq!(run("foo bar", 0, "cwxy<Esc>u").0, "foo bar");
    assert_eq!(run("abc", 2, "xu").1, 2);
}

#[test]
fn visual_line_delete() {
    let mut h = Harness::new("a\nb\nc", 2);
    h.feed("Vj");
    assert_eq!(h.vim.mode(), Mode::VisualLine);
    assert_eq!(h.vim.visual_range(&h.buf, &h.ed), Some(2..5));
    h.feed("d");
    assert_eq!(
        (h.buf.text(), h.caret(), h.vim.mode()),
        ("a".into(), 0, Mode::Normal)
    );
    assert_eq!(h.fx, vec![VimEffect::SetClipboard("b\nc\n".into())]);
    assert_eq!(h.vim.visual_range(&h.buf, &h.ed), None);
}

#[test]
fn visual_char_yank_emits_clipboard() {
    let mut h = Harness::new("hello world", 0);
    h.feed("ve");
    assert_eq!(h.vim.visual_range(&h.buf, &h.ed), Some(0..5));
    h.feed("y");
    assert_eq!(h.fx, vec![VimEffect::SetClipboard("hello".into())]);
    assert_eq!((h.caret(), h.vim.mode()), (0, Mode::Normal));
    assert_eq!(run("hello world", 6, "vbd").0, "orld");
    assert_eq!(run("hello world", 6, "vhd").0, "helloorld");
    assert_eq!(run("hello world", 0, "veold").0, "h world");
    assert_eq!(run("hello world", 0, "vecX<Esc>").0, "X world");
    assert_eq!(run("a\nb", 0, "vj>").0, "    a\n    b");
}

#[test]
fn find_char_and_semicolon() {
    assert_eq!(caret("a,b,c,d", 0, "f,"), 1);
    assert_eq!(caret("a,b,c,d", 0, "f,;"), 3);
    assert_eq!(caret("a,b,c,d", 0, "f,;;,"), 3);
    assert_eq!(caret("a,b,c,d", 0, "2f,"), 3);
    assert_eq!(caret("a,b,c,d", 0, "t,;"), 2);
    assert_eq!(caret("a,b,c,d", 6, "F,"), 5);
    assert_eq!(caret("a,b,c,d", 6, "T,;"), 4);
    assert_eq!(caret("a,b,c,d", 0, "fz"), 0);
    assert_eq!(run("a,b,c,d", 0, "dt,").0, ",b,c,d");
    assert_eq!(run("a,b,c,d", 0, "df,").0, "b,c,d");
}

#[test]
fn percent_matches_bracket() {
    assert_eq!(caret("if (a[1]) {}", 0, "%"), 8);
    assert_eq!(caret("if (a[1]) {}", 0, "%%"), 3);
    assert_eq!(caret("if (a[1]) {}", 5, "%"), 7);
    assert_eq!(run("if (a[1]) {}", 3, "d%").0, "if  {}");
}

#[test]
fn marks_jump_line_and_exact() {
    let mut h = Harness::new("  abc\n  def", 4);
    h.feed("maG");
    assert_eq!(h.caret(), 8);
    h.feed("'a");
    assert_eq!(h.caret(), 2);
    h.feed("G`a");
    assert_eq!(h.caret(), 4);
    h.feed("'b");
    assert_eq!(h.fx, vec![VimEffect::Error("E20: Mark not set".into())]);
    assert_eq!(run("a\nb\nc", 4, "magg'ad").0, "a\nb\nc");
    assert_eq!(run("a\nb\nc", 4, "magg d'a").0, "");
}

#[test]
fn marks_follow_edits() {
    let mut h = Harness::new("abc\ndef", 5);
    h.feed("magg");
    h.feed("Ox<Esc>");
    assert_eq!(h.buf.text(), "x\nabc\ndef");
    h.feed("`a");
    assert_eq!(h.caret(), 7);
}

#[test]
fn search_forward_and_n() {
    let mut h = Harness::new("foo bar foo baz foo", 0);
    h.feed("/foo");
    assert_eq!((h.vim.mode(), h.vim.cmdline()), (Mode::CommandLine, "/foo"));
    h.feed("<CR>");
    assert_eq!((h.caret(), h.vim.mode()), (8, Mode::Normal));
    h.feed("n");
    assert_eq!(h.caret(), 16);
    h.feed("n");
    assert_eq!(h.caret(), 0);
    h.feed("N");
    assert_eq!(h.caret(), 16);
    h.feed("?bar<CR>");
    assert_eq!(h.caret(), 4);
    h.feed("n");
    assert_eq!(h.caret(), 4);
    h.feed("/zzz<CR>");
    assert_eq!(
        h.fx.last(),
        Some(&VimEffect::Error("E486: Pattern not found: zzz".into()))
    );
    h.feed("/(<CR>");
    assert!(matches!(h.fx.last(), Some(VimEffect::Error(e)) if e.starts_with("invalid pattern")));
    assert_eq!(caret("foo x foo", 0, "*"), 6);
    assert_eq!(caret("foo x foo", 6, "#"), 0);
    assert_eq!(caret("foo x foobar foo", 0, "*"), 13);
    assert_eq!(caret("foo bar foo", 0, "/o<CR>n"), 2);
    assert_eq!(caret("foo bar foo", 0, "/<CR>"), 0);
}

#[test]
fn insert_esc_moves_left() {
    assert_eq!(run("abc", 0, "Axy<Esc>"), ("abcxy".into(), 4, Mode::Normal));
    assert_eq!(run("abc", 0, "i<Esc>"), ("abc".into(), 0, Mode::Normal));
    assert_eq!(run("abc", 1, "a<Esc>").1, 1);
    assert_eq!(run("abc", 3, "iX<BS>Y<Esc>").0, "abYc");
    assert_eq!(run("  ab", 3, "a<CR>x<Esc>").0, "  ab\n  x");
    assert_eq!(run("ab", 0, "I<Tab><Esc>").0, "    ab");
    assert_eq!(run("abc", 0, "sX<Esc>").0, "Xbc");
    assert_eq!(run("  abc\nd", 3, "Sx<Esc>").0, "  x\nd");
    assert_eq!(run("abc def", 4, "Cx<Esc>").0, "abc x");
    assert_eq!(run("abc def", 4, "D"), ("abc ".into(), 3, Mode::Normal));
}

#[test]
fn o_opens_line_with_indent() {
    assert_eq!(
        run("  foo\nbar", 2, "obar<Esc>"),
        ("  foo\n  bar\nbar".into(), 10, Mode::Normal)
    );
    assert_eq!(
        run("  foo", 2, "Ox<Esc>"),
        ("  x\n  foo".into(), 2, Mode::Normal)
    );
}

#[test]
fn r_replaces_char() {
    assert_eq!(run("abc", 1, "rx"), ("axc".into(), 1, Mode::Normal));
    assert_eq!(run("abc", 0, "3rx"), ("xxx".into(), 2, Mode::Normal));
    assert_eq!(run("abc", 0, "5rx").0, "abc");
}

#[test]
fn j_joins_lines() {
    assert_eq!(
        run("foo\n   bar\nbaz", 0, "J"),
        ("foo bar\nbaz".into(), 3, Mode::Normal)
    );
    assert_eq!(run("foo\n   bar\nbaz", 0, "3J").0, "foo bar baz");
    assert_eq!(run("foo\n\nbaz", 0, "J").0, "foo\nbaz");
    assert_eq!(run("foo", 0, "J").0, "foo");
}

#[test]
fn indent_shift_right() {
    assert_eq!(
        run("foo\nbar", 0, ">>"),
        ("    foo\nbar".into(), 4, Mode::Normal)
    );
    assert_eq!(run("foo\nbar", 0, "2>>").0, "    foo\n    bar");
    assert_eq!(run("      foo", 0, "<<").0, "  foo");
    assert_eq!(run("foo\n\nbar", 0, ">2j").0, "    foo\n\n    bar");
}

#[test]
fn accented_text_motions() {
    assert_eq!(caret("è già qui", 0, "ww"), 6);
    assert_eq!(caret("è già qui", 6, "b"), 2);
    assert_eq!(run("è già qui", 2, "dw").0, "è qui");
}

#[test]
fn paragraph_motions_with_operators() {
    assert_eq!(run("a\nb\n\nc", 0, "d}").0, "\nc");
    assert_eq!(caret("a\nb\n\nc", 0, "}"), 4);
    assert_eq!(caret("a\nb\n\nc", 5, "{"), 4);
    assert_eq!(run("a\nb\nc", 0, "dj").0, "c");
}

#[test]
fn cmdline_editing_and_unknown_command() {
    let mut h = Harness::new("abc", 0);
    h.feed(":foo");
    assert_eq!((h.vim.mode(), h.vim.cmdline()), (Mode::CommandLine, ":foo"));
    h.feed("<CR>");
    assert_eq!(h.vim.mode(), Mode::Normal);
    assert_eq!(
        h.fx,
        vec![VimEffect::Error("E492: not an editor command: foo".into())]
    );
    h.feed(":a<BS>");
    assert_eq!(h.vim.cmdline(), ":");
    h.feed("<BS>");
    assert_eq!((h.vim.mode(), h.vim.cmdline()), (Mode::Normal, ""));
    h.feed("vl:");
    assert_eq!(
        (h.vim.mode(), h.vim.cmdline()),
        (Mode::CommandLine, ":'<,'>")
    );
    assert_eq!((h.buf.mark('<'), h.buf.mark('>')), (Some(0), Some(1)));
    h.feed("<Esc>");
    assert_eq!(h.vim.mode(), Mode::Normal);
}

#[test]
fn empty_buffer_never_panics() {
    for k in [
        "x", "dd", "p", "w", "b", "e", "$", "G", "J", "dw", "ciw", "%", "}", "{", "vd", "Vd",
    ] {
        assert_eq!(run("", 0, k).0, "", "keys {k}");
    }
    assert_eq!(run("", 0, "ohi<Esc>").0, "\nhi");
    assert_eq!(run("a\n", 2, "x"), ("a\n".into(), 2, Mode::Normal));
    assert_eq!(run("a\n", 0, "x").0, "\n");
}

#[test]
fn backward_exclusive_motion_ending_in_column_zero() {
    let mut h = Harness::new("x foo\nbar", 6);
    h.feed("db");
    assert_eq!((h.buf.text(), h.caret()), ("x \nbar".into(), 1));
    assert_eq!(h.fx, vec![VimEffect::SetClipboard("foo".into())]);
    assert_eq!(run("x foo\nbar", 6, "dB").0, "x \nbar");
    // Starting before the first non-blank, the range becomes linewise.
    let mut h = Harness::new("foo\nbar", 4);
    h.feed("db");
    assert_eq!(h.buf.text(), "bar");
    assert_eq!(h.fx, vec![VimEffect::SetClipboard("foo\n".into())]);
    assert_eq!(run("a\n\nb\nc", 5, "d{").0, "a\nc");
    assert_eq!(run("foo\nfoo\nbar", 0, "/foo<CR>jdN").0, "foo\nbar");
}

#[test]
fn preferred_column_resets_after_edits() {
    assert_eq!(caret("abcdef\nab\nabcdef", 5, "jxj"), 9);
    assert_eq!(caret("abcdef\nab\nabcdef", 5, "jj"), 15);
    assert_eq!(caret("abcdef\nab\nabcdef", 0, "$jj"), 15);
    assert_eq!(caret("abcdef\nab\nabcdef", 5, "jr.j"), 11);
    assert_eq!(
        run("abcdef\nab\nabcdef", 5, "jvd"),
        ("abcdef\na\nabcdef".into(), 7, Mode::Normal)
    );
    assert_eq!(caret("abcdef\nab\nabcdef", 5, "jvd<Esc>j"), 9);
    assert_eq!(caret("abcdef\nab\nabcdef", 5, "ja<Left><Esc>j"), 10);
}

#[test]
fn esc_drops_untouched_auto_indent() {
    assert_eq!(
        run("  foo", 2, "o<Esc>"),
        ("  foo\n".into(), 6, Mode::Normal)
    );
    assert_eq!(run("  foo", 2, "O<Esc>").0, "\n  foo");
    assert_eq!(run("  foo", 2, "ox<Esc>").0, "  foo\n  x");
    assert_eq!(run("  foo", 0, "A<CR><CR>x<Esc>").0, "  foo\n\n  x");
    assert_eq!(
        run("  foo", 0, "A<CR><Esc>"),
        ("  foo\n".into(), 6, Mode::Normal)
    );
    assert_eq!(run("foo", 0, "o<Esc>"), ("foo\n".into(), 4, Mode::Normal));
    assert_eq!(run("  foo\nbar", 2, "cc<Esc>").0, "\nbar");
    assert_eq!(run("  foo", 2, "o<Tab><Esc>").0, "  foo\n    ");
}

#[test]
fn enter_moves_to_next_line_first_non_blank() {
    assert_eq!(caret("a\n  b\n  c", 0, "<CR>"), 4);
    assert_eq!(caret("a\n  b\n  c", 0, "2<CR>"), 8);
    assert_eq!(caret("a\n  b", 4, "<CR>"), 4);
    assert_eq!(run("a\nb\nc", 0, "d<CR>").0, "c");
}

#[test]
fn empty_cmdline_is_a_no_op() {
    let mut h = Harness::new("abc", 1);
    h.feed(":<CR>");
    assert_eq!((h.vim.mode(), h.caret()), (Mode::Normal, 1));
    assert!(h.fx.is_empty());
    h.feed(":  <CR>");
    assert!(h.fx.is_empty());
}

#[test]
fn trailing_empty_line_is_not_a_motion_target() {
    assert_eq!(caret("a\nb\n", 0, "G"), 2);
    assert_eq!(caret("a\nb\n", 0, "5G"), 2);
    assert_eq!(caret("a\nb\n", 2, "j"), 2);
    assert_eq!(caret("a\nb\n", 0, "5j"), 2);
    assert_eq!(caret("a\nb\n", 0, "}"), 2);
    assert_eq!(run("a\nb\n", 0, "5dd").0, "");
    // An edit may leave the caret on it, and Normal-mode clamping keeps it there.
    assert_eq!(run("abc", 0, "o<Esc>"), ("abc\n".into(), 4, Mode::Normal));
    assert_eq!(caret("abc", 0, "o<Esc>j"), 4);
    assert_eq!(caret("abc", 0, "o<Esc>k"), 0);
    assert_eq!(caret("abc", 0, "o<Esc>$"), 4);
    assert_eq!(run("abc", 0, "o<Esc>dd"), ("abc".into(), 0, Mode::Normal));
    assert_eq!(run("abc", 0, "o<Esc>Vd").0, "abc");
}

#[test]
fn ex_substitute_through_keys() {
    let mut h = Harness::new("aaa\nx aaa aaa\n", 0);
    h.feed(":%s/aaa/b/g<CR>");
    assert_eq!(h.buf.text(), "b\nx b b\n");
    assert_eq!(h.fx, vec![VimEffect::Message("3 substitutions".into())]);
    assert_eq!(h.vim.mode(), Mode::Normal);
    h.feed("u");
    assert_eq!(h.buf.text(), "aaa\nx aaa aaa\n");
}

#[test]
fn ex_visual_range_uses_last_selection() {
    let mut h = Harness::new("a\na\na\na\n", 2);
    h.feed("Vj:s/a/b/<CR>");
    assert_eq!(h.buf.text(), "a\nb\nb\na\n");
    assert_eq!((h.buf.mark('<'), h.buf.mark('>')), (Some(2), Some(4)));
}

#[test]
fn ex_visual_range_follows_edits_made_after_the_selection() {
    let mut h = Harness::new("a\na\na\na\n", 2);
    h.feed("Vj<Esc>ggOx<Esc>");
    assert_eq!(h.buf.text(), "x\na\na\na\na\n");
    h.feed(":'<,'>s/a/b/<CR>");
    assert_eq!(h.buf.text(), "x\na\nb\nb\na\n");
}

#[test]
fn visual_shift_ending_on_a_line_break_stays_on_its_line() {
    let mut h = Harness::new("a\nb\n", 0);
    h.feed("v$>");
    assert_eq!(h.buf.text(), "    a\nb\n");
    let mut h = Harness::new("a\n\nb\n", 2);
    h.feed("v>");
    assert_eq!(h.buf.text(), "a\n\nb\n");
    let mut h = Harness::new("    a\n    b\n", 4);
    h.feed("v$<");
    assert_eq!(h.buf.text(), "a\n    b\n");
}

#[test]
fn dot_repeats_visual_operators() {
    let mut h = Harness::new("abc\ndef\nghi\njkl\n", 0);
    h.feed("vjd");
    assert_eq!(h.buf.text(), "ef\nghi\njkl\n");
    h.feed(".");
    assert_eq!(h.buf.text(), "hi\njkl\n");

    let mut h = Harness::new("1\n2\n3\n4\n5\n", 0);
    h.feed("Vjd");
    assert_eq!(h.buf.text(), "3\n4\n5\n");
    h.feed(".");
    assert_eq!(h.buf.text(), "5\n");

    let mut h = Harness::new("abcdef", 0);
    h.feed("vlcX<Esc>");
    assert_eq!(h.buf.text(), "Xcdef");
    h.feed("l.");
    assert_eq!(h.buf.text(), "XXef");

    let mut h = Harness::new("a\nb\nc\n", 0);
    h.feed("V>j.");
    assert_eq!(h.buf.text(), "    a\n    b\nc\n");
    // A yank is not a change: `.` still repeats the last change.
    h.feed("vy.");
    assert_eq!(h.buf.text(), "    a\n        b\nc\n");
}

#[test]
fn ex_unknown_and_file_commands() {
    let mut h = Harness::new("a\n", 0);
    h.feed(":foo<CR>");
    assert!(matches!(&h.fx[..], [VimEffect::Error(e)] if e.contains("not an editor command")));
    h.fx.clear();
    h.feed(":wq<CR>:q!<CR>");
    assert_eq!(
        h.fx,
        vec![VimEffect::WriteQuit, VimEffect::Quit { force: true }]
    );
}

#[test]
fn ex_mark_address_and_goto() {
    let mut h = Harness::new("a\nb\nc\nd\n", 2);
    h.feed("majj:'a,.s/./x/<CR>");
    assert_eq!(h.buf.text(), "a\nx\nx\nx\n");
    h.feed(":1<CR>");
    assert_eq!(h.caret(), 0);
}

#[test]
fn insert_group_can_be_closed_and_reopened() {
    let mut h = Harness::new("", 0);
    h.feed("iab");
    h.vim.end_insert_group(&mut h.buf);
    assert!(h.buf.is_modified());
    h.feed("c<Esc>");
    assert_eq!(h.buf.text(), "abc");
    h.feed("u");
    assert_eq!(h.buf.text(), "ab", "the closed group is its own step");
    h.feed("u");
    assert_eq!(h.buf.text(), "");
}

#[test]
fn dd_treats_form_feed_as_text_not_line_break() {
    assert_eq!(run("a\x0cb\nc", 0, "dd").0, "c");
    let h = Harness::new("a\x0cb\nc", 0);
    assert_eq!(h.buf.rope().len_lines(), 2);
}

#[test]
fn huge_put_counts_are_refused() {
    let line = "x".repeat(1000);
    let mut h = Harness::new(&format!("{line}\n"), 0);
    h.feed("yy99999p");
    assert_eq!(h.buf.text(), format!("{line}\n"));
    assert!(matches!(&h.fx[..], [.., VimEffect::Error(e)] if e.contains("too large")));
    h.feed("3p");
    assert_eq!(h.buf.rope().len_lines(), 5);
}

#[test]
fn counted_dot_replaces_the_operator_count() {
    let mut h = Harness::new("a b c d e f g h i\n", 0);
    h.feed("d3w");
    assert_eq!(h.buf.text(), "d e f g h i\n");
    h.feed("2.");
    assert_eq!(h.buf.text(), "f g h i\n");
    h.feed("2d1w.");
    assert_eq!(h.buf.text(), "\n");
}

#[test]
fn insert_tab_uses_the_display_column() {
    let mut h = Harness::new("\tx\n", 1);
    h.feed("i<Tab><Esc>");
    // The caret sits at display column 4 (after the tab): a full tab stop is inserted.
    assert_eq!(h.buf.text(), "\t    x\n");
    let mut h = Harness::new("ab\tx\n", 3);
    h.feed("a<Tab><Esc>");
    // After "ab\tx" the display column is 5: three spaces reach column 8.
    assert_eq!(h.buf.text(), "ab\tx   \n");
}
