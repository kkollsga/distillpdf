#!/usr/bin/env python3
"""Fail-closed provenance and table-only timing runner for the local Phase-1 gate."""
from __future__ import annotations

import argparse
import hashlib
import importlib.metadata as metadata
import json
import os
import platform
import statistics
import subprocess
import sys
import sysconfig
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
LOCK = json.loads((HERE / "lock.json").read_text())
MANIFEST_PATH = HERE / "corpus_manifest.json"
MANIFEST = json.loads(MANIFEST_PATH.read_text())


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def tree_sha256(root: Path, pattern: str = "*") -> str:
    h = hashlib.sha256()
    for path in sorted(p for p in root.glob(pattern) if p.is_file()):
        h.update(path.relative_to(root).as_posix().encode())
        h.update(b"\0")
        h.update(path.read_bytes())
    return h.hexdigest()


def locate_corpus() -> Path:
    override = os.environ.get("TABLE_CORPUS_ROOT")
    if override:
        return Path(override).resolve()
    try:
        dist = metadata.distribution("pdf-parse-bench")
    except metadata.PackageNotFoundError as exc:
        raise SystemExit("pdf-parse-bench is absent; set TABLE_CORPUS_ROOT") from exc
    return Path(dist.locate_file("pdf_parse_bench/data/2026-q1-tables-only")).resolve()


def verify_manifest(corpus: Path) -> dict:
    failures = []
    for folder, key in (("pdfs", "pdfs"), ("ground_truth", "ground_truth")):
        actual_names = {p.name for p in (corpus / folder).iterdir() if p.is_file()}
        expected_names = {r["file"] for r in MANIFEST[key]}
        if actual_names != expected_names:
            failures.append(f"{folder} names differ: missing={sorted(expected_names-actual_names)}, "
                            f"extra={sorted(actual_names-expected_names)}")
        for rec in MANIFEST[key]:
            path = corpus / folder / rec["file"]
            if not path.is_file() or path.stat().st_size != rec["bytes"] or sha256(path) != rec["sha256"]:
                failures.append(f"{folder}/{rec['file']} bytes/hash mismatch")
    if failures:
        raise SystemExit("corpus manifest verification failed:\n  " + "\n  ".join(failures))
    return {
        "manifest_sha256": sha256(MANIFEST_PATH),
        "pdf_tree_sha256": tree_sha256(corpus / "pdfs", "*.pdf"),
        "ground_truth_tree_sha256": tree_sha256(corpus / "ground_truth", "*.json"),
        "pdfs": len(MANIFEST["pdfs"]),
        "ground_truth_files": len(MANIFEST["ground_truth"]),
    }


def installed_version(name: str) -> str:
    try:
        return metadata.version(name)
    except metadata.PackageNotFoundError:
        return "absent"


def verify_packages(extractors: list[str], full: bool) -> dict:
    required = {"pymupdf": "PyMuPDF", "pdfplumber": "pdfplumber",
                "pymupdf4llm": "pymupdf4llm"}
    versions = {name: installed_version(name) for name in LOCK["packages"]}
    versions["distillpdf"] = installed_version("distillpdf")
    versions["pymupdf-layout"] = installed_version("pymupdf-layout")
    if full and versions["pymupdf-layout"] != "absent":
        raise SystemExit("pymupdf-layout is installed but the frozen reference excludes it")
    selected = {required[x] for x in extractors if x in required}
    if full:
        selected = set(LOCK["packages"])
    proof = {}
    for package in sorted(selected):
        pinned = LOCK["packages"][package]
        if versions[package] != pinned["version"]:
            raise SystemExit(f"{package}=={versions[package]} installed; pinned "
                             f"{pinned['version']} required")
        dist = metadata.distribution(package)
        try:
            direct = json.loads(dist.read_text("direct_url.json") or "")
        except (json.JSONDecodeError, FileNotFoundError) as exc:
            raise SystemExit(f"{package} has no direct_url.json; pinned artifact is unproved") from exc
        got = direct.get("archive_info", {}).get("hash")
        want = "sha256=" + pinned["sha256"]
        if got != want or not direct.get("url", "").endswith("/" + pinned["artifact"]):
            raise SystemExit(f"{package} direct artifact does not match lock: "
                             f"url={direct.get('url')!r}, hash={got!r}")
        proof[package] = {"version": versions[package], "artifact": pinned["artifact"],
                          "sha256": pinned["sha256"], "direct_url": direct["url"]}
    return {"versions": versions, "artifacts": proof}


def verify_wheel(wheel: Path) -> dict:
    if not wheel.is_file():
        raise SystemExit(f"wheel does not exist: {wheel}")
    version = installed_version("distillpdf")
    if version == "absent" or f"distillpdf-{version}" not in wheel.name:
        raise SystemExit(f"installed distillpdf {version} is not proved by wheel {wheel.name}")
    dist = metadata.distribution("distillpdf")
    try:
        direct = json.loads(dist.read_text("direct_url.json") or "")
    except (json.JSONDecodeError, FileNotFoundError) as exc:
        raise SystemExit("installed distillpdf has no valid direct_url.json; exact wheel "
                         "provenance is unproved") from exc
    recorded = direct.get("archive_info", {}).get("hash", "")
    expected_hash = "sha256=" + sha256(wheel)
    installed_url = direct.get("url", "")
    if recorded != expected_hash:
        raise SystemExit(f"installed distillpdf archive hash {recorded!r}; expected "
                         f"{expected_hash!r} from supplied wheel")
    if not installed_url.endswith("/" + wheel.name):
        raise SystemExit(f"installed distillpdf direct URL {installed_url!r} does not name "
                         f"supplied wheel {wheel.name!r}")
    import distillpdf

    installed = Path(distillpdf.__file__).resolve()
    roots = {Path(sysconfig.get_paths()[k]).resolve() for k in ("purelib", "platlib")}
    if not any(installed == root or root in installed.parents for root in roots):
        raise SystemExit(f"distillpdf resolves outside this interpreter's site-packages: {installed}")
    return {"path": str(wheel.resolve()), "filename": wheel.name,
            "sha256": expected_hash.removeprefix("sha256="), "version": version,
            "installed_module": str(installed), "direct_url": installed_url,
            "direct_url_archive_hash": recorded}


def extract_distillpdf(path: Path) -> int:
    import distillpdf

    return len(distillpdf.Pdf.open(str(path)).extract_tables())


def extract_pymupdf(path: Path) -> int:
    import pymupdf

    count = 0
    with pymupdf.open(path) as doc:
        for page in doc:
            count += len(page.find_tables().tables)
    return count


def extract_pdfplumber(path: Path) -> int:
    import pdfplumber

    with pdfplumber.open(path) as doc:
        return sum(len(page.extract_tables()) for page in doc.pages)


def extract_pymupdf4llm(path: Path) -> int:
    import pymupdf4llm

    # The comparator exposes a document Markdown operation, not a table-only primitive.
    # Keep it separately named so its timing is never presented as equivalent table-only work.
    text = pymupdf4llm.to_markdown(str(path), table_strategy="lines_strict")
    return text.count("\n|")


EXTRACTORS = {"distillpdf": extract_distillpdf, "pymupdf": extract_pymupdf,
              "pdfplumber": extract_pdfplumber, "pymupdf4llm": extract_pymupdf4llm}


def time_extractor(name: str, pdfs: list[Path], warmups: int, repeats: int) -> dict:
    fn = EXTRACTORS[name]
    samples, observations = [], []
    for iteration in range(warmups + repeats):
        started = time.perf_counter()
        count = sum(fn(path) for path in pdfs)
        elapsed = time.perf_counter() - started
        if iteration >= warmups:
            samples.append(elapsed)
            observations.append(count)
    if len(set(observations)) != 1:
        raise SystemExit(f"{name} emitted inconsistent observations: {observations}")
    return {"samples_seconds": samples, "median_seconds": statistics.median(samples),
            "min_seconds": min(samples), "max_seconds": max(samples),
            "relative_range": (max(samples) - min(samples)) / statistics.median(samples),
            "observation": observations[0]}


def git(args: list[str]) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, check=True, text=True,
                          stdout=subprocess.PIPE).stdout.strip()


def write_outputs(result: dict, label: str) -> tuple[Path, Path]:
    raw_dir = ROOT / "dev-docs/bench/out/table-analysis"
    summary_dir = ROOT / "dev-docs/bench/results"
    raw_dir.mkdir(parents=True, exist_ok=True)
    summary_dir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    raw = raw_dir / f"{stamp}-{label}.json"
    compact = summary_dir / f"table-analysis-{stamp}-{label}.json"
    raw.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    summary = {"schema": result["schema"], "timestamp_utc": result["timestamp_utc"],
               "git": result["git"], "wheel": result["wheel"],
               "provenance": result["provenance"],
               "timings": {k: {x: v[x] for x in ("median_seconds", "min_seconds",
                                                   "max_seconds", "relative_range", "observation")}
                           for k, v in result["timings"].items()},
               "raw": str(raw.relative_to(ROOT))}
    compact.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    md = compact.with_suffix(".md")
    lines = [f"# Table-analysis timing — {label}", "", f"- commit: `{result['git']['commit']}`",
             f"- wheel: `{result['wheel']['sha256']}`", f"- raw: `{summary['raw']}`", "",
             "| extractor | median s | min s | max s | range / median |", "|---|---:|---:|---:|---:|"]
    for name, row in summary["timings"].items():
        lines.append(f"| {name} | {row['median_seconds']:.6f} | {row['min_seconds']:.6f} | "
                     f"{row['max_seconds']:.6f} | {row['relative_range']:.2%} |")
    md.write_text("\n".join(lines) + "\n")
    return raw, compact


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wheel", type=Path, required=True)
    ap.add_argument("--extractors", default="distillpdf,pymupdf,pymupdf4llm")
    ap.add_argument("--repeats", type=int, default=LOCK["runner"]["repeats"])
    ap.add_argument("--warmups", type=int, default=LOCK["runner"]["warmups"])
    ap.add_argument("--limit", type=int, help="smoke-test prefix; not acceptance evidence")
    ap.add_argument("--label", default="phase1")
    args = ap.parse_args()
    extractors = args.extractors.split(",")
    unknown = set(extractors) - set(EXTRACTORS)
    if unknown:
        raise SystemExit(f"unknown extractors: {sorted(unknown)}")
    if not args.limit:
        expected = LOCK["runner"]
        actual = {"system": platform.system(), "machine": platform.machine(),
                  "pythonhashseed": os.environ.get("PYTHONHASHSEED"),
                  "threads": int(os.environ.get("RAYON_NUM_THREADS", "1"))}
        for key in ("system", "machine", "pythonhashseed", "threads"):
            if actual[key] != expected[key]:
                raise SystemExit(f"runner {key}={actual[key]!r}; pinned {expected[key]!r} required")
        if (args.repeats, args.warmups) != (expected["repeats"], expected["warmups"]):
            raise SystemExit("full evidence requires the pinned repeats and warmups")

    corpus = locate_corpus()
    corpus_proof = verify_manifest(corpus)
    pdfs = [corpus / "pdfs" / rec["file"] for rec in MANIFEST["pdfs"]]
    if args.limit:
        pdfs = pdfs[:args.limit]
    scorer = ROOT / "dev-docs/bench/scripts/table_content_metric.py"
    if not scorer.is_file():
        raise SystemExit(f"external scorer missing: {scorer}")
    reference = ROOT / "dev-docs/bench/out/external_calibration/pymupdf4llm"
    commit = git(["rev-parse", "HEAD"])
    dirty = bool(git(["status", "--porcelain", "--untracked-files=no"]))
    if not args.limit and dirty:
        raise SystemExit("full evidence requires a clean tracked worktree")
    result = {
        "schema": 1,
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "command": [sys.executable, *sys.argv],
        "git": {"commit": commit, "dirty": dirty},
        "runner": {"python": sys.version, "executable": sys.executable,
                   "system": platform.system(), "release": platform.release(),
                   "machine": platform.machine(), "processor": platform.processor(),
                   "pythonhashseed": os.environ.get("PYTHONHASHSEED"),
                   "rayon_num_threads": os.environ.get("RAYON_NUM_THREADS", "1")},
        "wheel": verify_wheel(args.wheel),
        "packages": verify_packages(extractors, full=not args.limit),
        "installed_distributions": sorted(
            f"{d.metadata['Name']}=={d.version}" for d in metadata.distributions()
            if d.metadata.get("Name")
        ),
        "provenance": {**corpus_proof, "lock_sha256": sha256(HERE / "lock.json"),
                       "scorer_sha256": sha256(scorer),
                       "owned_scorer_sha256": sha256(ROOT / "tests/test_table_corpus.py"),
                       "owned_corpus_tree_sha256": tree_sha256(ROOT / "tests/table_corpus"),
                       "reference_tree_sha256": tree_sha256(reference, "*/parsed.md")
                       if reference.is_dir() else None,
                       "pdfs_timed": len(pdfs), "dataset_root": str(corpus)},
        "timings": {},
    }
    for name in extractors:
        result["timings"][name] = time_extractor(name, pdfs, args.warmups, args.repeats)
    raw, compact = write_outputs(result, args.label)
    print(f"raw: {raw.relative_to(ROOT)}")
    print(f"summary: {compact.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
