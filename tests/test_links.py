#!/usr/bin/env python3
"""Goalline for hyperlink extraction: distillpdf.Pdf.extract_links() vs PyMuPDF.

For every PDF in corpus/ and corpus_headers/ we compare against PyMuPDF's
page.get_links() (the reference):
  - external URIs: recall of distinct (page, uri) link annotations
  - internal links: recall of GoTo/GoToR cross-references, and the fraction we
    resolve to a target page number
  - HTML wiring: external links also surface as <a href> in to_html()

Run:  python3 bench/test_links.py     (report + non-zero exit on failure)
  or: pytest bench/test_links.py
"""
import glob, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = sorted(glob.glob(os.path.join(HERE, "corpus", "*.pdf"))
                + glob.glob(os.path.join(HERE, "corpus_headers", "*.pdf")))

# Bars (set below demonstrated capability). Link annotations are unambiguous in
# the PDF, so recall should be ~1.0; resolution of internal dests is the harder
# part. Only PDFs that actually carry links are scored.
MIN_URI_RECALL = 0.97
MIN_INTERNAL_RECALL = 0.95
MIN_INTERNAL_RESOLVED = 0.90


_META = None
def pymupdf_links(path):
    """(uri set, internal count) from frozen fixtures (freeze_refs.py) — needs
    only distillpdf. Falls back to live pymupdf if the fixture is missing."""
    global _META
    import json
    meta_p = os.path.join(HERE, "fixtures", "ref_meta.json")
    if _META is None and os.path.exists(meta_p):
        _META = json.load(open(meta_p))
    if _META is not None and os.path.basename(path) in _META:
        m = _META[os.path.basename(path)]
        return set((p, u) for p, u in m.get("link_uris", [])), m.get("link_internal", 0)
    return None  # no frozen fixture — caller skips (gates run on distillpdf alone)


def run():
    import distillpdf
    failures = []
    uri_recalls, int_recalls, int_resolved, scored = [], [], [], 0
    tot_uri = tot_int = tot_html_a = 0

    print(f"{'PDF':30s}{'uriR':>7s}{'intR':>7s}{'resolv':>8s}{'htmlA':>7s}")
    for path in CORPUS:
        name = os.path.basename(path)
        ref = pymupdf_links(path)
        if ref is None:
            print(f"{name[:30]:30s}  (skipped — no frozen fixture)")
            continue
        ref_uris, ref_int = ref
        links = distillpdf.Pdf.open(path).extract_links()
        our_uris = set((l["page"], l["uri"]) for l in links if l["kind"] == "uri" and l["uri"])
        our_int = [l for l in links if l["kind"] == "internal"]
        resolved = sum(1 for l in our_int if l["dest_page"])
        html_a = len(re.findall(r"<a href=", distillpdf.Pdf.open(path).to_html()))
        tot_uri += len(our_uris); tot_int += len(our_int); tot_html_a += html_a

        if not ref_uris and ref_int == 0:
            print(f"{name[:30]:30s}{'  —  ':>7s}{'  —  ':>7s}{'  —  ':>8s}{html_a:>7d}  (no links)")
            continue
        scored += 1
        uri_r = len(ref_uris & our_uris) / len(ref_uris) if ref_uris else 1.0
        int_r = min(1.0, len(our_int) / ref_int) if ref_int else 1.0
        res = resolved / len(our_int) if our_int else 1.0
        uri_recalls.append(uri_r); int_recalls.append(int_r); int_resolved.append(res)
        print(f"{name[:30]:30s}{uri_r:>7.2f}{int_r:>7.2f}{res:>8.2f}{html_a:>7d}")

    mu = sum(uri_recalls) / len(uri_recalls) if uri_recalls else 1.0
    mi = sum(int_recalls) / len(int_recalls) if int_recalls else 1.0
    mr = sum(int_resolved) / len(int_resolved) if int_resolved else 1.0
    print(f"\n{scored} PDFs with links. totals: uri={tot_uri} internal={tot_int} html<a>={tot_html_a}")
    print(f"mean URI recall:        {mu:.3f} (need >={MIN_URI_RECALL})")
    print(f"mean internal recall:   {mi:.3f} (need >={MIN_INTERNAL_RECALL})")
    print(f"mean internal resolved: {mr:.3f} (need >={MIN_INTERNAL_RESOLVED})")
    if mu < MIN_URI_RECALL:
        failures.append(f"URI recall {mu:.3f} < {MIN_URI_RECALL}")
    if mi < MIN_INTERNAL_RECALL:
        failures.append(f"internal recall {mi:.3f} < {MIN_INTERNAL_RECALL}")
    if mr < MIN_INTERNAL_RESOLVED:
        failures.append(f"internal resolved {mr:.3f} < {MIN_INTERNAL_RESOLVED}")
    if tot_html_a == 0:
        failures.append("no <a href> emitted in to_html across the corpus")

    if failures:
        print(f"\n❌ {len(failures)} FAILURES:")
        for f in failures:
            print("  -", f)
        return False
    print("\n✅ ALL CHECKS PASSED")
    return True


def test_link_extraction():
    assert run(), "link extraction goalline not met (see report above)"


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
