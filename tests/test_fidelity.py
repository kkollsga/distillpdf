"""Structural-fidelity guards over the owned fixture set — the same 16 defect checks the
local corpus suite runs, here applied to data we own so they gate CI. Each check scans
every owned document's HTML for the signature of a specific defect (duplicated tokens,
prose-as-table, decoupled captions, decimal-as-colon, page-number noise, reading-order
fusion, math-doc with flattened sub/superscripts, …) and must report 0 hits."""
import os

import pytest

import _htmlcheck as hc

OWNED = hc.doc_pdfs()


@pytest.fixture(scope="module")
def htmls():
    return {os.path.basename(p): hc.html_for(p) for p in OWNED}


@pytest.mark.parametrize("key,fn", hc.CHECKS, ids=[k for k, _ in hc.CHECKS])
def test_fidelity_check(key, fn, htmls):
    hits, examples = 0, []
    for name, html in htmls.items():
        h, ex = fn(name, html, hc.plain(html))
        hits += h
        examples += ex
    assert hits == 0, f"{key}: {hits} structural defect(s): {examples[:5]}"
