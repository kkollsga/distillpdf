#!/usr/bin/env python3
"""One extraction in one process, so its peak footprint belongs to that extraction alone.

The gate's sweep starts this once per (document, engine, operation, repeat). A shared process
would report the largest operation's peak against every operation after it, and a panic or an
out-of-memory kill would take the rest of the document's results with it. Here a crash is a
non-zero exit the parent records against exactly one operation.

Writes the operation's output to ``--out`` and prints one JSON line of measurements. Measured:
open + operation, with the peak read before the output is hashed or written, so the gate's own
bookkeeping is not billed to the library.
"""
from __future__ import annotations

import argparse
import base64
import ctypes
import hashlib
import json
import os
import re
import sys
import time

import distillpdf




def _rusage() -> dict:
    """phys_footprint figures and retired instructions for this process (macOS), else {}.

    Footprint is what the owner's memory rule measures. Retired instructions are the timing
    signal that survives a busy machine: other agents' builds move wall clock by multiples and
    leave this count nearly unchanged."""
    if sys.platform != "darwin":
        return {}

    class RusageInfoV4(ctypes.Structure):
        # sys/resource.h, struct rusage_info_v4: only the names used below matter, the rest
        # hold the offsets.
        _fields_ = [("ri_uuid", ctypes.c_uint8 * 16)] + [
            (name, ctypes.c_uint64)
            for name in (
                "user", "system", "pkg_idle", "interrupt", "pageins", "wired", "resident",
                "phys_footprint", "start", "exit", "c_user", "c_system", "c_idle", "c_int",
                "c_pageins", "c_elapsed", "disk_r", "disk_w", "qos_default", "qos_maint",
                "qos_bg", "qos_util", "qos_legacy", "qos_ui", "qos_interactive", "billed_sys",
                "serviced_sys", "logical_writes", "lifetime_max_phys_footprint", "instructions",
                "cycles", "billed_energy", "serviced_energy", "interval_max_phys_footprint",
                "runnable",
            )
        ]

    info = RusageInfoV4()
    libc = ctypes.CDLL("libc.dylib", use_errno=True)
    if libc.proc_pid_rusage(os.getpid(), 4, ctypes.byref(info)) != 0:
        return {}
    return {
        "peak_footprint": info.lifetime_max_phys_footprint,
        "footprint": info.phys_footprint,
        "instructions": info.instructions,
    }


_DATA_URI = re.compile(r'data:([a-zA-Z0-9.+/-]+);base64,([A-Za-z0-9+/=]+)')


def readable_html(html: str) -> str:
    """HTML with each embedded image replaced by its hash: diffable and a fraction of the size,
    and a changed image still shows as a changed line. The gate hashes the untouched HTML."""
    def repl(m: re.Match) -> str:
        payload = m.group(2)
        sha = hashlib.sha256(payload.encode("ascii")).hexdigest()[:16]
        return f"data:{m.group(1)};sha256={sha};b64len={len(payload)}"
    return _DATA_URI.sub(repl, html)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--pdf", required=True)
    ap.add_argument("--op", required=True, choices=["text", "pages", "markdown", "html", "links"])
    ap.add_argument("--engine", default="eager", choices=["eager", "lazy"])
    ap.add_argument("--out", default="", help="write the output here (omit on timing repeats)")
    a = ap.parse_args()

    before = _rusage()
    wall = time.perf_counter()
    cpu = time.process_time()
    rec: dict = {"op": a.op, "engine": a.engine}
    page_hashes = None
    try:
        doc = distillpdf.Pdf.open(a.pdf, engine=a.engine)
        rec["route"] = doc.engine
        rec["page_count"] = doc.page_count()
        if a.op == "text":
            output = doc.extract_text()
        elif a.op == "pages":
            pages = [doc.extract_page_text(p) for p in range(1, rec["page_count"] + 1)]
            output = None
        elif a.op == "markdown":
            output = doc.to_markdown(return_string=True, image_mode="drop")
        elif a.op == "html":
            output = doc.to_html(return_string=True, image_mode="embed")
        else:
            output = json.dumps(doc.extract_links(), sort_keys=True, ensure_ascii=False, indent=0)
    except Exception as e:  # an extraction error is a result the gate compares, not a crash
        rec["wall_s"] = time.perf_counter() - wall
        rec["cpu_s"] = time.process_time() - cpu
        rec["error"] = f"{type(e).__name__}: {e}"[:300]
        print(json.dumps(rec))
        return 0

    rec["wall_s"] = round(time.perf_counter() - wall, 4)
    rec["cpu_s"] = round(time.process_time() - cpu, 4)
    after = _rusage()
    if after:
        rec["peak_footprint"] = after["peak_footprint"]
        rec["import_footprint"] = before["footprint"]
        rec["instructions"] = after["instructions"] - before["instructions"]

    if a.op == "pages":
        page_hashes = [hashlib.sha256(p.encode("utf-8", "surrogatepass")).hexdigest()[:12]
                       for p in pages]
        rec["page_hashes"] = page_hashes
        rec["sha"] = hashlib.sha256("\n".join(page_hashes).encode()).hexdigest()[:24]
        rec["chars"] = sum(len(p) for p in pages)
        rec["empty_pages"] = [i for i, p in enumerate(pages, 1) if not p.strip()]
        if a.out:
            with open(a.out, "w", encoding="utf-8", errors="surrogatepass") as f:
                json.dump(pages, f, ensure_ascii=False, indent=0)
    else:
        rec["sha"] = hashlib.sha256(output.encode("utf-8", "surrogatepass")).hexdigest()[:24]
        rec["chars"] = len(output)
        if a.out:
            body = readable_html(output) if a.op == "html" else output
            with open(a.out, "w", encoding="utf-8", errors="surrogatepass") as f:
                f.write(body)
    print(json.dumps(rec))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
