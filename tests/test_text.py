"""Reading order and word-span granularity on owned fixtures.

twocol.pdf  — a two-column page must be read column-by-column (the whole left column
              before the right), the paragraphs must survive intact, and it must NOT be
              mistaken for a table.
numeric.pdf — every span is word-granular (no span packs more than one numeric token,
              the property that makes numeric table cells recoverable) and decimals are
              never mangled into a ' : ' colon form.
"""
import re

from _fixtures import GT, doc, html, text, cells

NUM = re.compile(r"^[-+]?\d[\d.,/%]*$")


def test_twocol_reading_order():
    t = text(html("twocol.pdf"))
    order = GT["twocol.pdf"]["order"]
    pos = [t.find(k) for k in order]
    assert all(p >= 0 for p in pos), f"a column marker is missing: {list(zip(order, pos))}"
    assert pos == sorted(pos), f"two-column reading order scrambled: {list(zip(order, pos))}"


def test_twocol_not_a_table():
    assert "<table" not in html("twocol.pdf"), "two-column prose mis-detected as a table"


def test_twocol_tight_gutter_reads_in_order():
    """A tight Times-Roman two-column page with a centered page number in the gutter must read
    column-by-column. Locks the Standard-14 AFM widths + crossing-tolerant gutter together
    (pre-fix the columns interleave L/R line-by-line)."""
    t = text(html("twocol_tight.pdf"))
    order = GT["twocol_tight.pdf"]["order"]
    pos = [t.find(k) for k in order]
    assert all(p >= 0 for p in pos), f"a column marker is missing: {list(zip(order, pos))}"
    assert pos == sorted(pos), f"tight two-column reading order scrambled: {list(zip(order, pos))}"


def test_twocol_fullwidth_header_reads_header_then_columns():
    """A full-width title/abstract above a two-column body must read header → entire left
    column → right column. Locks the full-width horizontal peel: the header spans the gutter,
    so it must be cut off above the body before the vertical column cut (pre-fix the page
    falls to a single band and the columns interleave L/R line-by-line)."""
    t = text(html("twocol_fullwidth.pdf"))
    order = GT["twocol_fullwidth.pdf"]["order"]
    pos = [t.find(k) for k in order]
    assert all(p >= 0 for p in pos), f"a marker is missing: {list(zip(order, pos))}"
    assert pos == sorted(pos), f"full-width-header reading order scrambled: {list(zip(order, pos))}"


def test_yflip_reading_order_not_reversed():
    """A page under a Y-flip CTM (top-left origin) must read top-to-bottom, not reversed.
    Locks the SEC-filing global bottom-to-top reversal fix (extract_spans device coords)."""
    t = text(html("yflip.pdf"))
    order = GT["yflip.pdf"]["order"]
    pos = [t.find(k) for k in order]
    assert all(p >= 0 for p in pos), f"a Y-flip marker is missing: {list(zip(order, pos))}"
    assert pos == sorted(pos), f"Y-flip page read bottom-to-top (reversed): {list(zip(order, pos))}"


def test_twocol_paragraphs_intact():
    t = text(html("twocol.pdf"))
    # each column paragraph's distinctive opening must appear contiguously (no fusion)
    for frag in ["Reading order in a two-column layout must follow each column",
                 "right column is read only after the entire left column"]:
        assert frag in t, f"paragraph text scrambled or lost: {frag!r}"


def test_word_spans_one_numeric_token_each():
    """No span may pack >1 space-separated numeric token (else table cells fuse)."""
    d = doc("numeric.pdf")
    worst = 0
    for pg in range(1, d.page_count() + 1):
        for s, *_ in d._dbg_spans(pg):
            worst = max(worst, sum(1 for tok in s.split() if NUM.match(tok)))
    assert worst <= 1, f"a span packs {worst} numeric tokens (cells would fuse)"


def test_numeric_table_decimals_intact():
    h = html("numeric.pdf")
    td = cells(h, "td")
    for dec in GT["numeric.pdf"]["decimals"]:
        assert any(dec in c for c in td), f"decimal value missing from table: {dec!r}"
    # a decimal must never render as ' : ' between digits
    tbl = " ".join(re.findall(r"<table\b.*?</table>", h, re.DOTALL))
    assert not re.search(r"\d\s:\s\d", text(tbl)), "decimal rendered as ' : ' (glyph-map defect)"


def test_identity_cid_font_gaps_keep_their_symbols():
    """A Type0 font whose CIDs are Unicode code points ships a machine-generated identity
    ToUnicode that stops short of the newer blocks. A code in one of those gaps still carries
    its text; it used to be extracted as a space (`unicode_showcase.pdf` p2 lost ⬟⬢⬡ ⟶⟵⟷).
    Both the covered symbols and the uncovered ones must survive, through the Python API."""
    gt = GT["identity_cid_font.pdf"]
    d = doc("identity_cid_font.pdf")
    t = d.extract_page_text(1)
    for sym in gt["symbols"]:
        assert sym in t, f"symbol {sym!r} lost from extracted text: {t!r}"
    for line in gt["lines"]:
        assert line in t, f"line not extracted whole: {line!r} not in {t!r}"
    # and through the HTML render, not only the raw text extractor
    h = text(html("identity_cid_font.pdf"))
    for sym in gt["uncovered"]:
        assert sym in h, f"symbol {sym!r} lost from HTML: {h!r}"


def test_subset_cid_font_does_not_invent_text_from_glyph_indices():
    """The guard on the fix above: the same fixture's SUBSET font (CIDs are glyph indices,
    non-identity ToUnicode) shows an unmapped CID 0x41. It must stay dropped — falling back to
    the code there would print an 'A' that the page never contained."""
    gt = GT["identity_cid_font.pdf"]
    t = doc("identity_cid_font.pdf").extract_page_text(1)
    assert gt["subset_text"] in t, f"subset font text lost: {t!r}"
    assert "A" not in t, f"an unmapped glyph index was invented as text: {t!r}"


def test_partial_tounicode_recovers_dropped_letters():
    """A simple font's ToUnicode is routinely *incomplete* — Distiller emits one `bfchar`
    per subsetted glyph and omits the rest. An omitted code used to decode to nothing, so
    letters vanished mid-word ("Redding" → "edding", "EXPLANATION" → "E P A ATIO" on the
    USGS map sheets). The font's declared `/Encoding` carries exactly that information."""
    gt = GT["partial_tounicode.pdf"]
    t = doc("partial_tounicode.pdf").extract_page_text(1)
    for word in gt["winansi_words"]:
        assert word in t, f"word lost to a ToUnicode hole: {word!r} not in {t!r}"
    assert gt["winansi_line"] in t, f"line not extracted whole: {t!r}"
    # …and through the HTML render, not only the raw text extractor.
    assert gt["winansi_line"] in text(html("partial_tounicode.pdf"))


def test_symbolic_font_without_encoding_never_becomes_latin():
    """The guard on the fix above, and the one that matters more.

    A symbolic font with **no** `/Encoding` is read through its font program's built-in
    cmap, whose codes a subsetter re-packs arbitrarily. Falling back to a Latin table there
    prints `a` for θ and `^` for π — confident nonsense, strictly worse than a gap, and
    unrecoverable downstream. The unmapped codes must stay dropped."""
    gt = GT["partial_tounicode.pdf"]
    t = doc("partial_tounicode.pdf").extract_page_text(2)
    for sym in gt["symbolic_mapped"]:
        assert sym in t, f"a mapped symbol was lost: {sym!r} not in {t!r}"
    for bad in gt["symbolic_forbidden"]:
        assert bad not in t, f"an unmapped symbolic code was invented as {bad!r}: {t!r}"
    assert not any(c.isalpha() for c in t), f"Latin substituted for a symbol font: {t!r}"


def test_macroman_font_high_bytes_are_not_latin1():
    """A font declaring `/MacRomanEncoding` and no ToUnicode: its high bytes are MacRoman,
    so `A5 C9 D0 D5` is `• … – ’`. Read as raw Latin-1 they came out `¥ É Ð Õ` — the
    mojibake that littered a real arXiv paper's quotes, dashes and degree signs."""
    gt = GT["partial_tounicode.pdf"]
    t = doc("partial_tounicode.pdf").extract_page_text(3)
    assert gt["macroman_text"] in t, f"MacRoman high bytes mis-decoded: {t!r}"
    for bad in gt["macroman_forbidden"]:
        assert bad not in t, f"MacRoman byte read as Latin-1 {bad!r}: {t!r}"


def test_unfiltered_form_text_survives():
    """A Form XObject whose stream carries NO /Filter must still yield its glyphs.

    lopdf errors on `decompressed_content()` for an unfiltered stream, and the text walker
    took `.unwrap_or_default()` — so every glyph drawn inside such a form decoded as
    nothing. The page-level text was never affected, which is what made the loss silent."""
    gt = GT["unfiltered_form.pdf"]
    t = doc("unfiltered_form.pdf").extract_page_text(1)
    assert gt["form_text"] in t, f"text drawn inside the unfiltered form is lost: {t!r}"
    assert gt["title"] in t, f"page-level text lost: {t!r}"
    # …and through the HTML render, not only the raw text extractor.
    assert gt["form_text"] in text(html("unfiltered_form.pdf"))
