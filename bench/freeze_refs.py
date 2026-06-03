#!/usr/bin/env python3
"""Freeze PyMuPDF-derived ground truth to on-disk fixtures so the goallines run
with ONLY distillpdf installed (no pymupdf at test time). Run this once with an env
that has pymupdf, after changing the corpus; commit the fixtures/ dir.

Writes (committed, stable — NOT in the wipeable out/ cache):
  fixtures/<name>.pymupdf_plain.txt  raw MuPDF text reference (text/html/order tests)
  fixtures/ref_meta.json             {name: {pages, link_uris:[[page,uri]], link_internal:int}}
"""
import glob, json, os
import pymupdf

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "fixtures")
os.makedirs(OUT, exist_ok=True)
CORPUS = sorted(glob.glob(os.path.join(HERE, "corpus", "*.pdf"))
                + glob.glob(os.path.join(HERE, "corpus_headers", "*.pdf")))

meta = {}
for path in CORPUS:
    name = os.path.splitext(os.path.basename(path))[0]
    d = pymupdf.open(path)
    open(os.path.join(OUT, f"{name}.pymupdf_plain.txt"), "w").write("".join(p.get_text() for p in d))
    uris, internal = [], 0
    for pno in range(d.page_count):
        for lk in d[pno].get_links():
            if lk.get("uri"):
                uris.append([pno + 1, lk["uri"]])
            elif lk.get("kind") in (1, 4, 5):
                internal += 1
    meta[os.path.basename(path)] = {"pages": d.page_count, "link_uris": uris, "link_internal": internal}
    print(f"  froze {name}: {d.page_count}pp, {len(uris)} uri, {internal} internal")

json.dump(meta, open(os.path.join(OUT, "ref_meta.json"), "w"), indent=1)
print(f"\nwrote {len(CORPUS)} text refs + fixtures/ref_meta.json")
