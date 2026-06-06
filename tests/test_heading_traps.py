"""Locks for the content-based heading rejection filters (src/html.rs detect_header).

Each trap in heading_traps.pdf is drawn in the HEADING face (bold, larger than body) yet
must NOT be promoted to a heading, because its CONTENT disqualifies it:
  - "x = y + z"                      → equation fragment (relation / no real words)
  - "a b c d e f"                    → single-character symbol soup
  - "The ATLAS Collaboration"        → author / collaboration byline
  - "The result was set during the run" → a clause, not a section name

A genuine canonical section ("Introduction") in the SAME face must still be a heading —
so the filters discriminate, not blanket-reject heading-styled lines. Each assertion
would fail on the pre-filter baseline (these were promoted to <h1>/<h2>/<h4>).
"""
import os
import re

from _fixtures import GT, html, text

NAME = "heading_traps.pdf"
G = GT[NAME]


def _heading_texts(h):
    return [re.sub(r"\s+", " ", re.sub(r"<[^>]+>", "", m.group(1))).strip()
            for m in re.finditer(r"<h[1-6]\b[^>]*>(.*?)</h[1-6]>", h, re.DOTALL)]


def test_real_heading_kept():
    """The genuine section head in the heading face is still emitted as <h2>."""
    hs = _heading_texts(html(NAME))
    assert any("Introduction" in t for t in hs), f"real heading dropped; headings={hs}"


def test_content_traps_not_promoted():
    """No heading-styled-but-content-disqualified line becomes a heading; each stays in
    the body text."""
    h = html(NAME)
    hs = _heading_texts(h)
    body = text(h)
    for trap in G["not_headings"]:
        assert not any(trap[:18] in t for t in hs), f"trap promoted to heading: {trap!r} (headings={hs})"
        assert trap[:18] in body, f"trap text lost from body: {trap!r}"
