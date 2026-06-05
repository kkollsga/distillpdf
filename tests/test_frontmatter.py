#!/usr/bin/env python3
"""Front-matter extraction on an owned academic-paper fixture (fixtures_pdf/frontmatter.pdf).

Covers the paper-specific path: pdf.metadata() returns the title, authors linked to their
organisation, the abstract and keywords; and to_html() emits a semantic <header> block
(title/authors/affiliations/abstract/keywords) while the numbered body section stays in
the body and the e-mail line is dropped. Regenerate with `python tests/gen_fixtures.py`.
"""
import json
import os
import re

import distillpdf
import _htmlcheck as hc

HERE = os.path.dirname(os.path.abspath(__file__))
FIX = os.path.join(HERE, "fixtures_pdf")
PDF = os.path.join(FIX, "frontmatter.pdf")
with open(os.path.join(FIX, "groundtruth.json")) as _f:
    GT = json.load(_f)["frontmatter.pdf"]


def _meta():
    return distillpdf.Pdf.open(PDF).metadata()


def _header(mode="section"):
    html = distillpdf.Pdf.open(PDF).to_html(mode=mode, return_string=True)
    m = re.search(r"<header>.*?</header>", html, re.DOTALL)
    return html, (m.group(0) if m else None)


# ------------------------------------------------------------------- metadata() dict
def test_metadata_title():
    assert _meta()["title"] == GT["title"]


def test_metadata_authors_linked_to_orgs():
    authors = _meta()["authors"]
    assert [a["name"] for a in authors] == [a["name"] for a in GT["authors"]]
    for got, want in zip(authors, GT["authors"]):
        assert got["affiliation"] and want["affiliation_contains"] in got["affiliation"], got


def test_metadata_abstract_and_keywords():
    m = _meta()
    assert m["abstract"] and GT["abstract_contains"] in m["abstract"]
    assert m["keywords"] == GT["keywords"]


def test_metadata_excludes_email():
    m = _meta()
    blob = json.dumps(m)
    assert GT["email_fragment"] not in blob


# --------------------------------------------------------------------- <header> HTML
def test_header_block_emitted():
    for mode in ("section", "page"):
        html, header = _header(mode)
        assert header, f"no <header> in {mode} mode"
        assert html.count("<header>") == 1 and html.count("</header>") == 1
        assert f"<h1>{GT['title']}</h1>" in header
        # class-free, semantic markup: title <h1>, authors <p> with <sup>, affiliations <ol>.
        assert "class=" not in header
        assert "<ol>" in header and "<li>University of Example" in header


def test_abstract_and_keywords_are_h2_sections():
    # scientific-paper front matter: Abstract/Keywords become first-class <h2> headings
    # AFTER the header (so they land in the section/TOC machinery), not buried inside it.
    # (page mode adds an id= attribute to headings, so match with a tolerant regex.)
    for mode in ("section", "page"):
        html, header = _header(mode)
        for label in ("Abstract", "Keywords"):
            assert re.search(rf"<h2[^>]*>{label}</h2>", html), f"{label} not an <h2> in {mode}"
            assert re.search(rf"<h2[^>]*>{label}</h2>", header) is None, f"{label} should be after <header>"
        assert GT["abstract_contains"] in hc.plain(html)
        for kw in GT["keywords"]:
            assert kw in hc.plain(html)


def test_header_authors_have_sup_markers():
    _, header = _header()
    # each author's affiliation marker is a superscript; two distinct orgs → 1 and 2
    assert header.count("<sup>") == len(GT["authors"])
    assert "<sup>1</sup>" in header and "<sup>2</sup>" in header


def test_body_section_stays_in_body():
    html, header = _header("section")
    # the numbered section is a real body heading, not swallowed into the header
    assert "1 Introduction" not in header
    assert re.search(r"<h[1-6][^>]*>\s*(<b>)?1 Introduction", html)
    assert GT["body_contains"] in hc.plain(html)


def test_email_not_in_html_header():
    _, header = _header()
    assert GT["email_fragment"] not in header


def test_section_abstract_resolves():
    sec = distillpdf.Pdf.open(PDF).section("abstract")
    assert sec and GT["abstract_contains"] in sec


def test_header_is_well_formed():
    for mode in ("section", "page"):
        html, _ = _header(mode)
        ok, errs = hc.well_formed(html)
        assert ok, errs
