#!/usr/bin/env python3
"""Per-document quality scores the gate freezes and compares.

Nothing is redefined here. `recall`, `jaccard` and `order` are benchmarking/score_quality.py's
definitions against PyMuPDF; `uri_recall`, `internal_recall` and `internal_resolved` are
corpus_tests/test_links.py's; `gt_*` come straight from bench100_score.py's blind-judge page
scores; table agreement is bench100_score.table_score. What is added is the per-document grain,
because those tools report a corpus mean and a mean can hide one document falling off a cliff.

`order` is computed per page and pooled (sum 2M / sum T), not over the joined document:
difflib with autojunk off is quadratic in a token's frequency, and the 383-page document would
cost minutes for every changed run. Pooling keeps the same ratio definition and a column-join
on one page still moves it.
"""
from __future__ import annotations

import difflib
import gzip
import hashlib
import html as htmllib
import json
import os
import re

WORD = re.compile(r"\w+", re.UNICODE)
_TAGS = re.compile(r"<[^>]+>")
_DROP = re.compile(r"<(style|script|svg|nav|head)\b.*?</\1>", re.S | re.I)
_HEADING = re.compile(r"<h[1-6]\b[^>]*>(.*?)</h[1-6]>", re.S)


def norm_words(text: str) -> list[str]:
    """score_quality.py's tokenizer."""
    return [w.lower() for w in WORD.findall(text)]


def overlap(ref: set, got: set) -> tuple[float | None, float | None]:
    """(recall, jaccard) on word sets, as in score_quality.py. None when the reference has no
    text at all (a scan): there is nothing to recall, and 0.0 would read as a failure."""
    if not ref:
        return None, None
    return len(ref & got) / len(ref), len(ref & got) / len(ref | got)


def pooled_order(ref_pages: list[list[str]], got_pages: list[list[str]]) -> float | None:
    """Token-level difflib ratio with autojunk off, pooled over pages. See the module docstring."""
    matched = total = 0
    for i in range(max(len(ref_pages), len(got_pages))):
        ref = ref_pages[i] if i < len(ref_pages) else []
        got = got_pages[i] if i < len(got_pages) else []
        total += len(ref) + len(got)
        if ref and got:
            sm = difflib.SequenceMatcher(None, ref, got, autojunk=False)
            matched += sum(b.size for b in sm.get_matching_blocks())
    if not any(ref_pages):
        return None
    return 2.0 * matched / total if total else None


def html_text(html: str) -> str:
    return htmllib.unescape(_TAGS.sub(" ", _DROP.sub(" ", html)))


def heading_hashes(body: str) -> list[str]:
    """Short hashes of the normalized heading texts, in order. Hashes, so the committed baseline
    carries no text from a third-party document."""
    out = []
    for m in _HEADING.finditer(body):
        text = " ".join(norm_words(htmllib.unescape(_TAGS.sub(" ", m.group(1)))))
        out.append(hashlib.sha256(text.encode()).hexdigest()[:6])
    return out


def load_reference(path: str) -> dict:
    with gzip.open(path, "rt", encoding="utf-8", errors="surrogatepass") as f:
        return json.load(f)


def score_document(out_dir: str, ref: dict, grids_of, body_of) -> dict:
    """Every score for one engine's outputs of one document. `grids_of` and `body_of` are
    bench100_score's own HTML readers, passed in so this module imports without PyMuPDF."""
    ref_pages = [norm_words(p) for p in ref["pages"]]
    ref_set = {w for p in ref_pages for w in p}
    s: dict = {}

    def read(name):
        p = os.path.join(out_dir, name)
        if not os.path.exists(p):
            return None
        with open(p, encoding="utf-8", errors="surrogatepass") as f:
            return f.read()

    text = read("text")
    if text is not None:
        s["recall"], s["jaccard"] = overlap(ref_set, set(norm_words(text)))
    pages = read("pages")
    if pages is not None:
        got_pages = [norm_words(p) for p in json.loads(pages)]
        s["order"] = pooled_order(ref_pages, got_pages)
        s["pages_recall"] = overlap(ref_set, {w for p in got_pages for w in p})[0]
    md = read("markdown")
    if md is not None:
        s["md_recall"] = overlap(ref_set, set(norm_words(md)))[0]
    html = read("html")
    if html is not None:
        s["html_recall"] = overlap(ref_set, set(norm_words(html_text(html))))[0]
        body = body_of(html)
        s["headings"] = heading_hashes(body)
        s["grids"] = grids_of(body)
        s["n_links"] = len(re.findall(r"<a href=", html))
        s["n_images"] = len(re.findall(r"<img\b", html)) + len(re.findall(r"<svg\b", html))
        s["n_lists"] = len(re.findall(r"<[ou]l\b", body))
        s["n_paragraphs"] = len(re.findall(r"<p\b", body))
    links = read("links")
    if links is not None and (ref["uris"] or ref["internal"]):
        rows = json.loads(links)
        ours = {(r["page"], r["uri"]) for r in rows if r["kind"] == "uri" and r["uri"]}
        internal = [r for r in rows if r["kind"] == "internal"]
        want = {(p, u) for p, u in ref["uris"]}
        s["uri_recall"] = len(want & ours) / len(want) if want else 1.0
        s["internal_recall"] = min(1.0, len(internal) / ref["internal"]) if ref["internal"] else 1.0
        s["internal_resolved"] = (sum(1 for r in internal if r["dest_page"]) / len(internal)
                                  if internal else 1.0)
    return {k: (round(v, 4) if isinstance(v, float) else v) for k, v in s.items()}
