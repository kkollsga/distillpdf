#!/usr/bin/env python3
"""Acceptance test / oracle for pylopdf.Pdf.to_html() — the "AI-ready HTML" feature.

This is the single source of truth for the to_html goal. It is corpus-driven:
every PDF in bench/corpus/ is exercised, and the PyMuPDF reference text is computed
on the fly (no pre-baked fixtures), so the bar scales as the corpus grows.

Run:  python3 bench/test_html.py        (prints a report, exits non-zero on failure)
  or: pytest bench/test_html.py

Thresholds are intentionally set BELOW demonstrated capability (text recall ~0.98)
so the goal is achievable and binary — avoiding an unsatisfiable loop.
"""
import glob, os, re, sys
from html.parser import HTMLParser

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = sorted(glob.glob(os.path.join(HERE, "corpus", "*.pdf")))

# ---- thresholds (calibrated to demonstrated capability across a broad corpus) ----
# Single-column docs extract near-perfectly (0.94-1.0). Two-column arXiv papers
# (cmr fonts + column reading order) are the hard frontier at ~0.88-0.91; the bars
# below are demonstrated-achievable + strong, not optimistic guesses.
MIN_RECALL_PER_FILE = 0.85
MIN_RECALL_MEAN = 0.93
MAX_MARKUP_RATIO = 2.2          # non-image markup / plain-text bytes (heading-dense
                                # tiny docs are legitimately ~1.9x for clean semantic HTML;
                                # real bloat with CSS/classes would be far higher)
WORD = re.compile(r"\w+", re.UNICODE)

# Files expected to contain images / tables (by filename substring).
EXPECT_IMAGES = ("Cold_Email", "attention", "arxiv_nerf", "geology_usgs")
EXPECT_TABLES = ("fw9_form", "attention")


def pymupdf_ref(path):
    import pymupdf
    d = pymupdf.open(path)
    return d.page_count, "".join(p.get_text() for p in d)


def to_html(path):
    import pylopdf
    return pylopdf.Pdf.open(path).to_html()


# ---- HTML well-formedness: tag-balance check for key block elements ----
class Balance(HTMLParser):
    BLOCK = {"html", "body", "section", "table", "tr", "td", "th",
             "ul", "ol", "li", "h1", "h2", "h3", "p", "b", "i", "pre", "code"}
    VOID = {"img", "br", "hr", "meta"}

    def __init__(self):
        super().__init__()
        self.stack = []
        self.errors = []

    def handle_starttag(self, tag, attrs):
        if tag in self.VOID:
            return
        if tag in self.BLOCK:
            self.stack.append(tag)

    def handle_endtag(self, tag):
        if tag in self.VOID or tag not in self.BLOCK:
            return
        if tag in self.stack:
            # pop to the matching tag (tolerate implicit closes)
            while self.stack and self.stack.pop() != tag:
                pass
        else:
            self.errors.append(f"stray </{tag}>")


def well_formed(html):
    b = Balance()
    try:
        b.feed(html)
        b.close()
    except Exception as e:
        return False, [str(e)]
    if b.stack:
        b.errors.append(f"unclosed: {b.stack[-5:]}")
    return (len(b.errors) == 0), b.errors


def strip_tags(html):
    # drop embedded image data URIs, then tags, then unescape basics
    html = re.sub(r'src="data:[^"]*"', 'src=""', html)
    text = re.sub(r"<[^>]+>", " ", html)
    for a, b in (("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", '"'), ("&#39;", "'")):
        text = text.replace(a, b)
    return text


def markup_bytes(html):
    return len(re.sub(r'src="data:[^"]*"', 'src=""', html))


def recall(html_text, ref_text):
    rw = set(w.lower() for w in WORD.findall(ref_text))
    hw = set(w.lower() for w in WORD.findall(html_text))
    return len(rw & hw) / len(rw) if rw else 1.0


def run():
    assert CORPUS, "no corpus PDFs found"
    results = {}
    recalls = []
    feature = {"heading": False, "bold": False, "italic": False,
               "table": False, "image": False, "list": False, "mono": False}
    failures = []

    for path in CORPUS:
        name = os.path.basename(path)
        rec = {}
        try:
            pages, ref = pymupdf_ref(path)
            html = to_html(path)
        except Exception as e:
            failures.append(f"{name}: to_html raised {type(e).__name__}: {str(e)[:120]}")
            results[name] = {"error": str(e)[:120]}
            continue

        # 1. non-empty + well-formed
        rec["bytes"] = len(html)
        ok, errs = well_formed(html)
        if not ok:
            failures.append(f"{name}: malformed HTML {errs[:2]}")

        # 2. per-page sections
        n_sections = len(re.findall(r"<section\b", html))
        rec["sections"] = n_sections
        if n_sections != pages:
            failures.append(f"{name}: {n_sections} sections != {pages} pages")

        # 3. no bloat
        for bad in ("<script", "<style", "class="):
            if bad in html:
                failures.append(f"{name}: contains '{bad}' (not thin)")

        # 4. text recall
        r = recall(strip_tags(html), ref)
        rec["recall"] = round(r, 3)
        recalls.append(r)
        if r < MIN_RECALL_PER_FILE:
            failures.append(f"{name}: recall {r:.3f} < {MIN_RECALL_PER_FILE}")

        # 5. thinness (exclude embedded image data)
        plain = " ".join(WORD.findall(ref)) or " "
        ratio = markup_bytes(html) / max(len(plain), 1)
        rec["markup_ratio"] = round(ratio, 2)
        if ratio > MAX_MARKUP_RATIO:
            failures.append(f"{name}: markup ratio {ratio:.2f} > {MAX_MARKUP_RATIO}")

        # 6. per-file structural expectations
        if any(k in name for k in EXPECT_IMAGES) and not re.search(r'<img\s+[^>]*src="data:image', html):
            failures.append(f"{name}: expected embedded <img data:> but found none")
        if any(k in name for k in EXPECT_TABLES) and not re.search(r"<table\b", html):
            failures.append(f"{name}: expected <table> but found none")

        # feature coverage (corpus-wide)
        if re.search(r"<h[123]\b", html): feature["heading"] = True
        if "<b>" in html: feature["bold"] = True
        if "<i>" in html: feature["italic"] = True
        if re.search(r"<table\b", html): feature["table"] = True
        if re.search(r'src="data:image', html): feature["image"] = True
        if re.search(r"<[uo]l\b", html): feature["list"] = True
        if re.search(r"<pre\b", html): feature["mono"] = True
        results[name] = rec

    mean_recall = sum(recalls) / len(recalls) if recalls else 0.0
    if mean_recall < MIN_RECALL_MEAN:
        failures.append(f"MEAN recall {mean_recall:.3f} < {MIN_RECALL_MEAN}")
    for feat, seen in feature.items():
        if not seen:
            failures.append(f"feature '{feat}' not produced by ANY corpus PDF")

    # ---- report ----
    print(f"{'PDF':28s}{'pages':>6s}{'sect':>5s}{'recall':>8s}{'mu_ratio':>9s}")
    for name, rec in results.items():
        if "error" in rec:
            print(f"{name:28s}  ERROR: {rec['error']}")
        else:
            print(f"{name:28s}{'':>6}{rec['sections']:>5}{rec['recall']:>8}{rec['markup_ratio']:>9}")
    print(f"\nmean recall: {mean_recall:.3f} (need >={MIN_RECALL_MEAN})")
    print("features:", {k: ("✓" if v else "✗") for k, v in feature.items()})
    if failures:
        print(f"\n❌ {len(failures)} FAILURES:")
        for f in failures[:40]:
            print("  -", f)
        return False
    print("\n✅ ALL CHECKS PASSED")
    return True


# ---- pytest entrypoints ----
def test_to_html_contract():
    assert run(), "to_html acceptance contract not met (see report above)"


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
