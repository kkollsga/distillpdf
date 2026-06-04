"""Shared HTML-inspection helpers for the owned CI suite.

These are the same structural checks the (local-only) corpus suite uses, factored out
so the committed, corpus-free tests can apply them to our OWN fixtures. Pure functions
over the emitted HTML — no ground truth, no corpus.
"""
import glob
import os
import re
import unicodedata
from collections import Counter
from html.parser import HTMLParser

HERE = os.path.dirname(os.path.abspath(__file__))
FIX = os.path.join(HERE, "fixtures_pdf")


# --------------------------------------------------------------- the owned fixture set
def owned_pdfs():
    """Every committed, self-generated PDF: the capability fixtures, the demo + its
    platypus variant, and the synthetic table corpus. The structural checks run over
    all of them, so a defect in any owned document fails CI."""
    pats = [
        os.path.join(FIX, "*.pdf"),
        os.path.join(HERE, "demo", "*.pdf"),
        os.path.join(HERE, "corpus_tables", "*.pdf"),
    ]
    out = []
    for p in pats:
        out += sorted(glob.glob(p))
    return out


def doc_pdfs():
    """The document-like owned fixtures (a title, sections, prose) — the set the
    document-level structural-fidelity checks apply to. Excludes the synthetic table
    corpus (bare grids with no title/sections), which test_tables covers and which
    would false-positive document checks like title_not_h1."""
    out = sorted(glob.glob(os.path.join(FIX, "*.pdf")))
    out += sorted(glob.glob(os.path.join(HERE, "demo", "*.pdf")))
    return out


_HTML_CACHE = {}


def html_for(path):
    # These structural checks encode the PAGE-mode contract (one <section> per page,
    # #page-N anchors, per-page reading order), so they pin mode="page" explicitly.
    # Section-mode output is validated separately in test_section_mode.py.
    if path not in _HTML_CACHE:
        import distillpdf
        _HTML_CACHE[path] = distillpdf.Pdf.open(path).to_html(mode="page")
    return _HTML_CACHE[path]


# ----------------------------------------------------------------------------- helpers
def plain(html):
    """Visible text: SVG dropped, tags stripped, entities decoded, NFKC, ws-collapsed."""
    s = re.sub(r"<svg\b.*?</svg>", " ", html, flags=re.DOTALL)
    s = re.sub(r"<[^>]+>", " ", s)
    for a, b in (("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", '"'), ("&#39;", "'")):
        s = s.replace(a, b)
    return re.sub(r"\s+", " ", unicodedata.normalize("NFKC", s)).strip()


def words(s):
    return re.findall(r"\w+", s, re.UNICODE)


def headings(html):
    return [(int(m.group(1)), plain(m.group(2)))
            for m in re.finditer(r"<h([1-6])\b[^>]*>(.*?)</h\1>", html, re.DOTALL)]


def captions(html):
    out = []
    for tag in ("caption", "figcaption", "div"):
        out += [plain(m) for m in re.findall(rf"<{tag}\b.*?</{tag}>", html, re.DOTALL)]
    return out


def tables(html):
    return re.findall(r"<table\b.*?</table>", html, re.DOTALL)


def row_cells(table, tag):
    rows = []
    for tr in re.findall(r"<tr\b.*?</tr>", table, re.DOTALL):
        rows.append([plain(c) for c in re.findall(rf"<{tag}\b[^>]*>(.*?)</{tag}>", tr, re.DOTALL)])
    return rows


# ---------------------------------------------------------------- well-formedness etc.
class _Balance(HTMLParser):
    BLOCK = {"html", "body", "section", "table", "tr", "td", "th", "ul", "ol", "li",
             "h1", "h2", "h3", "h4", "h5", "h6", "p", "b", "i", "pre", "code",
             "figure", "figcaption", "nav", "a", "sub", "sup", "aside"}
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
            while self.stack and self.stack.pop() != tag:
                pass
        else:
            self.errors.append(f"stray </{tag}>")


def well_formed(html):
    b = _Balance()
    try:
        b.feed(html)
        b.close()
    except Exception as e:  # noqa: BLE001
        return False, [str(e)]
    if b.stack:
        b.errors.append(f"unclosed: {b.stack[-5:]}")
    return (len(b.errors) == 0), b.errors


def plain_bytes(html):
    return len(re.sub(r'src="data:[^"]*"', 'src=""', re.sub(r"<[^>]+>", " ", html)).encode())


def markup_bytes(html):
    """Body markup size, excluding figure payloads (image data URIs, SVG paths), the
    fixed <style>, and the auto <nav> TOC — so the ratio measures body markup density."""
    html = re.sub(r'src="data:[^"]*"', 'src=""', html)
    html = re.sub(r"<svg\b.*?</svg>", "<svg></svg>", html, flags=re.DOTALL)
    html = re.sub(r"<style\b.*?</style>", "", html, flags=re.DOTALL)
    html = re.sub(r"<nav\b.*?</nav>", "", html, flags=re.DOTALL)
    return len(html.encode())


# ----------------------------------------------------------- 16 structural-defect checks
# Each: fn(name, html, txt) -> (hits, examples). All must be 0 over the owned fixtures.
def c_dup_tokens(name, html, txt):
    toks = words(txt)
    if len(toks) < 20:
        return 0, []
    adj = sum(1 for a, b in zip(toks, toks[1:]) if a == b and len(a) > 1)
    intra = sum(1 for t in toks if len(t) >= 6 and len(t) % 2 == 0 and t[: len(t) // 2] == t[len(t) // 2:])
    frac = (adj + intra) / len(toks)
    return (1, [f"{name}: {frac:.0%} duplicated tokens"]) if frac > 0.05 else (0, [])


PNUM = re.compile(r"<p>\s*(?:<b>)?\s*(\d{1,4})\s*(?:</b>)?\s*</p>")


def c_page_number_noise(name, html, txt):
    ex = []
    for sec in re.findall(r"<section\b[^>]*>(.*?)</section>", html, re.DOTALL):
        s = sec.strip()
        m = PNUM.match(s)
        if m and not PNUM.match(s[m.end():].lstrip()):
            ex.append(f"{name}: top <p>{m.group(1)}</p>")
        m2 = re.search(PNUM.pattern + r"\s*$", s)
        if m2 and not re.search(PNUM.pattern + r"\s*$", s[: m2.start()].rstrip()):
            ex.append(f"{name}: bottom <p>{m2.group(1)}</p>")
    for _ in re.finditer(r"<h[1-6]\b[^>]*>\s*\d{1,4}\s*</h[1-6]>", html):
        ex.append(f"{name}: numeric heading")
    return len(ex), ex[:4]


YEAR = re.compile(r"\b(19|20)\d{2}\b")
MID_INITIAL = re.compile(r"\S\s+[A-Z]\.(\s|$)|,\s*[A-Z]\.")


def c_bad_headings(name, html, txt):
    ex = []

    def strip_num(t):
        return re.sub(r"^\s*(\d+(\.\d+)*\.?|[IVXLC]+\.|[A-Z]\.)\s+", "", t)

    def reads_as_sentence(t):
        t = strip_num(t)
        return ("," in t) or bool(re.search(r"\.\s+[A-Z(]", t)) or bool(re.search(r"\b(is|are|was|were)\b", t.lower()))

    def reads_as_sentence_strict(t):
        t = strip_num(t)
        return bool(re.search(r"\.\s+[A-Z(]", t)) or bool(re.search(r"\b(is|are|was|were)\b", t.lower()))

    for lvl, t in headings(html):
        t = t.strip()
        wc = len(words(t))
        if wc == 0:
            continue
        why = None
        title = lvl == 1
        if t.endswith(","):
            why = "ends comma"
        elif not title and wc > 12 and reads_as_sentence(t):
            why = "sentence(>12w)"
        elif not title and wc >= 5 and t.endswith(".") and reads_as_sentence_strict(t):
            why = "sentence-period"
        elif (MID_INITIAL.search(t) and not re.match(r"(Appendix|Section|Chapter|Part|Theorem|Lemma|Figure|Table)\b", t)) or YEAR.search(t) or " et al" in t.lower():
            why = "reference/author markers"
        elif not title and t.count(",") >= 2:
            why = "comma-list"
        elif wc >= 6 and not re.search(r"[.!?:]$", t) and t[:1].islower():
            why = "lowercase fragment"
        if why:
            ex.append(f"{name}: [{why}] <h{lvl}>{t[:48]}")
    return len(ex), ex[:4]


REF_CAPTION = re.compile(
    r"^(figure|fig\.?|table)\s*\d+\s+(shows?|summari|present|plot|compar|illustrat|depict|"
    r"report|lists?|gives?|provides?|denotes?|displays?|demonstrat|describe|we |it )", re.I)


def c_ref_as_caption(name, html, txt):
    ex = [f"{name}: {c[:60]}" for c in captions(html) if REF_CAPTION.match(c.strip())]
    return len(ex), ex[:4]


def c_duplicate_ids(name, html, txt):
    ids = re.findall(r'\bid="([^"]+)"', html)
    dups = [i for i, n in Counter(ids).items() if n > 1]
    return len(dups), [f"{name}: id={i}×{Counter(ids)[i]}" for i in dups[:4]]


def c_orphan_graphics(name, html, txt):
    ex = []
    for m in re.finditer(r"(<figure\b.*?</figure>)\s*(<figure\b.*?</figure>)", html, re.DOTALL):
        a, b = m.group(1), m.group(2)
        ga, ca = ("<img" in a or "<svg" in a), ("<figcaption" in a)
        gb, cb = ("<img" in b or "<svg" in b), ("<figcaption" in b)
        if (ga and not ca and cb and not gb) or (ca and not ga and gb and not cb):
            ex.append(f"{name}: graphic+caption split into adjacent <figure>s")
    return len(ex), ex[:4]


MATH_SIG = re.compile(r"[∑∫√∈≤≥≠×·θαβγλσφπμΩ]|\b(theorem|lemma|equation|proof|eq\.)\b", re.I)
MATH_DOC = re.compile(r"\b(theorem|lemma|proof|corollary|proposition|equation|eq\.)\b", re.I)


def c_no_subsup(name, html, txt):
    if not MATH_DOC.search(txt):
        return 0, []
    n = len(re.findall(r"<su[bp]\b", html))
    return (1, [f"{name}: math doc, 0 <sub>/<sup>"]) if n == 0 else (0, [])


EQ_NUM = re.compile(r"^\(\d+(\.\d+)?\)")


def c_math_as_table(name, html, txt):
    ex = []
    for t in tables(html):
        if "<caption" in t:
            continue
        cells = [c for r in row_cells(t, "td") + row_cells(t, "th") for c in r if c.strip()]
        rows = len(re.findall(r"<tr\b", t))
        if not cells or rows > 5:
            continue
        joined = " ".join(cells)
        has_eqnum = any(EQ_NUM.match(c.strip()) for c in cells)
        has_math = bool(MATH_SIG.search(joined)) or "=" in joined
        real_words = len(re.findall(r"[^\W\d_]{3,}", joined))
        if has_math and (has_eqnum or "=" in joined) and real_words <= len(cells):
            ex.append(f"{name}: equation-as-table {cells[:3]}")
    return len(ex), ex[:4]


def c_prose_tables(name, html, txt):
    ex = []
    aw = re.compile(r"[^\W\d_]{2,}", re.UNICODE)
    for t in tables(html):
        cells = [c for r in row_cells(t, "td") for c in r if c.strip()]
        if len(cells) < 3:
            continue
        mean_w = sum(len(aw.findall(c)) for c in cells) / len(cells)
        if mean_w > 8:
            ex.append(f"{name}: table mean {mean_w:.1f} prose-words/cell")
    return len(ex), ex[:4]


def c_stray_comma(name, html, txt):
    n = len(re.findall(r"<p>\s*,+\s*</p>", html))
    return n, ([f"{name}: {n} stray <p>,</p>"] if n else [])


def c_single_item_list(name, html, txt):
    html = re.sub(r"<nav\b.*?</nav>", "", html, flags=re.DOTALL)
    lists = [(m.group(1), len(re.findall(r"<li\b", m.group(0))) == 1, m.start(), m.end())
             for m in re.finditer(r"<(ul|ol)\b.*?</\1>", html, re.DOTALL)]

    def adjacent_same(a_end, b_start):
        if not (0 <= b_start - a_end < 500):
            return False
        return "</section>" not in html[a_end:b_start]

    n = 0
    for t, single, s, e in lists:
        if not single:
            continue
        near = any(t2 == t and adjacent_same(e, s2) for (t2, _, s2, _) in lists) \
            or any(t2 == t and adjacent_same(e2, s) for (t2, _, _, e2) in lists)
        if near:
            n += 1
    return n, ([f"{name}: {n} fragmented single-item list(s)"] if n else [])


def c_decimal_as_colon(name, html, txt):
    tbl_txt = " ".join(plain(t) for t in tables(html))
    n = len(re.findall(r"\d{2,}\s:\s\d|\d\s:\s\d{2,}", tbl_txt))
    return n, ([f"{name}: {n}× table decimal-as-colon"] if n else [])


def c_split_links(name, html, txt):
    ex = []
    cur = None
    toks = list(re.finditer(r'<a href="([^"]+)">|</a>', html))
    for idx, m in enumerate(toks):
        if m.group(0) == "</a>":
            if idx + 1 < len(toks):
                nxt = toks[idx + 1]
                if cur and html[m.end():nxt.start()].strip() == "" and nxt.group(1) == cur:
                    ex.append(f"{name}: split <a href={cur}>")
            cur = None
        else:
            cur = m.group(1)
    return len(ex), ex[:4]


def c_first_heading_h1(name, html, txt):
    hs = headings(html)
    if not hs or hs[0][0] == 1:
        return 0, []
    m = re.search(r"<section\b[^>]*>(.*?)</section>", html, re.DOTALL)
    if m:
        p1 = m.group(1)
        p1_text = re.sub(r"<svg.*?</svg>", " ", p1, flags=re.DOTALL)
        p1_text = re.sub(r"<[^>]+>", " ", re.sub(r'src="[^"]*"', "", p1_text))
        if ("<img" in p1 or "<figure" in p1) and len(p1_text.split()) < 10:
            return 0, []
    return 1, [f"{name}: first heading is <h{hs[0][0]}>, not h1"]


def c_citation_page_anchor(name, html, txt):
    n = len(re.findall(r'href="#page-\d+"', html))
    return n, ([f"{name}: {n}× href=#page-N"] if n else [])


FUSION = re.compile(r"\b[a-z]{5,}[A-Z][a-z]{3,}\b")
_IDENT_OK = ("javascript", "github", "openai", "arxiv", "distillpdf", "pymupdf", "bibtex",
             "pytorch", "tensorflow", "huggingface")


def c_word_salad(name, html, txt):
    counts = Counter(FUSION.findall(txt))
    frequent = {e for e, c in counts.items() if c > 2}

    def ident_variant(e):
        return any(e != f and (e.startswith(f) or f.startswith(e)) for f in frequent)

    ex = [e for e, c in counts.items() if c <= 2 and e.lower() not in _IDENT_OK and not ident_variant(e)]
    return len(ex), [f"{name}: {e}" for e in ex[:4]]


# (key, check fn). math_no_subsup is a capability-presence check that needs real sub/sup
# notation; the owned (synthetic) fixtures can't produce detectable scripts yet, so the
# test marks it xfail. Every other check is a hard defect guard over the owned set.
CHECKS = [
    ("dup_tokens", c_dup_tokens),
    ("reading_order_fusion", c_word_salad),
    ("stray_comma_p", c_stray_comma),
    ("figure_caption_decoupled", c_orphan_graphics),
    ("duplicate_ids", c_duplicate_ids),
    ("bad_headings", c_bad_headings),
    ("page_number_noise", c_page_number_noise),
    ("ref_as_caption", c_ref_as_caption),
    ("math_no_subsup", c_no_subsup),
    ("math_as_table", c_math_as_table),
    ("prose_as_table", c_prose_tables),
    ("single_item_list", c_single_item_list),
    ("decimal_as_colon", c_decimal_as_colon),
    ("split_links", c_split_links),
    ("title_not_h1", c_first_heading_h1),
    ("citation_page_anchor", c_citation_page_anchor),
]
