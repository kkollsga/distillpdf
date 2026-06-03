#!/usr/bin/env python3
"""Head-to-head header extraction vs competitors, on tuned + held-out sets.
Markdown tools (pymupdf4llm/pdfinspector/pdfoxide) -> "#" headings; distillpdf -> <h1-3>.
Reports recall, precision, level accuracy per tool. Run from bench/."""
import json, os, re, distillpdf
SETS=[("tuned","header_groundtruth.json","corpus"),
      ("held-out","header_groundtruth_heldout.json","corpus_headers")]
def norm(s):
    s=re.sub(r"<[^>]+>"," ",s); s=re.sub(r"[^0-9a-z]+"," ",s.lower()); return re.sub(r"\s+"," ",s).strip()
NUM=re.compile(r"^\s*([IVX]+|[A-Z]|\d+(?:\.\d+)*)[.\)]?\s+")
def title(s): return norm(NUM.sub("",s.strip()))
def cap(g,heads):
    gf,gt_=norm(g),title(g) or norm(g)
    for h in heads:
        if gf==h[1] or gt_==h[2] or (gt_ and (f" {gt_} " in f" {h[1]} " or f" {gt_} " in f" {h[2]} ")) or (h[2] and f" {h[2]} " in f" {gf} "): return h
    return None
def md_heads(t):
    out=[]
    for line in t.splitlines():
        m=re.match(r"^(#{1,6})\s+(.*)$",line.strip())
        if m and norm(m.group(2)): out.append((min(len(m.group(1)),3),norm(m.group(2)),title(m.group(2))))
    return out
def html_heads(h): return [(int(l),norm(t),title(t)) for l,t in re.findall(r"<h([123])>(.*?)</h\1>",h,re.DOTALL) if norm(t)]
def score(rows,heads):
    found=lvlok=0; matched=set()
    for r in rows:
        m=cap(r["text"],heads)
        if m: found+=1; matched.add(id(m)); lvlok+= (m[0]==r["level"])
    rec=found/len(rows) if rows else 1
    prec=(sum(1 for h in heads if id(h) in matched)/len(heads)) if heads else 1
    lvl=lvlok/found if found else 0
    return rec,prec,lvl
TOOLS={"pymupdf4llm":"pymupdf4llm.md","pdfinspector":"pdfinspector.md","pdfoxide":"pdfoxide.md","distillpdf":None}
for label,gtf,subdir in SETS:
    gt=json.load(open(gtf)); files=[k for k in gt if not k.startswith("_")]
    agg={t:[] for t in TOOLS}
    for name in files:
        for t,suf in TOOLS.items():
            if t=="distillpdf": heads=html_heads(distillpdf.Pdf.open(f"{subdir}/{name}").to_html())
            else:
                p=f"out/{os.path.splitext(name)[0]}.{suf}"
                heads=md_heads(open(p,encoding='utf-8',errors='replace').read()) if os.path.exists(p) else []
            agg[t].append(score(gt[name],heads))
    print(f"\n=== {label} set ({len(files)} papers) ===")
    print(f"  {'tool':14s}{'recall':>9s}{'precision':>11s}{'level_acc':>11s}")
    for t in TOOLS:
        rs=agg[t]; n=len(rs)
        print(f"  {t:14s}{sum(x[0] for x in rs)/n:>9.3f}{sum(x[1] for x in rs)/n:>11.3f}{sum(x[2] for x in rs)/n:>11.3f}")
