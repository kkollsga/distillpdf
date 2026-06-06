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
