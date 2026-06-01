#!/usr/bin/env python3
"""Benchmark pdf_oxide (pip wheel) on the same corpus, mirroring the baseline."""
import glob, json, os, time, statistics
import pdf_oxide as m

CORPUS = sorted(glob.glob(os.path.join(os.path.dirname(__file__), "corpus", "*.pdf")))
OUT = os.path.join(os.path.dirname(__file__), "out")
RUNS = 5

def timeit(fn, runs=RUNS):
    fn()
    ts=[]
    for _ in range(runs):
        t=time.perf_counter(); fn(); ts.append(time.perf_counter()-t)
    return statistics.median(ts)

def call_any(doc, names, *a):
    for n in names:
        f=getattr(doc,n,None)
        if f:
            try: return f(*a), n
            except Exception: pass
    return None, None

results={}
for path in CORPUS:
    name=os.path.splitext(os.path.basename(path))[0]
    rec={"file":os.path.basename(path),"bytes":os.path.getsize(path)}
    data=open(path,"rb").read()
    try:
        doc=m.PdfDocument.from_bytes(data)
        rec["pages"]=doc.page_count()
    except Exception as e:
        rec["load_error"]=str(e)[:160]; results[name]=rec
        print(f"{name:30s} LOAD FAIL {rec['load_error']}"); continue

    # markdown (whole doc) — the pymupdf4llm-equivalent
    md_box={}
    def run_md():
        d=m.PdfDocument.from_bytes(data); md_box["md"]=d.to_markdown_all()
    try:
        rec["t_markdown_s"]=round(timeit(run_md),4)
        md=md_box["md"]; rec["md_chars"]=len(md)
        open(os.path.join(OUT,f"{name}.pdfoxide.md"),"w").write(md)
    except Exception as e: rec["markdown_error"]=str(e)[:160]

    # plain text
    def run_plain():
        d=m.PdfDocument.from_bytes(data); md_box["plain"]=d.to_plain_text_all()
    try:
        rec["t_plain_s"]=round(timeit(run_plain),4)
        open(os.path.join(OUT,f"{name}.pdfoxide.txt"),"w").write(md_box["plain"])
    except Exception as e: rec["plain_error"]=str(e)[:160]

    # capabilities
    tabs,_=call_any(doc,["extract_tables"]);  rec["tables"]=len(tabs) if tabs is not None else None
    imgs,_=call_any(doc,["extract_images","page_images"]); rec["images"]=len(imgs) if imgs is not None else None
    try: rec["has_text_layer"]=doc.has_text_layer()
    except Exception: pass

    print(f"{name:30s} pg={rec.get('pages')} md={rec.get('t_markdown_s')}s "
          f"plain={rec.get('t_plain_s')}s chars={rec.get('md_chars')} "
          f"tables={rec.get('tables')} images={rec.get('images')}")
    results[name]=rec

json.dump(results, open(os.path.join(OUT,"results_pdfoxide.json"),"w"), indent=2)
print("\nsaved -> out/results_pdfoxide.json")
