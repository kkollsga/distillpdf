#!/usr/bin/env python3
"""Fresh-child eager probe used by L1's measurement harness.

The process emits exactly one bounded JSON record.  Source open starts after the
internal timer and the timer ends only after the selected sink is closed.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import distillpdf


def _chunks(pdf, operation: str):
    if operation == "pages":
        for page in range(1, pdf.page_count() + 1):
            yield page, pdf.extract_page_text(page).encode() + b"\n"
        return
    if operation == "text":
        yield 1, pdf.extract_text().encode()
        return
    if operation == "html":
        yield 1, pdf.to_html(return_string=True, mode="page", image_mode="drop").encode()
        return
    if operation == "images":
        for image in pdf.extract_images():
            metadata = {
                key: value for key, value in image.items() if key != "data"
            }
            yield image["page"], json.dumps(metadata, sort_keys=True).encode() + b"\0" + image["data"]
        return
    if operation == "ocr":
        for entry in pdf.ocr_plan():
            image = entry.pop("image")
            metadata = json.dumps(entry, sort_keys=True).encode()
            yield entry["page"], metadata + b"\0" + (image or b"")
        return
    if operation == "model":
        with tempfile.TemporaryDirectory(prefix="lazy-model-probe-") as temporary:
            raw = Path(temporary) / "raw.dpdf"
            output = Path(temporary) / "model.dpdf"
            pdf.distill(str(raw), assets="none")
            model = json.loads(distillpdf.load_model(str(raw)))
            model["source"]["generated_at"] = "<normalized>"
            for space in model.get("embedding_spaces", []):
                space["generated_at"] = "<normalized>"
            distillpdf.save_dpdf(str(raw), str(output), json.dumps(model), {})
            with output.open("rb") as model:
                ordinal = 0
                while chunk := model.read(64 * 1024):
                    yield ordinal, chunk
                    ordinal += 1
        return
    if operation == "open":
        yield 0, str(pdf.page_count()).encode()
        return
    raise ValueError(f"unknown operation: {operation}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("path", type=Path)
    parser.add_argument("--input", choices=("file", "bytes"), default="file")
    parser.add_argument("--operation", choices=("open", "pages", "text", "html", "images", "ocr", "model"), default="pages")
    parser.add_argument("--sink", choices=("null", "file", "collected"), default="null")
    parser.add_argument("--sink-path", type=Path)
    parser.add_argument(
        "--inject",
        choices=("none", "order", "digest", "error", "accounting", "rss", "stdout", "sleep", "grandchild", "retain"),
        default="none",
    )
    args = parser.parse_args()

    if args.inject == "stdout":
        print("injected stray stdout", flush=True)
    if args.inject == "sleep":
        time.sleep(30)
    if args.inject == "grandchild":
        subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
        time.sleep(30)
    if args.inject == "error":
        print(json.dumps({"schema": 1, "status": "error", "error": "injected", "stage": "work"}))
        raise SystemExit(2)

    started = time.perf_counter_ns()
    source_bytes = args.path.stat().st_size
    if args.input == "bytes":
        pdf = distillpdf.Pdf.from_bytes(args.path.read_bytes())
    else:
        pdf = distillpdf.Pdf.open(str(args.path))
    opened = time.perf_counter_ns()

    digest = hashlib.sha256()
    output_bytes = 0
    order = []
    collected = []
    sink_file = None
    if args.sink == "file":
        if args.sink_path is None:
            raise ValueError("file sink requires --sink-path")
        sink_file = args.sink_path.open("wb")
    try:
        for ordinal, payload in _chunks(pdf, args.operation):
            order.append(ordinal)
            digest.update(payload)
            output_bytes += len(payload)
            if sink_file is not None:
                sink_file.write(payload)
            elif args.sink == "collected":
                collected.append(payload)
    finally:
        if sink_file is not None:
            sink_file.flush()
            os.fsync(sink_file.fileno())
            sink_file.close()
    finished = time.perf_counter_ns()

    order_ok = order == sorted(order)
    if args.inject == "order":
        order_ok = False
    checksum = digest.hexdigest()
    if args.inject == "digest":
        checksum = "not-a-sha256"
    accounting_ok = output_bytes == sum(len(payload) for payload in collected) if args.sink == "collected" else True
    if args.inject == "accounting":
        accounting_ok = False
    retained = sum(len(payload) for payload in collected)
    if args.inject == "retain" and args.sink != "collected":
        retained = 1

    row = {
        "schema": 1,
        "status": "ok",
        "input": args.input,
        "operation": args.operation,
        "sink": args.sink,
        "source_bytes": source_bytes,
        "pages": pdf.page_count(),
        "open_nanos": opened - started,
        "work_sink_nanos": finished - opened,
        "total_nanos": finished - started,
        "output_bytes": output_bytes,
        "digest": checksum,
        "order_ok": order_ok,
        "accounting_ok": accounting_ok,
        "retained_output_bytes": retained,
        "rss_contract_ok": args.inject != "rss",
    }
    encoded = json.dumps(row, separators=(",", ":"), sort_keys=True)
    row["result_payload_bytes"] = len(encoded.encode())
    print(json.dumps(row, separators=(",", ":"), sort_keys=True))


if __name__ == "__main__":
    main()
