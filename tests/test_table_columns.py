"""HTML-level locks for the two-pass 'table profile' column model (src/extract.rs).

test_tables.py scores detection/column-count against ground truth; these tests pin the
finer STRUCTURAL guarantees of the whitespace-lane column model on the deterministic
locking fixtures (tests/gen_tables.py → tbl_lock_*.pdf), so a future refactor that
re-introduces the old failures (sparse columns collapsing, right-aligned numerics
merging, prose admitted once the keep rule is relaxed) fails CI.

Each assertion would FAIL on the pre-band baseline:
  - sparse_multicol collapsed to ~2 columns (the ≥50%-occupancy filter dropped the
    header-named sparse columns);
  - right_aligned packed mixed-width values into one cell (left-x clustering scattered
    the right-aligned column);
  - the prose glossary was at risk of admission once `named`-keep relaxed the filter —
    is_coherent_grid (decoupled from column-keeping) must still reject it.
"""
import os
import re

import distillpdf
import pytest

HERE = os.path.dirname(os.path.abspath(__file__))
GTDIR = os.path.join(HERE, "corpus_tables")


def _tables(path):
    html = distillpdf.Pdf.open(os.path.join(GTDIR, path)).to_html(return_string=True)
    out = []
    for t in re.findall(r"<table\b.*?</table>", html, re.DOTALL):
        rows = []
        for tr in re.findall(r"<tr\b.*?</tr>", t, re.DOTALL):
            cells = [re.sub(r"<[^>]+>", "", c).strip()
                     for c in re.findall(r"<t[dh]\b[^>]*>(.*?)</t[dh]>", tr, re.DOTALL)]
            rows.append(cells)
        out.append(rows)
    return out


def _max_cols(rows):
    return max((len(r) for r in rows), default=0)


def test_sparse_multicol_keeps_all_columns():
    """A wide table whose header names 10 columns but whose body rows each fill only
    1-2 cells must keep ~10 columns — not collapse into a couple of dense ones."""
    tabs = _tables("tbl_lock_sparse_multicol.pdf")
    assert len(tabs) == 1, f"expected exactly one table, got {len(tabs)}"
    rows = tabs[0]
    ncols = _max_cols(rows)
    assert ncols >= 9, f"sparse columns collapsed: only {ncols} columns (want ~10)"
    # No body cell should pack two of the (space-separated numeric) values together:
    # a collapse folds a sparse column's value into a neighbour ("11 7" in one cell).
    for r in rows:
        for cell in r:
            nums = re.findall(r"\d+", cell)
            assert len(nums) <= 1, f"two values packed into one cell: {cell!r}"


def test_right_aligned_numeric_stable_columns():
    """A right-aligned numeric column with mixed-width values (1–7 digits) must stay a
    single column with each value in its own cell — not split or merge by left edge."""
    tabs = _tables("tbl_lock_right_aligned.pdf")
    assert len(tabs) == 1, f"expected exactly one table, got {len(tabs)}"
    rows = tabs[0]
    assert _max_cols(rows) == 3, f"expected 3 stable columns, got {_max_cols(rows)}"
    # Every wide value must survive intact in its own cell (not merged with a neighbour
    # nor split across cells).
    flat = [c for r in rows for c in r]
    for want in ("890,123", "56,789", "12,000", "1,234"):
        assert any(c.strip() == want for c in flat), f"{want!r} not isolated in its own cell: {flat}"


def test_prose_glossary_not_a_table():
    """A two-column glossary (short term + long wrapped definition) reaches the column
    model as ≥2-cell rows, but admission (is_coherent_grid) must reject it as prose —
    the relaxed `named` keep rule must not be able to admit it (decoupling)."""
    tabs = _tables("tbl_lock_prose_not_table.pdf")
    assert tabs == [], f"prose glossary wrongly admitted as {len(tabs)} table(s)"


def test_equation_not_a_table():
    """A display equation positioned into aligned columns reaches the richer band
    column model, but admission (is_coherent_grid) must still reject it via the relation
    (=) rule — locks that the column-model change can't let a positioned equation slip
    past the equation guard. (The ≤/≥ inequality variant is locked by the corpus
    math_as_table gate on the real math_PR PDF; base-14 fonts can't encode it here.)"""
    tabs = _tables("tbl_lock_equation.pdf")
    assert tabs == [], f"equation wrongly admitted as {len(tabs)} table(s)"


@pytest.mark.parametrize("fname", [
    "tbl_lock_sparse_multicol.pdf",
    "tbl_lock_right_aligned.pdf",
])
def test_column_model_deterministic(fname):
    """Column reconstruction must be deterministic (the event-sweep lane detection has
    no RNG / float-order hazard) — extract twice, identical HTML."""
    p = os.path.join(GTDIR, fname)
    a = distillpdf.Pdf.open(p).to_html(return_string=True)
    b = distillpdf.Pdf.open(p).to_html(return_string=True)
    assert a == b, f"{fname}: non-deterministic table output"
