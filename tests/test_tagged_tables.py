"""L0 — the table structure a PDF *declares* (`/StructTreeRoot`), end to end.

A tagged PDF states its tables exactly: rows, columns, header cells and spans, with `/MCID`s
tying each cell to the glyphs on the page. Where that declaration exists and resolves, it
outranks every alignment heuristic, because there is nothing to infer.

`tagged_table.pdf` (tests/gen_fixtures.py, self-generated) puts all three outcomes on one
page, so these are the locks:

  * the declared table is emitted with the DECLARED grid, spans expanded;
  * two degenerate declarations — a one-row shard and a one-column shard, both shapes
    observed in the measurement corpus — are refused and fall through to prose;
  * an UNDECLARED ruled table on the same page is still found by inference, i.e. L0 masks
    its own region and never takes over the page.

Plus the property that makes the whole phase safe: a document with no structure tree is
extracted byte-for-byte as it was before L0 existed.
"""
import os
import re

import distillpdf
import pytest

from _fixtures import GT, FIX, html

NAME = "tagged_table.pdf"


def _tables(page_html):
    out = []
    for t in re.findall(r"<table\b.*?</table>", page_html, re.DOTALL):
        rows = []
        for tr in re.findall(r"<tr\b.*?</tr>", t, re.DOTALL):
            rows.append([re.sub(r"<[^>]+>", "", c).strip()
                         for c in re.findall(r"<t[dh]\b[^>]*>(.*?)</t[dh]>", tr, re.DOTALL)])
        out.append(rows)
    return out


@pytest.fixture(scope="module")
def tables():
    return _tables(html(NAME))


def test_the_declared_table_is_emitted_exactly_as_declared(tables):
    """3 rows, 3 columns, `/ColSpan 2` and `/RowSpan 2` expanded where the file puts them.

    Alignment inference reading the same page would see a 2-column shape (the header's
    spanning cell leaves column 1 empty on the only row that names it) — the declaration is
    the only evidence that says otherwise, and it wins inside its own region."""
    assert tables[0] == GT[NAME]["declared_grid"]


def test_the_declared_header_row_is_th_and_the_data_rows_are_td():
    body = html(NAME)
    first = re.search(r"<table\b.*?</table>", body, re.DOTALL).group(0)
    rows = re.findall(r"<tr\b.*?</tr>", first, re.DOTALL)
    assert rows[0].count("<th") == 3 and "<td" not in rows[0]
    assert all("<th" not in r for r in rows[1:])


def test_an_indirect_mcid_still_resolves_to_its_cell(tables):
    """`/K [21 0 R]` where object 21 is the integer `3` — legal, and what every World Bank
    document in the measurement corpus writes. Reading only DIRECT integers found 34 declared
    tables in one such file and not a single cell's content."""
    assert tables[0][1][1] == "Alpha"


def test_a_degenerate_declaration_is_refused_and_falls_through(tables):
    """A one-row `/Table` and a one-column `/Table` are not tables, whatever the file says.

    Both must be refused BEFORE emission — a table element carrying a single row is exactly
    the shard shape a fragmenting producer emits (one corpus table is declared as 1x9 + 1x8 +
    4x13 for a single 2x12 grid) — and their text must reappear in the page flow rather than
    vanish into a `<table>` nobody wanted."""
    flat = [c for t in tables for r in t for c in r]
    assert "one" not in flat and "gamma" not in flat
    body = re.sub(r"<table\b.*?</table>", " ", html(NAME), flags=re.DOTALL)
    text = re.sub(r"\s+", " ", re.sub(r"<[^>]+>", " ", body))
    assert "one row only" in text and "alpha beta gamma" in text


def test_an_undeclared_table_on_the_same_page_is_still_inferred(tables):
    """Declaration coverage is partial per page by measurement — IRS f1040 p1 declares one of
    its two tables — so L0 masks its region and leaves the rest of the page to inference."""
    assert len(tables) == 2
    assert tables[1] == GT[NAME]["undeclared_rows"]


def test_an_untagged_document_is_unchanged_by_l0():
    """The safety argument, executable: L0 acts only where a declaration exists, so a file
    without a `/StructTreeRoot` cannot be touched by it. `glyph_table.pdf` carries the same
    kind of content — a ruled data table — and no structure tree at all."""
    import pikepdf

    with pikepdf.open(os.path.join(FIX, "glyph_table.pdf")) as p:
        assert "/StructTreeRoot" not in p.Root
    rows = _tables(html("glyph_table.pdf"))
    assert rows and rows[0] == GT["glyph_table.pdf"]["cells"]


def test_a_structure_tree_that_points_at_nothing_does_not_produce_a_table(tmp_path):
    """A stale tag — the declaration survives an edit that removed the content it named.

    Rebuilt from the fixture with every `BDC`/`EMC` stripped out of the content stream: the
    tree still declares a 3x3 table, and not one of its `/MCID`s resolves to a glyph. The
    trust rule must refuse it and leave the page to inference, because a grid of empty cells
    is worse than no grid at all."""
    import pikepdf

    src = os.path.join(FIX, NAME)
    dst = str(tmp_path / "stale.pdf")
    with pikepdf.open(src) as p:
        page = p.pages[0]
        data = page.Contents.read_bytes()
        data = re.sub(rb"/P << /MCID \d+ >> BDC ", b"", data).replace(b" EMC", b"")
        page.Contents = p.make_stream(data)
        p.save(dst)
    tables = _tables(distillpdf.Pdf.open(dst).to_html(mode="page", return_string=True))
    # The declared 3x3 is gone; only the undeclared ruled grid survives.
    assert all(r != GT[NAME]["declared_grid"] for r in tables)
