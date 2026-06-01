#!/usr/bin/env python3
"""Score text-extraction quality of each tool against the PyMuPDF reference.
Reference = pymupdf_plain.txt (raw MuPDF text, most complete ground truth).
Metrics per PDF/tool:
  recall    = |ref_words ∩ tool_words| / |ref_words|   (did it capture the content words)
  jaccard   = |∩| / |∪|                                 (content overlap, penalises junk)
  seqratio  = difflib ratio on normalised text          (order + content fidelity)
"""
import glob, json, os, re, difflib

OUT = os.path.join(os.path.dirname(__file__), "out")
WORD = re.compile(r"\w+", re.UNICODE)

def norm_words(t):
    return [w.lower() for w in WORD.findall(t)]

def read(name, suffix):
    for ext in (suffix,):
        p = os.path.join(OUT, f"{name}.{ext}")
        if os.path.exists(p):
            return open(p, encoding="utf-8", errors="replace").read()
    return None

# discover corpus names from the reference files
names = sorted(os.path.splitext(os.path.basename(p))[0].replace(".pymupdf_plain","")
               for p in glob.glob(os.path.join(OUT, "*.pymupdf_plain.txt")))

TOOLS = {
    "pymupdf4llm": "pymupdf4llm.md",   # baseline's own md vs its own plain (sanity ~1.0 recall)
    "oxidize":     "oxidize.txt",
    "pdfextract":  "pdfextract.txt",
    "pdfinspector":"pdfinspector.md",
    "pdfoxide":    "pdfoxide.md",
    "pylopdf":     "pylopdf.txt",
}

rows = {}
for name in names:
    ref = read(name, "pymupdf_plain.txt")
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
        seq = difflib.SequenceMatcher(None, " ".join(refw), " ".join(tw)).ratio()
        rows[name][tool] = {"recall": round(recall,3), "jaccard": round(jacc,3),
                            "seqratio": round(seq,3), "words": len(tw)}

# print table
hdr = f"{'PDF':28s} {'refW':>6s}  " + "  ".join(f"{t:>22s}" for t in TOOLS)
print(hdr); print("-"*len(hdr))
for name, r in rows.items():
    cells = []
    for t in TOOLS:
        v = r[t]
        cells.append("        FAIL/—        " if v is None
                     else f"R{v['recall']:.2f} J{v['jaccard']:.2f} S{v['seqratio']:.2f}")
    print(f"{name:28s} {r['ref_words']:>6d}  " + "  ".join(f"{c:>22s}" for c in cells))

print("\nLegend: R=word recall  J=Jaccard  S=difflib seq ratio (vs PyMuPDF raw text). Higher=better.")
json.dump(rows, open(os.path.join(OUT,"quality.json"),"w"), indent=2)
