//! The small text transforms every output surface had reinvented.
//!
//! Tag-stripping had **seven** copies, entity-unescaping three, whitespace collapse four,
//! data-URI decoding three and HTML escaping three — none of them hard, all of them the
//! kind of ten-line loop that quietly disagrees. Two of the three unescapers ran
//! `&amp;` **first**, which double-unescapes: `&amp;lt;` came back as `<` instead of
//! `&lt;`. One data-URI decoder's own doc comment admitted it was a fork of another.
//!
//! **Where copies stay, they stay as a typed parameter, never as a second function.**
//! [`TagBreak`] is the only real disagreement between the tag-strippers, and it is a real
//! one: see its variants for the input that separates them.
//!
//! **Deliberately not adopted here** (recorded so nobody "fixes" them into this module):
//! - `ocr::doctags::strip_tags` counts *nesting depth* rather than tracking an in-tag flag,
//!   so `<<a>b>` yields `""` there and `"b"` here. It parses the DocTags grammar, not HTML,
//!   and its callers key repetition detection on the result.
//! - `links`'s `/Dests` name decoding stays `from_utf8_lossy`: name-tree keys are byte
//!   strings, not text strings.

use base64::Engine as _;

/// The gap, as a fraction of the type size, at or above which two adjacent spans are
/// separated by a **space** rather than being consecutive glyphs of one word.
///
/// Spans are word-level *at best*: a generator is entitled to emit one `Tj` per glyph, and
/// several do (map labels, SEC filing tables). A typical space is ≈0.25 em and some fonts
/// pack to ≈0.28; intra-word kerning is ≈0. `0.2` clears the former and stays below the
/// latter, so it separates a word break from a glyph break without inventing either.
///
/// Spelled once because four paths draw this same line and had drifted into four copies of
/// the literal: `layout::lines_of` (body prose), `text::extract_page` (plain text),
/// `vector::coalesce_glyph_runs` (figure labels — see `GLYPH_JOIN_GAP`) and
/// `extract::row_cells` (table cells). The last was the copy that did **not** exist: it
/// spaced every glyph pair unconditionally, so a table header drawn glyph-by-glyph came out
/// as `T e x a s`.
pub(crate) const SPACE_GAP: f32 = 0.2;

/// Glyphs may also *overlap* slightly — kerning, an accent, a hand-tracked label. A small
/// negative gap is still one word; a deeply stacked glyph (a struck-through or overprinted
/// mark), or a span that starts far to the LEFT of the previous one (a wrapped line folded
/// into one cell), is not.
pub(crate) const GLYPH_OVERLAP_GAP: f32 = 0.35;

/// Are two adjacent spans consecutive glyphs of **one word**, i.e. is there no space between
/// them? `gap` is the second span's start minus the first's end, both in the same units as
/// `size`.
///
/// The one place this question is answered. It is asked wherever spans are concatenated into
/// text — body lines, plain text, figure labels, table cells — and a generator that emits one
/// `Tj` per glyph is what makes the answer load-bearing rather than cosmetic.
pub(crate) fn glyph_adjacent(gap: f32, size: f32) -> bool {
    gap <= size * SPACE_GAP && gap >= -size * GLYPH_OVERLAP_GAP
}

/// What a tag leaves behind when [`strip_tags`] removes it.
///
/// The three are not interchangeable, and the input that separates them is a **stray `>`
/// in the text** — legal content that no tag opened. On `a>b`:
/// [`Join`](TagBreak::Join) and [`SpaceAtOpen`](TagBreak::SpaceAtOpen) give `"ab"`,
/// [`SpaceAtClose`](TagBreak::SpaceAtClose) gives `"a b"`. That one character decides
/// whether the OCR word-statistics gate sees one word-like token or none, so the choice is
/// spelled at the call site rather than picked here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TagBreak {
    /// Tags vanish: `<b>a</b>b` → `"ab"`. What a *label* wants — the markup was inside a
    /// word, and inserting a break would split it.
    Join,
    /// A space is emitted where each tag **opens**. What *text extraction* wants: a tag
    /// boundary must become a token boundary so `<td>A</td><td>B</td>` reads as `A B` and
    /// not `AB`.
    SpaceAtOpen,
    /// A space is emitted where each tag **closes** — including for a `>` that closed no
    /// tag. Same token-boundary effect as [`SpaceAtOpen`](TagBreak::SpaceAtOpen) on
    /// well-formed markup; differs only on a stray `>`.
    SpaceAtClose,
}

/// The characters of `s` that lie outside `<…>` tags, in order, with `brk` deciding what a
/// tag leaves behind.
///
/// This is the single tag-skipping state machine. It is an iterator so a caller that only
/// needs the *first* visible character does not have to build the whole string.
///
/// Malformed markup degrades rather than failing: an unterminated `<` swallows the rest of
/// the input (a viewer does the same), and a `>` with no open tag is consumed.
pub(crate) fn visible_chars(s: &str, brk: TagBreak) -> impl Iterator<Item = char> + '_ {
    let mut in_tag = false;
    s.chars().filter_map(move |c| match c {
        '<' => {
            in_tag = true;
            (brk == TagBreak::SpaceAtOpen).then_some(' ')
        }
        '>' => {
            in_tag = false;
            (brk == TagBreak::SpaceAtClose).then_some(' ')
        }
        _ if in_tag => None,
        _ => Some(c),
    })
}

/// [`visible_chars`] collected — the plain text of a fragment of inline HTML.
pub(crate) fn strip_tags(s: &str, brk: TagBreak) -> String {
    visible_chars(s, brk).collect()
}

/// Decode the five entities this codebase emits, **in the one safe order**.
///
/// `&amp;` is expanded **last**. Expanding it first is a double-unescape: `&amp;lt;` — the
/// literal text `&lt;`, correctly escaped — becomes `&lt;` after the first pass and then
/// `<` after the second, so escaped markup in a heading turns back into markup. Two of the
/// three copies this replaces had that bug.
pub(crate) fn unescape_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

/// Collapse every run of whitespace to a single space, **keeping** the leading and trailing
/// ones. The caller trims if it wants to; [`normalize_ws`] is the trimming variant.
pub(crate) fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

/// [`collapse_ws`] plus a trim — the `split_whitespace().join(" ")` idiom, spelled once.
pub(crate) fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Escape for **element content** in HTML or SVG: `&`, `<`, `>`.
///
/// A `"` needs no escaping between tags, and adding one would change every `<text>` label
/// an SVG figure emits.
pub(crate) fn esc_text(s: &str) -> String {
    escape_with(s, false)
}

/// Escape for a value that may land in **either** element content or a double-quoted
/// attribute: [`esc_text`]'s set plus `"`. The HTML writer uses this everywhere, so one
/// string is safe wherever it is interpolated.
pub(crate) fn esc(s: &str) -> String {
    escape_with(s, true)
}

/// The one escaping pass. `quote` adds `"` to the set.
fn escape_with(s: &str, quote: bool) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' if quote => o.push_str("&quot;"),
            _ => o.push(c),
        }
    }
    o
}

/// Escape for a **double-quoted attribute value**: `&`, `"`, `<`.
///
/// Deliberately *not* [`esc`]: a `>` inside an attribute value needs no escaping (HTML5
/// §13.1.2.3 — only `&` and the delimiting quote are special there, and `<` is escaped by
/// long convention). Adding it would rewrite the `alt` text of every externalised SVG
/// figure whose caption contains `>` — 12 of the 432 figure captions in the local corpus —
/// for no correctness gain. Kept here rather than in the one file that calls it so the
/// three escaping rules can be read side by side.
pub(crate) fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;")
}

/// Decode a `data:<mime>;base64,…` URI into its raw bytes and a file extension.
///
/// `None` for anything that is not a base64 `data:` URI — a `data:text/plain,hello` has no
/// bytes to decode and returning its payload as if it did is how the weak copy of this
/// (which only looked for the first comma) fabricated content. An unrecognised mime type
/// degrades to `"bin"`, never to a guess.
pub(crate) fn decode_data_uri(uri: &str) -> Option<(Vec<u8>, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if !meta.contains("base64") {
        return None;
    }
    let ext = match meta.split(';').next().unwrap_or("") {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "bin",
    };
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    Some((bytes, ext.to_string()))
}

/// Is this whitespace-delimited token a **word**, for the OCR text-vs-image gate? At least
/// two alphanumeric characters, so specks and stray punctuation cannot inflate the count.
pub(crate) fn is_word_like(token: &str) -> bool {
    token.chars().filter(|c| c.is_alphanumeric()).count() >= 2
}

/// [`is_word_like`] tokens in `s`.
pub(crate) fn word_like_count(s: &str) -> usize {
    s.split_whitespace().filter(|w| is_word_like(w)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_dropped_and_a_break_is_a_choice() {
        assert_eq!(strip_tags("<b>a</b>b", TagBreak::Join), "ab");
        assert_eq!(strip_tags("<td>A</td><td>B</td>", TagBreak::SpaceAtOpen), " A  B ");
        assert_eq!(strip_tags("<td>A</td><td>B</td>", TagBreak::SpaceAtClose), " A  B ");
        // The one input that separates the two break policies, and why both exist.
        assert_eq!(strip_tags("a>b", TagBreak::Join), "ab");
        assert_eq!(strip_tags("a>b", TagBreak::SpaceAtOpen), "ab");
        assert_eq!(strip_tags("a>b", TagBreak::SpaceAtClose), "a b");
    }

    #[test]
    fn malformed_markup_degrades_instead_of_panicking() {
        assert_eq!(strip_tags("a<b", TagBreak::Join), "a", "an unterminated tag swallows the tail");
        assert_eq!(strip_tags("", TagBreak::Join), "");
        assert_eq!(strip_tags("<<a>b>", TagBreak::Join), "b");
    }

    #[test]
    fn first_visible_char_needs_no_allocation() {
        assert_eq!(visible_chars("  <a href=\"x\">Hi", TagBreak::Join).find(|c| !c.is_whitespace()), Some('H'));
        assert_eq!(visible_chars("<a></a>", TagBreak::Join).find(|c| !c.is_whitespace()), None);
    }

    #[test]
    fn amp_is_unescaped_last_so_escaped_markup_stays_escaped() {
        // The bug two of the three copies shipped: `&amp;` first turns `&amp;lt;` into
        // `&lt;` and then into `<`, so text that was correctly escaped becomes markup.
        assert_eq!(unescape_entities("&amp;lt;"), "&lt;");
        assert_eq!(unescape_entities("&lt;b&gt;"), "<b>");
        assert_eq!(unescape_entities("a &amp; b"), "a & b");
        assert_eq!(unescape_entities("&quot;q&quot; &#39;s&#39;"), "\"q\" 's'");
        assert_eq!(unescape_entities("no entities"), "no entities");
        // What the buggy order produced, kept as the counter-example:
        let buggy = "&amp;lt;".replace("&amp;", "&").replace("&lt;", "<");
        assert_eq!(buggy, "<");
        assert_ne!(unescape_entities("&amp;lt;"), buggy);
    }

    #[test]
    fn whitespace_collapse_keeps_or_drops_the_edges_by_name() {
        assert_eq!(collapse_ws("  a \t\n b  "), " a b ");
        assert_eq!(normalize_ws("  a \t\n b  "), "a b");
        assert_eq!(collapse_ws(""), "");
        assert_eq!(normalize_ws("   "), "");
    }

    #[test]
    fn the_three_escapes_differ_only_where_their_context_does() {
        assert_eq!(esc_text("a<b>&\"c"), "a&lt;b&gt;&amp;\"c");
        assert_eq!(esc("a<b>&\"c"), "a&lt;b&gt;&amp;&quot;c");
        assert_eq!(esc_attr("a<b>&\"c"), "a&lt;b>&amp;&quot;c");
        // Each set is sufficient for its context: nothing that could terminate the
        // construct survives.
        assert!(!esc_text("<>&").contains(['<', '>']));
        assert!(!esc_attr("\"<&").contains('"'));
    }

    #[test]
    fn a_data_uri_decodes_only_when_it_really_is_one() {
        let (b, ext) = decode_data_uri("data:image/png;base64,QUJD").expect("png");
        assert_eq!((b.as_slice(), ext.as_str()), (b"ABC".as_slice(), "png"));
        assert_eq!(decode_data_uri("data:image/svg+xml;base64,QUJD").unwrap().1, "svg");
        assert_eq!(decode_data_uri("data:application/x-thing;base64,QUJD").unwrap().1, "bin");
        // The weak copy took everything after the first comma and decoded it blind.
        assert!(decode_data_uri("data:text/plain,hello").is_none(), "not base64: no bytes to hand back");
        assert!(decode_data_uri("https://example.com/a.png").is_none());
        assert!(decode_data_uri("data:image/png;base64,!!!not-base64!!!").is_none());
    }

    #[test]
    fn a_word_needs_two_alphanumerics() {
        assert!(is_word_like("ab"));
        assert!(is_word_like("a1"));
        assert!(!is_word_like("a"));
        assert!(!is_word_like("..."));
        assert!(!is_word_like("-"));
        assert_eq!(word_like_count("the . a quick , brown"), 3);
    }

    /// The two OCR stat helpers used to spell this loop out; the rewire must not move the
    /// numbers, so the pre-move implementation is kept here as the oracle.
    #[test]
    fn the_ocr_word_stats_are_unchanged_by_the_rewire() {
        fn old(doctags: &str) -> (usize, usize) {
            let mut text = String::with_capacity(doctags.len());
            let mut in_tag = false;
            for c in doctags.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => {
                        in_tag = false;
                        text.push(' ');
                    }
                    _ if !in_tag => text.push(c),
                    _ => {}
                }
            }
            let words = text
                .split_whitespace()
                .filter(|w| w.chars().filter(|c| c.is_alphanumeric()).count() >= 2)
                .count();
            let chars = text.chars().filter(|c| !c.is_whitespace()).count();
            (words, chars)
        }
        fn new(doctags: &str) -> (usize, usize) {
            let text = strip_tags(doctags, TagBreak::SpaceAtClose);
            (word_like_count(&text), text.chars().filter(|c| !c.is_whitespace()).count())
        }
        for s in [
            "<loc_50><loc_30><loc_450><loc_60>COMISSÃO SINDICANTE",
            "<text>a>b</text>",
            "<otsl><fcel>1<fcel>22</otsl>",
            "",
            ">>>",
            "no tags at all, just words",
            "<unclosed",
        ] {
            assert_eq!(old(s), new(s), "stats moved for {s:?}");
        }
    }
}
