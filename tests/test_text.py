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
