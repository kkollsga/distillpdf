#!/usr/bin/env python3
"""Per-document regression sweep over benchmarking/bench100. Driven by scripts/regression-gate.sh,
which also runs the test suites and corpus goallines; see scripts/regression_gate/README.md for how to
use it and scripts/regression_gate/compare.py for what decides the verdict.

    gate.py freeze --run-dir D --stages F [--gt ACCURACY] --owner-note "why"   write the baseline
    gate.py check  --run-dir D --stages F [--gt ACCURACY] [--mode fast|full]   compare, exit 0/1/2/3
    gate.py name   --run-dir D --name N --reason R [--accept-drops]            name a run's changes

Exit codes: 0 pass, 1 quality drop or lost document or failed suite, 2 the gate could not run,
3 output changed with no drop and nobody has named it.
"""
from __future__ import annotations

import argparse
import collections
import concurrent.futures as cf
import datetime as dt
import difflib
import gzip
import hashlib
import json
import os
import platform
import re
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import compare as C  # noqa: E402
import score as S  # noqa: E402

ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
BASELINE = os.path.join(ROOT, "accuracy", "regression_gate_baseline.json")
NAMED = os.path.join(ROOT, "accuracy", "regression_gate_named_changes.json")
SCHEMA = 1

OPS = ("text", "pages", "markdown", "html", "links")
ENGINES = ("eager", "lazy")
OP_TIMEOUT_S = 900
DIFF_CAP_BYTES = 4 << 20

#: One to three documents per source, under 15 CPU-seconds in all, chosen so the fast sweep still
#: reaches what the full one exists to catch: two-column papers with many links and tables, a
#: predefined CMap paper, non-English law, ruled forms, an image-heavy report, RFC plain text.
FAST_SUBSET: tuple[str, ...] = (
    "cs_CL_2607.28545", "physics_optics_2607.28558", "stat_ML_2607.28248",
    "eu_law_eu_dma", "eu_law_nonenglish_eu_gdpr_de",
    "forms_irs_f1040", "forms_irs_f990",
    "gov_nasa_ntrs20160011963", "gov_nasa_ntrs20205000464",
    "gov_usgs_usgs70277728",
    "policy_worldbank_wbD34466172",
    "standards_ietf_rfc9457", "standards_ietf_rfc9562",
)


def local_root() -> str:
    """The checkout holding the gitignored corpus. A worktree has neither benchmarking/ nor
    dev-docs/, so it uses its main checkout's."""
    env = os.environ.get("GATE_LOCAL_ROOT")
    if env:
        return env
    common = subprocess.run(["git", "-C", ROOT, "rev-parse", "--path-format=absolute", "--git-common-dir"],
                            capture_output=True, text=True, check=True).stdout.strip()
    return os.path.dirname(common)


def gate_home() -> str:
    return os.environ.get("GATE_HOME") or os.path.join(ROOT, "target", "regression-gate")


def bench100_score():
    sys.path.insert(0, os.path.join(local_root(), "dev-docs", "bench", "scripts"))
    import bench100_score as b
    return b


def die(msg: str) -> "None":
    print(f"regression-gate: CANNOT RUN: {msg}", file=sys.stderr)
    raise SystemExit(2)


def sha_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def scorer_sha() -> str:
    """Scores from a changed scorer are not comparable to frozen ones: refuse, don't drift."""
    h = hashlib.sha256()
    for p in (os.path.join(HERE, "score.py"),
              os.path.join(local_root(), "dev-docs", "bench", "scripts", "bench100_score.py")):
        h.update(open(p, "rb").read())
    return h.hexdigest()[:16]


def git_info() -> dict:
    def git(*args):
        return subprocess.run(["git", "-C", ROOT, *args], capture_output=True, text=True).stdout.strip()
    source_paths = ("distillpdf", "distillpdf-python", "python", "Cargo.lock", "Cargo.toml")
    lock = open(os.path.join(ROOT, "Cargo.lock")).read()
    m = re.search(r'name = "lopdf"\nversion = "([^"]+)"\nsource = "[^"]*#([0-9a-f]+)"', lock)
    return {
        "commit": git("rev-parse", "--short", "HEAD"),
        "last_source_commit": git("log", "-1", "--format=%h", "--", *source_paths),
        "source_dirty": bool(git("status", "--porcelain", "--", *source_paths)),
        "lopdf": f"{m.group(1)}@{m.group(2)[:9]}" if m else "unknown",
    }


def machine_load() -> dict:
    out = {"loadavg_1m": round(os.getloadavg()[0], 1)}
    if sys.platform == "darwin":
        r = subprocess.run(["sysctl", "-n", "kern.memorystatus_level"], capture_output=True, text=True)
        out["memory_free_pct"] = int(r.stdout.strip() or 0)
    return out


# ------------------------------------------------------------------------- the corpus

def corpus_docs(slugs) -> dict:
    corpus = os.path.join(local_root(), "benchmarking", "bench100")
    manifest = os.path.join(corpus, "manifest.json")
    if not os.path.exists(manifest):
        die(f"no corpus at {corpus}: benchmarking/ is gitignored and lives in the main checkout "
            f"(set GATE_LOCAL_ROOT)")
    out = {}
    for m in json.load(open(manifest)):
        if slugs is not None and m["slug"] not in slugs:
            continue
        p = os.path.join(corpus, m["slug"] + ".pdf")
        if not os.path.exists(p):
            die(f"corpus document missing: {p}")
        out[m["slug"]] = {"path": p, "source": m["source"], "bytes": os.path.getsize(p)}
    if slugs is not None and set(slugs) - set(out):
        die(f"not in the corpus manifest: {sorted(set(slugs) - set(out))}")
    return out


# ---------------------------------------------------------------- the PyMuPDF reference

def _build_ref(args) -> tuple[str, str]:
    slug, pdf, path = args
    import fitz
    d = fitz.open(pdf)
    pages, uris, internal = [], [], 0
    for pno in range(d.page_count):
        page = d[pno]
        pages.append(page.get_text())
        for lk in page.get_links():
            if lk.get("uri"):  # freeze_refs.py's definition, same kinds
                uris.append([pno + 1, lk["uri"]])
            elif lk.get("kind") in (1, 4, 5):
                internal += 1
    blob = json.dumps({"pages": pages, "uris": uris, "internal": internal},
                      ensure_ascii=False).encode("utf-8", "surrogatepass")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = f"{path}.{os.getpid()}"
    with gzip.GzipFile(tmp, "wb", mtime=0) as f:
        f.write(blob)
    os.replace(tmp, path)
    return slug, hashlib.sha256(blob).hexdigest()[:16]


def reference_shas(docs: dict, workers: int) -> dict:
    """PyMuPDF's text and links per document, cached outside the repo because it is a copy of
    third-party text. Regenerated when absent: same pinned PyMuPDF, same bytes, and the baseline's
    hash proves it."""
    cache = os.environ.get("GATE_REF_CACHE") or os.path.join(gate_home(), "pymupdf-ref")
    out, todo = {}, []
    for slug, d in docs.items():
        path = os.path.join(cache, slug + ".json.gz")
        d["ref_path"] = path
        if os.path.exists(path):
            with gzip.open(path, "rb") as f:
                out[slug] = hashlib.sha256(f.read()).hexdigest()[:16]
        else:
            todo.append((slug, d["path"], path))
    if todo:
        print(f"building the PyMuPDF reference for {len(todo)} documents", flush=True)
        with cf.ProcessPoolExecutor(workers) as ex:
            out.update(dict(ex.map(_build_ref, todo)))
    return out


# -------------------------------------------------------------------------- the sweep

def _run_op(job: dict) -> dict:
    cmd = [sys.executable, "-I", os.path.join(HERE, "runner.py"), "--pdf", job["pdf"],
           "--op", job["op"], "--engine", job["engine"]]
    if job.get("out"):
        os.makedirs(os.path.dirname(job["out"]), exist_ok=True)
        cmd += ["--out", job["out"]]
    t = time.perf_counter()
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=OP_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        return {**job, "crash": f"timeout after {OP_TIMEOUT_S}s"}
    if p.returncode != 0:
        return {**job, "crash": f"exit {p.returncode}: {p.stderr[-300:]}", "wall_s": time.perf_counter() - t}
    try:
        return {**job, **json.loads(p.stdout.splitlines()[-1])}
    except (IndexError, ValueError):
        return {**job, "crash": f"no measurement line: {p.stdout[-200:]} {p.stderr[-200:]}"}


def run_jobs(jobs: list[dict], workers: int, samples: dict, label: str) -> None:
    done, start = 0, time.perf_counter()
    with cf.ThreadPoolExecutor(workers) as ex:
        for r in ex.map(_run_op, jobs):
            samples.setdefault((r["slug"], r["engine"], r["op"]), []).append(r)
            done += 1
            if done % 200 == 0:
                print(f"  {label}: {done}/{len(jobs)} in {time.perf_counter() - start:.0f}s", flush=True)


def _band(values: list[float], nd: int) -> list:
    return [round(statistics.median(values), nd), round(min(values), nd), round(max(values), nd)]


def finalize(samples: dict, docs: dict) -> dict:
    """Medians over repeats, and any repeat whose bytes disagree with the first: the library's
    output is contractually deterministic, so that is a finding, not noise."""
    out: dict = {}
    for (slug, engine, op), rows in samples.items():
        first = rows[0]
        rec = {k: first[k] for k in ("sha", "chars", "page_count", "route", "page_hashes",
                                     "empty_pages", "error", "crash") if k in first}
        good = [r for r in rows if "crash" not in r and "error" not in r and "wall_s" in r]
        if good:
            m: dict = {"n": len(good),
                       "wall": _band([r["wall_s"] for r in good], 3),
                       "cpu": _band([r["cpu_s"] for r in good], 3)}
            if good[0].get("peak_footprint"):
                m["peak"] = _band([r["peak_footprint"] / 2**20 for r in good], 1)
                m["import"] = round(good[0]["import_footprint"] / 2**20, 1)
            if good[0].get("instructions"):
                m["instr"] = _band([r["instructions"] / 1e6 for r in good], 0)
            rec["m"] = m
        doc = out.setdefault(slug, {"source": docs[slug]["source"], "bytes": docs[slug]["bytes"], "ops": {}})
        doc["ops"].setdefault(engine, {})[op] = rec
        shas = {r.get("sha") for r in rows if "sha" in r}
        if len(shas) > 1:
            doc.setdefault("nondeterministic", []).append(f"{engine}/{op}")
    return out


def _score_one(args) -> tuple[str, str, dict]:
    slug, engine, out_dir, ref_path = args
    b = bench100_score()
    return slug, engine, S.score_document(out_dir, S.load_reference(ref_path), b.html_grids, b._body)


def score_run(docs_now: dict, docs: dict, outputs: str, baseline: dict | None, workers: int) -> None:
    """Scores are a pure function of output and reference, so an output unchanged from the
    baseline keeps its frozen score and only changed documents cost any scoring time. The
    lazy engine's bytes usually equal eager's, and then the eager score is the lazy score."""
    jobs = []
    for slug, d in docs_now.items():
        d["scores"] = {}
        bd = (baseline or {"docs": {}})["docs"].get(slug)
        for engine in d["ops"]:
            same = (bd is not None and engine in bd.get("scores", {}) and engine in bd["ops"]
                    and all(bd["ops"][engine].get(op, {}).get("sha") == r.get("sha")
                            for op, r in d["ops"][engine].items()))
            if same:
                d["scores"][engine] = dict(bd["scores"][engine])
            elif (engine == "lazy" and "eager" in d["ops"]
                  and all(d["ops"]["eager"].get(op, {}).get("sha") == r.get("sha")
                          for op, r in d["ops"]["lazy"].items())):
                d["scores"]["lazy"] = "same_as_eager"
            else:
                jobs.append((slug, engine, os.path.join(outputs, slug, engine), docs[slug]["ref_path"]))
    if jobs:
        print(f"scoring {len(jobs)} document outputs", flush=True)
        with cf.ProcessPoolExecutor(workers) as ex:
            for slug, engine, s in ex.map(_score_one, jobs):
                docs_now[slug]["scores"][engine] = s
    for d in docs_now.values():
        if d["scores"].get("lazy") == "same_as_eager":
            d["scores"]["lazy"] = d["scores"].get("eager", {})


def attach_gt(docs_now: dict, accuracy_path: str | None) -> None:
    """Blind-judge page scores from bench100_score's own run (the floors stage), per document.
    Without that run the gt_ columns are marked not-run rather than read as zero."""
    if not accuracy_path or not os.path.exists(accuracy_path):
        for d in docs_now.values():
            for s in d["scores"].values():
                s["_gt_not_run"] = True
        return
    by_doc = collections.defaultdict(list)
    for row in json.load(open(accuracy_path))["pages"]:
        by_doc[row["slug"]].append(row)
    keys = {"gt_headings": "dpacc_headings", "gt_tables": "dpacc_tables",
            "gt_paragraphs": "dpacc_paragraphs", "gt_lists": "dpacc_lists",
            "gt_images": "dpacc_images", "gt_fulldoc_headings": "dpacc_fulldoc_headings"}
    for slug, d in docs_now.items():
        rows = by_doc.get(slug, [])
        gt = {}
        for k, src in keys.items():
            vals = [r[src] for r in rows if r.get(src) is not None]
            if vals:
                gt[k] = round(statistics.mean(vals), 4)
        for s in d["scores"].values():
            s.pop("_gt_not_run", None)
            # A reused frozen score carries the old gt_ values. Clear them, or a page the scorer
            # can no longer score would keep reading as its baseline number.
            for k in keys:
                s.pop(k, None)
            s.update(gt)


def do_sweep(a, baseline: dict | None) -> tuple[dict, dict, dict, dict]:
    if a.docs:
        slugs = set(a.docs.split(","))
    elif a.mode == "fast":
        slugs = set(FAST_SUBSET)
    else:
        slugs = None
    docs = corpus_docs(slugs)
    outputs = os.path.join(a.run_dir, "outputs")
    engines = a.engines.split(",")
    samples: dict = {}
    print(f"sweep: {len(docs)} documents x {len(engines)} engines x {len(OPS)} operations, "
          f"{a.repeats} repeat(s), {a.workers} workers", flush=True)
    t0 = time.perf_counter()
    for round_no in range(a.repeats):
        jobs = [{"slug": s, "pdf": d["path"], "engine": e, "op": o,
                 **({"out": os.path.join(outputs, s, e, o)} if round_no == 0 else {})}
                for s, d in docs.items() for e in engines for o in OPS]
        run_jobs(jobs, a.workers, samples, f"round {round_no + 1}")
    now = finalize(samples, docs)
    print(f"sweep took {time.perf_counter() - t0:.0f}s", flush=True)
    return now, docs, samples, {"outputs": outputs, "sweep_s": round(time.perf_counter() - t0)}


def run_meta(a, extra: dict) -> dict:
    import distillpdf
    so = [f for f in os.listdir(os.path.dirname(distillpdf.__file__)) if f.startswith("_distillpdf")]
    return {
        "utc": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        **git_info(),
        "distillpdf_version": distillpdf.__version__,
        "distillpdf_at": distillpdf.__file__,
        "wheel_so_sha16": sha_file(os.path.join(os.path.dirname(distillpdf.__file__), so[0]))[:16],
        "platform": f"{platform.system()} {platform.machine()} {platform.python_version()}",
        "repeats": a.repeats, "workers": a.workers, "mode": a.mode,
        "engines": a.engines.split(","),
        "load": machine_load(),
        **extra,
    }


def read_stages(path: str | None) -> list[dict]:
    if not path or not os.path.exists(path):
        return []
    rows = []
    for line in open(path):
        name, status, secs, log = line.rstrip("\n").split("\t")
        rows.append({"stage": name, "status": status, "seconds": int(secs), "log": log})
    return rows


# ------------------------------------------------------------------------ the baseline

def compact_doc(doc: dict) -> dict:
    """Drop what can be rebuilt: lazy page hashes equal to eager's, lazy scores equal to eager's."""
    d = json.loads(json.dumps(doc))
    ops = d["ops"]
    if "lazy" in ops and "eager" in ops:
        lp, ep = ops["lazy"].get("pages", {}), ops["eager"].get("pages", {})
        if lp.get("page_hashes") == ep.get("page_hashes"):
            lp.pop("page_hashes", None)
    if d.get("scores", {}).get("lazy") == d.get("scores", {}).get("eager"):
        d["scores"]["lazy"] = "same_as_eager"
    return d


def expand_doc(d: dict) -> dict:
    ops = d["ops"]
    if "lazy" in ops and "eager" in ops:
        lp, ep = ops["lazy"].get("pages"), ops["eager"].get("pages")
        if lp is not None and "page_hashes" not in lp and ep and lp.get("sha") == ep.get("sha"):
            lp["page_hashes"] = ep.get("page_hashes")
    if d.get("scores", {}).get("lazy") == "same_as_eager":
        d["scores"]["lazy"] = d["scores"]["eager"]
    return d


def write_baseline(meta: dict, docs_now: dict, path: str = BASELINE) -> None:
    # Nothing machine-local goes in a committed file: this checkout's paths mean nothing elsewhere.
    meta = {k: v for k, v in meta.items() if k != "distillpdf_at"}
    meta["stages"] = [{k: v for k, v in s.items() if k != "log"} for s in meta.get("stages", [])]
    with open(path, "w", encoding="utf-8") as f:
        f.write("{\n" + json.dumps("meta") + ": " + json.dumps(meta, indent=1, sort_keys=True) + ",\n")
        f.write('"docs": {\n')
        rows = [json.dumps(s) + ": " + json.dumps(compact_doc(docs_now[s]), sort_keys=True,
                                                  separators=(",", ":"))
                for s in sorted(docs_now)]
        f.write(",\n".join(rows) + "\n}\n}\n")


def load_baseline(path: str = BASELINE) -> dict:
    if not os.path.exists(path):
        die(f"no baseline at {path}. It is committed; a new one comes from `freeze` with an owner note.")
    b = json.load(open(path))
    for d in b["docs"].values():
        expand_doc(d)
    return b


def load_named() -> list[dict]:
    if not os.path.exists(NAMED):
        return []
    return json.load(open(NAMED))["named_changes"]


def cmd_freeze(a) -> int:
    if os.path.exists(BASELINE) and not a.owner_note:
        die("a baseline exists. Replacing it needs --owner-note \"<why>\": a re-freeze resets what "
            "every later change is judged against, and git history is its only record.")
    now, docs, _samples, extra = do_sweep(a, None)
    shas = reference_shas(docs, a.workers)
    for slug, d in now.items():
        d["ref_sha"] = shas[slug]
        d["file_sha"] = sha_file(docs[slug]["path"])[:16]
    score_run(now, docs, extra["outputs"], None, a.workers)
    attach_gt(now, a.gt)
    fitz_version = __import__("fitz").__version__
    run = {"docs": now}
    meta = run_meta(a, {
        "what": "Frozen per-document behaviour of distillPDF. Hashes, scores and resource "
                "medians only: no document text. See scripts/regression_gate/README.md.",
        "owner_note": a.owner_note,
        "scorer_sha": scorer_sha(), "pymupdf": fitz_version,
        "stages": read_stages(a.stages),
        "sweep_s": extra["sweep_s"],
        "noise": C.noise_summary(run),
        "totals": C.totals(now),
        "documents": len(now),
        "pages": sum(max((r.get("page_count", 0) for r in d["ops"]["eager"].values()), default=0)
                     for d in now.values()),
        "fast_subset": sorted(FAST_SUBSET),
    })
    write_baseline(meta, now)
    json.dump({"meta": meta, "docs": now}, open(os.path.join(a.run_dir, "results.json"), "w"))
    nd = [s for s, d in now.items() if d.get("nondeterministic")]
    print(f"baseline written: {BASELINE} ({os.path.getsize(BASELINE) // 1024} KiB), "
          f"{meta['documents']} documents, {meta['pages']} pages")
    print(f"full outputs: {extra['outputs']}")
    if nd:
        print(f"NON-DETERMINISTIC output on: {nd}")
    return 0


# --------------------------------------------------------------------- the diff, for reading

def _read(path: str) -> str | None:
    try:
        with open(path, encoding="utf-8", errors="surrogatepass") as f:
            return f.read()
    except OSError:
        return None


def word_delta(a: str, b: str) -> tuple[list, list]:
    ca, cb = collections.Counter(S.norm_words(a)), collections.Counter(S.norm_words(b))
    return (sorted((ca - cb).items(), key=lambda kv: -kv[1]),
            sorted((cb - ca).items(), key=lambda kv: -kv[1]))


def readable_excerpt(diff_text: str, limit: int = 14, width: int = 140) -> list[str]:
    """The first changed lines, cut around where each pair first differs. HTML output is one long
    line per page, so the start of a changed line is usually identical and shows nothing."""
    lines = [ln for ln in diff_text.splitlines()
             if not ln.startswith(("diff --git", "index ", "--- ", "+++ ", "\\ No newline"))]
    out = []
    for i, ln in enumerate(lines):
        if len(out) >= limit:
            break
        other = None
        if ln.startswith("-") and i + 1 < len(lines) and lines[i + 1].startswith("+"):
            other = lines[i + 1]
        elif ln.startswith("+") and i > 0 and lines[i - 1].startswith("-"):
            other = lines[i - 1]
        if ln.startswith(" "):
            continue  # context carries nothing an excerpt needs
        if other is None:
            out.append(ln[:width])
            continue
        at = next((k for k, (x, y) in enumerate(zip(ln[1:], other[1:])) if x != y),
                  min(len(ln), len(other)) - 1)
        start = max(0, at - 40)
        out.append(ln[0] + ("..." if start else "") + ln[1 + start:1 + start + width])
    return out


def capped_git_diff(a: str, b: str, cap: int = DIFF_CAP_BYTES) -> bytes:
    """`git diff` stopped at `cap` bytes, with a line saying so. HTML with vector figures is one
    line per page: two new figures in the 383-page document diffed to 139 MiB."""
    p = subprocess.Popen(["git", "diff", "--no-index", "--no-color", "-U1", a, b], stdout=subprocess.PIPE)
    raw = p.stdout.read(cap + 1)
    p.stdout.close()
    p.kill()
    p.wait()
    if len(raw) > cap:
        return raw[:cap] + f"\n[diff cut at {cap} bytes by the regression gate]\n".encode()
    return raw


def write_diff(base_root: str, run_root: str, slug: str, engine: str, op: str, diffs: str,
               pages: list[int]) -> dict:
    """A diff file and a short excerpt, or a plain statement that it cannot be produced."""
    bp, np_ = os.path.join(base_root, slug, engine, op), os.path.join(run_root, slug, engine, op)
    base_text, now_text = _read(bp), _read(np_)
    if base_text is None or now_text is None:
        return {"note": f"no readable diff: baseline outputs not at {bp} "
                        "(set GATE_BASELINE_OUTPUTS; see scripts/regression_gate/README.md to regenerate)"}
    out = os.path.join(diffs, f"{slug}.{engine}.{op}.diff")
    os.makedirs(diffs, exist_ok=True)
    if op == "pages":
        lines = []
        bj, nj = json.loads(base_text), json.loads(now_text)
        for p in pages:
            a = bj[p - 1] if p - 1 < len(bj) else ""
            b = nj[p - 1] if p - 1 < len(nj) else ""
            lines += difflib.unified_diff(a.splitlines(), b.splitlines(), f"baseline page {p}",
                                          f"this build page {p}", lineterm="", n=1)
        body = "\n".join(lines) + "\n"
        with open(out, "w", encoding="utf-8", errors="surrogatepass") as f:
            f.write(body)
    else:
        raw = capped_git_diff(bp, np_)
        with open(out, "wb") as f:
            f.write(raw)
        body = raw.decode("utf-8", "replace")
    gone, new = word_delta(base_text, now_text)
    excerpt = readable_excerpt(body)
    return {"path": out, "excerpt": excerpt, "words_gone": sum(v for _, v in gone),
            "words_new": sum(v for _, v in new), "top_gone": gone[:8], "top_new": new[:8]}


# ----------------------------------------------------------------------------- the report

def fmt_ops(rows) -> str:
    return ", ".join(f"{r[0]}/{r[1]}" for r in rows)


def build_report(res: dict, baseline: dict, run: dict, stages: list[dict], diff_info: dict,
                 fast: bool) -> list[str]:
    bm, rm = baseline["meta"], run["meta"]
    n_docs = len(run["docs"])
    L = []
    w = L.append
    w(f"# regression gate: {res['final_verdict']}")
    w("")
    w(f"baseline {bm['commit']} (source {bm['last_source_commit']}, lopdf {bm['lopdf']}, frozen {bm['utc']}); "
      f"this build {rm['commit']}{' DIRTY' if rm['source_dirty'] else ''} "
      f"(source {rm['last_source_commit']}, lopdf {rm['lopdf']}); mode {rm['mode']}, "
      f"{n_docs} of {len(baseline['docs'])} documents, {rm['repeats']} repeat(s)")
    w("")
    w("## Suites and goallines")
    for s in stages:
        w(f"- {s['stage']}: **{s['status']}** ({s['seconds']}s) {s['log']}")
    if not stages:
        w("- none ran")
    w("")
    w("## Documents")
    w(f"- identical in every output: **{len(res['identical'])}** of {n_docs}")
    w(f"- changed: **{len(res['changed'])}** ({len(res['unnamed'])} unnamed, {len(res['named'])} named)")
    w(f"- lost output: **{len(res['lost'])}**")
    w(f"- with a score below baseline: **{len(res['drops'])}** ({len(res['unnamed_drops'])} unnamed)")
    if res["missing"]:
        w(f"- MISSING from this run: {res['missing']}")
    w("")
    if res["lost"]:
        w("## LOST")
        for s, rows in sorted(res["lost"].items()):
            for r in rows:
                w(f"- `{s}`: {r}")
        w("")
    if res["drops"]:
        w("## Score drops, per document")
        w("| document | engine | score | baseline | now | named |")
        w("|---|---|---|--:|--:|---|")
        named = {(s, *r[:4]): r[4] for s, rows in res["named_drops"].items() for r in rows}
        for s, rows in sorted(res["drops"].items()):
            for r in rows:
                w(f"| `{s}` | {r[0]} | {r[1]} | {r[2]} | {r[3]} | {named.get((s, *r), '**unnamed**')} |")
        w("")
    if res["gains"]:
        w("## Score gains")
        for s, rows in sorted(res["gains"].items()):
            w(f"- `{s}`: " + ", ".join(f"{e}/{k} {x} -> {y}" for e, k, x, y in rows))
        w("")
    if res["changed"]:
        w("## Changed documents")
        names = {(s, r[0], r[1]): r[4] for s, rows in res["named"].items() for r in rows}
        for s, info in sorted(res["changed"].items()):
            pages = info["pages"].get("eager") or info["pages"].get("lazy") or []
            w(f"### `{s}`")
            w(f"- outputs: {fmt_ops(info['ops'])}")
            if pages:
                w(f"- changed pages ({len(pages)}): {pages[:20]}{' ...' if len(pages) > 20 else ''}")
            for c in info["ops"]:
                w(f"- {c[0]}/{c[1]}: {names.get((s, c[0], c[1]), '**unnamed**')}")
                d = diff_info.get((s, c[0], c[1]))
                if not d:
                    continue
                if "note" in d:
                    w(f"  - {d['note']}")
                    continue
                w(f"  - {d['words_gone']} words gone {d['top_gone'][:6]}, "
                  f"{d['words_new']} words new {d['top_new'][:6]}")
                w(f"  - full diff: {d['path']}")
                w("```diff")
                L.extend(d["excerpt"])
                w("```")
        w("")
    if res["route_changes"]:
        w("## Engine route changes (a document moved between lazy and its eager fallback)")
        for r in res["route_changes"][:40]:
            w(f"- `{r[0]}` {r[1]}/{r[2]}: {r[3]} -> {r[4]}")
        w("")
    nd = [s for s, d in run["docs"].items() if d.get("nondeterministic")]
    if nd:
        w("## Non-deterministic output in this run")
        for s in nd:
            w(f"- `{s}`: {run['docs'][s]['nondeterministic']}")
        w("")
    parity = engine_parity(run)
    w("## Engine parity (eager vs lazy, this build)")
    w(f"- {len(parity)} document(s) differ between engines" + (f": {parity[:20]}" if parity else ""))
    b_parity = engine_parity(baseline)
    if set(parity) != set(b_parity):
        w(f"- baseline had {len(b_parity)}; newly differing: {sorted(set(parity) - set(b_parity))}; "
          f"newly equal: {sorted(set(b_parity) - set(parity))}")
    fallback = sorted(s for s, d in run["docs"].items()
                      if "fallback" in (d["ops"].get("lazy", {}).get("text", {}).get("route") or ""))
    w(f"- {len(fallback)} document(s) use the lazy engine's eager fallback" + (f": {fallback}" if fallback else ""))
    w("")
    w("## Footprint and time (reported, never decide the verdict)")
    w(f"Tolerance: footprint +{round((C.FOOTPRINT_RATIO - 1) * 100)}% and +{C.FOOTPRINT_ABS_MIB:g} MiB; "
      f"instructions +{round((C.INSTRUCTIONS_RATIO - 1) * 100)}% and +{C.INSTRUCTIONS_ABS_M} M; "
      f"each also above the baseline's own repeat maximum. Flagged rows were re-measured to 3 repeats.")
    w(f"Machine at start of this run: {rm['load']}. Baseline: {bm['load']}.")
    w(f"Repeat noise, (max-min)/median p50/p95. Baseline: {fmt_noise(bm.get('noise'))}. "
      f"This run: {fmt_noise(rm.get('noise')) or 'one repeat, no measure of noise'}.")
    slugs = set(run["docs"])
    tb, tn = C.totals(baseline["docs"], slugs)["ops"], C.totals(run["docs"], slugs)["ops"]
    w("| operation | cpu s baseline | now | Minstr baseline | now | wall s baseline | now | max peak MiB baseline | now |")
    w("|---|--:|--:|--:|--:|--:|--:|--:|--:|")
    for k in sorted(tn):
        x, y = tb.get(k, {}), tn[k]
        w(f"| {k} | {x.get('cpu_s')} | {y['cpu_s']} | {x.get('Minstr')} | {y['Minstr']} | "
          f"{x.get('wall_s')} | {y['wall_s']} | {x.get('peak_mib_max')} | {y['peak_mib_max']} |")
    for label, key, unit in (("Footprint growth", "footprint", "MiB"), ("Time growth", "time", "")):
        rows = res[key]
        w(f"\n**{label}: {len(rows)} document(s).**")
        for s, items in sorted(rows.items())[:40]:
            for it in items:
                w(f"- `{s}` {it[0]}/{it[1]}: {it[-2]} -> {it[-1]} {unit or it[2]}")
    return L


def fmt_noise(n: dict | None) -> str:
    if not n:
        return ""
    return "; ".join(f"{k} {v['p50'] * 100:.1f}%/{v['p95'] * 100:.1f}%" for k, v in n.items())


def engine_parity(run: dict) -> list[str]:
    out = []
    for s, d in run["docs"].items():
        e, z = d["ops"].get("eager"), d["ops"].get("lazy")
        if e and z and any(e.get(o, {}).get("sha") != z.get(o, {}).get("sha") for o in z):
            out.append(s)
    return sorted(out)


def console_summary(L: list[str], report_path: str) -> None:
    """The first screenful. Everything else is in the report file."""
    keep, in_changed = [], False
    for ln in L:
        if ln.startswith("## Changed documents"):
            in_changed = True
            continue
        if in_changed:
            if ln.startswith("## "):
                in_changed = False
            else:
                continue
        keep.append(ln)
    print("\n".join(keep[:70]))
    if len(keep) > 70:
        print(f"... {len(keep) - 70} more lines")
    print(f"\nreport: {report_path}")


# --------------------------------------------------------------------------- the commands

def cmd_check(a) -> int:
    baseline = load_baseline()
    if baseline["meta"]["scorer_sha"] != scorer_sha():
        die("the scorer changed since the baseline froze (scripts/regression_gate/score.py or "
            "bench100_score.py). Frozen scores no longer mean the same thing: re-freeze with an "
            "owner note, or restore the scorer.")
    pymupdf = __import__("fitz").__version__
    if baseline["meta"]["pymupdf"] != pymupdf:
        die(f"PyMuPDF {pymupdf} is not the reference the baseline froze with ({baseline['meta']['pymupdf']}).")
    now, docs, samples, extra = do_sweep(a, baseline)
    shas = reference_shas(docs, a.workers)
    for slug, d in now.items():
        bd = baseline["docs"].get(slug)
        if bd is None:
            die(f"{slug} is in this corpus and not in the baseline")
        if sha_file(docs[slug]["path"])[:16] != bd["file_sha"]:
            die(f"corpus drift: {slug} is not the file the baseline froze")
        if shas[slug] != bd["ref_sha"]:
            die(f"reference drift on {slug}: PyMuPDF text differs from the frozen reference")
    score_run(now, docs, extra["outputs"], baseline, a.workers)
    attach_gt(now, a.gt)
    run = {"docs": now, "scope": "all" if a.mode == "full" and not a.docs else "subset"}
    from_bench = bench100_score()
    res = C.compare(baseline, run, load_named(), from_bench.table_score)

    # One round cannot tell a growth from a busy minute: re-measure just the flagged operations
    # to three repeats before reporting them.
    flagged = {(s, it[0], it[1]) for key in ("footprint", "time") for s, items in res[key].items()
               for it in items}
    todo = [{"slug": s, "pdf": docs[s]["path"], "engine": e, "op": o}
            for s, e, o in flagged for _ in range(max(0, 3 - a.repeats))]
    if todo:
        print(f"re-measuring {len(flagged)} flagged operations to 3 repeats", flush=True)
        run_jobs(todo, a.workers, samples, "confirm")
        confirmed = finalize(samples, docs)
        for s, d in now.items():
            for e, ops in d["ops"].items():
                for o, r in ops.items():
                    r["m"] = confirmed[s]["ops"][e][o].get("m", r.get("m"))
        res = C.compare(baseline, run, load_named(), from_bench.table_score)

    stages = read_stages(a.stages)
    bad = [s for s in stages if s["status"] == "FAILED"]
    meta = run_meta(a, {"sweep_s": extra["sweep_s"], "noise": C.noise_summary({"docs": now})})
    run["meta"] = meta
    res["final_verdict"], code = res["verdict"], res["exit"]
    if bad:
        res["final_verdict"], code = "FAIL", 1

    base_root = os.environ.get("GATE_BASELINE_OUTPUTS") or os.path.join(gate_home(), "baseline-outputs")
    diffs = os.path.join(a.run_dir, "diffs")
    diff_info = {}
    for s, info in res["changed"].items():
        for c in info["ops"]:
            diff_info[(s, c[0], c[1])] = write_diff(base_root, extra["outputs"], s, c[0], c[1], diffs,
                                                    info["pages"].get(c[0], []))
    lines = build_report(res, baseline, run, stages, diff_info, a.mode == "fast")
    report = os.path.join(a.run_dir, "report.md")
    with open(report, "w", encoding="utf-8", errors="replace") as f:
        f.write("\n".join(lines) + "\n")
    json.dump({"meta": meta, "docs": now, "verdict": res["final_verdict"],
               "changed": {s: i["ops"] for s, i in res["changed"].items()},
               "unnamed": res["unnamed"], "unnamed_drops": res["unnamed_drops"],
               "lost": res["lost"]},
              open(os.path.join(a.run_dir, "results.json"), "w"))
    console_summary(lines, report)
    print(f"exit {code}: {res['final_verdict']}")
    return code


def cmd_name(a) -> int:
    """Record exactly what a finished run changed, under a name and a reason. Later runs match
    the entry by exact old and new hash only, and every report still lists it."""
    if not a.reason.strip() or not a.name.strip():
        die("--name and --reason must say something")
    results = json.load(open(os.path.join(a.run_dir, "results.json")))
    if results.get("lost"):
        die(f"this run lost output on {sorted(results['lost'])}. Lost documents cannot be named: fix "
            "them, or re-freeze with an owner note.")
    if results.get("unnamed_drops") and not a.accept_drops:
        die(f"this run dropped scores on {sorted(results['unnamed_drops'])}. Naming a drop takes "
            "--accept-drops: it is a decision to accept a worse number, so it has to be said.")
    keep = set(a.docs.split(",")) if a.docs else None
    entry = {"name": a.name, "reason": a.reason,
             "utc": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
             "commit": results["meta"]["commit"], "changes": {}, "accepted_drops": {}}
    for slug, rows in results.get("unnamed", {}).items():
        if keep is None or slug in keep:
            entry["changes"][slug] = [{"engine": r[0], "op": r[1], "from": r[2], "to": r[3]} for r in rows]
    for slug, rows in results.get("unnamed_drops", {}).items():
        if keep is None or slug in keep:
            entry["accepted_drops"][slug] = [{"engine": r[0], "metric": r[1], "from": r[2], "to": r[3]}
                                             for r in rows]
    if not entry["changes"] and not entry["accepted_drops"]:
        die("nothing unnamed in that run")
    data = {"what": "Named output changes against accuracy/regression_gate_baseline.json. Matched "
                    "by exact hash pair; a re-freeze empties this file.", "named_changes": load_named()}
    data["named_changes"].append(entry)
    with open(NAMED, "w") as f:
        json.dump(data, f, indent=1)
        f.write("\n")
    print(f"named {len(entry['changes'])} document(s) and {len(entry['accepted_drops'])} drop(s) "
          f"as {a.name!r} in {NAMED}")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("freeze", "check"):
        p = sub.add_parser(name)
        p.add_argument("--run-dir", required=True)
        p.add_argument("--mode", choices=["fast", "full"], default="full")
        p.add_argument("--repeats", type=int, default=3)
        p.add_argument("--workers", type=int, default=4)
        p.add_argument("--engines", default=",".join(ENGINES))
        p.add_argument("--docs", default="", help="comma-separated slugs (debugging)")
        p.add_argument("--stages", default="")
        p.add_argument("--gt", default="", help="bench100_accuracy.json from this build's floors stage")
        p.add_argument("--owner-note", default="")
    p = sub.add_parser("name")
    p.add_argument("--run-dir", required=True)
    p.add_argument("--name", required=True)
    p.add_argument("--reason", required=True)
    p.add_argument("--accept-drops", action="store_true")
    p.add_argument("--docs", default="")
    a = ap.parse_args()
    if a.cmd != "name":
        os.makedirs(a.run_dir, exist_ok=True)
    return {"freeze": cmd_freeze, "check": cmd_check, "name": cmd_name}[a.cmd](a)


if __name__ == "__main__":
    raise SystemExit(main())
