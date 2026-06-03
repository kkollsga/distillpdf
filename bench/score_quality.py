#!/usr/bin/env python3
"""Score text-extraction quality of each tool against the PyMuPDF reference.
Reference = pymupdf_plain.txt (raw MuPDF text, most complete ground truth).
Metrics per PDF/tool:
  recall    = |ref_words ∩ tool_words| / |ref_words|   (did it capture the content words)
  jaccard   = |∩| / |∪|                                 (content overlap, penalises junk)
  order(O)  = token-level difflib ratio, autojunk OFF   (READING ORDER fidelity)

Reading-order note (why `order` and not the old `seqratio`):
  The previous metric ran difflib on the *joined character string* with difflib's
  default `autojunk=True`. On any document over ~200 tokens autojunk discards every
  word occurring >1% of the time — i.e. exactly the frequent words that pin down
  sequence alignment — so the score collapsed into noise and *under*-reported good
  ordering (e.g. arXiv math scored 0.21 while its true word order matched the
  reference at 0.96). `order` instead runs difflib over the **word-token list with
  autojunk=False**, which measures how faithfully the tool reproduces PyMuPDF's
  reading order. This is what exposes 2-column interleaving: a tool that emits
  left-line/right-line/left-line keeps high recall but craters on `order`.
"""
import glob, json, os, re, difflib

OUT = os.path.join(os.path.dirname(__file__), "out")       # tool outputs (regenerated)
FIX = os.path.join(os.path.dirname(__file__), "fixtures")  # PyMuPDF reference (committed)
WORD = re.compile(r"\w+", re.UNICODE)

def norm_words(t):
    return [w.lower() for w in WORD.findall(t)]

def read(name, suffix, base=OUT):
    p = os.path.join(base, f"{name}.{suffix}")
    return open(p, encoding="utf-8", errors="replace").read() if os.path.exists(p) else None

# discover corpus names from the reference fixtures
names = sorted(os.path.splitext(os.path.basename(p))[0].replace(".pymupdf_plain","")
               for p in glob.glob(os.path.join(FIX, "*.pymupdf_plain.txt")))

TOOLS = {
    "pymupdf4llm": "pymupdf4llm.md",   # baseline's own md vs its own plain (sanity ~1.0 recall)
    "oxidize":     "oxidize.txt",
    "pdfextract":  "pdfextract.txt",
    "pdfinspector":"pdfinspector.md",
    "pdfoxide":    "pdfoxide.md",
    "distillpdf":     "distillpdf.txt",
}

rows = {}
for name in names:
    ref = read(name, "pymupdf_plain.txt", base=FIX)
    if not ref:
        continue
    refw = norm_words(ref)
    refset = set(refw)
    rows[name] = {"ref_words": len(refw)}
    for tool, suf in TOOLS.items():
        txt = read(name, suf)
        if txt is None:
            rows[name][tool] = None
            continue
        tw = norm_words(txt)
        tset = set(tw)
        recall = len(refset & tset) / len(refset) if refset else 0.0
        jacc = len(refset & tset) / len(refset | tset) if (refset|tset) else 0.0
        # READING ORDER: token-level, autojunk OFF (see module docstring).
        order = difflib.SequenceMatcher(None, refw, tw, autojunk=False).ratio()
        rows[name][tool] = {"recall": round(recall,3), "jaccard": round(jacc,3),
                            "order": round(order,3), "words": len(tw)}

# print table
hdr = f"{'PDF':28s} {'refW':>6s}  " + "  ".join(f"{t:>22s}" for t in TOOLS)
print(hdr); print("-"*len(hdr))
for name, r in rows.items():
    cells = []
    for t in TOOLS:
        v = r[t]
        cells.append("        FAIL/—        " if v is None
                     else f"R{v['recall']:.2f} J{v['jaccard']:.2f} O{v['order']:.2f}")
    print(f"{name:28s} {r['ref_words']:>6d}  " + "  ".join(f"{c:>22s}" for c in cells))

print("\nLegend: R=word recall  J=Jaccard  O=reading-order fidelity "
      "(token difflib, autojunk off, vs PyMuPDF). Higher=better.")
json.dump(rows, open(os.path.join(OUT,"quality.json"),"w"), indent=2)
