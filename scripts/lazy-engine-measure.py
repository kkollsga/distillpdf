#!/usr/bin/env python3
"""Parent fresh-child wall/RSS harness for the lazy parallel engine plan."""
from __future__ import annotations

import argparse
import json
import os
import platform
import re
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path


RSS_MAC = re.compile(r"\s*(\d+)\s+maximum resident set size")
RSS_LINUX = re.compile(r"Maximum resident set size \(kbytes\):\s*(\d+)")
MAX_RESULT_BYTES = 16 * 1024


class MeasurementError(RuntimeError):
    pass


def timed_command(command: list[str], system: str | None = None) -> tuple[list[str], str]:
    system = system or platform.system()
    if system == "Darwin":
        return ["/usr/bin/time", "-l", *command], "darwin"
    if system == "Linux":
        return ["/usr/bin/time", "-v", *command], "linux"
    raise MeasurementError(f"unsupported RSS platform: {system}")


def parse_rss(stderr: str, flavor: str) -> int:
    match = (RSS_MAC if flavor == "darwin" else RSS_LINUX).search(stderr)
    if not match:
        raise MeasurementError("maximum RSS missing from fresh-child timing output")
    value = int(match.group(1))
    return value if flavor == "darwin" else value * 1024


def validate(row: dict, *, raw_rss: int, stdout_bytes: int, rss_limit: int | None = None) -> None:
    if row.get("schema") != 1 or row.get("status") != "ok":
        raise MeasurementError(f"child status/error violation: {row.get('error', row.get('status'))}")
    if not row.get("order_ok"):
        raise MeasurementError("ordered-output violation")
    digest = row.get("digest", "")
    if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
        raise MeasurementError("digest violation")
    if not row.get("accounting_ok"):
        raise MeasurementError("accounting violation")
    if not row.get("rss_contract_ok"):
        raise MeasurementError("RSS contract violation")
    if stdout_bytes > MAX_RESULT_BYTES or row.get("result_payload_bytes", MAX_RESULT_BYTES + 1) > MAX_RESULT_BYTES:
        raise MeasurementError("bounded-result violation")
    retained = row.get("retained_output_bytes")
    if row.get("sink") == "collected":
        if retained != row.get("output_bytes"):
            raise MeasurementError("collected-output accounting violation")
    elif retained != 0:
        raise MeasurementError("streaming sink retained output")
    if raw_rss <= 0:
        raise MeasurementError("raw RSS is not positive")
    if rss_limit is not None and raw_rss > rss_limit:
        raise MeasurementError(f"raw RSS limit exceeded: {raw_rss} > {rss_limit}")


def _kill_group(process: subprocess.Popen) -> None:
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=0.5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=2)


def run_once(
    *,
    probe: Path,
    path: Path,
    input_mode: str = "file",
    operation: str = "pages",
    sink: str = "null",
    inject: str = "none",
    timeout: float = 60.0,
    rss_limit: int | None = None,
    temporary_root: Path | None = None,
) -> dict:
    with tempfile.TemporaryDirectory(prefix="lazy-measure-", dir=temporary_root) as temporary:
        sink_path = Path(temporary) / "sink.bin"
        command = [
            sys.executable,
            str(probe),
            str(path),
            "--input", input_mode,
            "--operation", operation,
            "--sink", sink,
            "--inject", inject,
        ]
        if sink == "file":
            command.extend(("--sink-path", str(sink_path)))
        command, flavor = timed_command(command)
        started = time.perf_counter_ns()
        process = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            _kill_group(process)
            raise MeasurementError(f"fresh-child timeout after {timeout:.3f}s")
        process_wall = time.perf_counter_ns() - started
        lines = [line for line in stdout.splitlines() if line.strip()]
        if len(lines) != 1:
            raise MeasurementError(f"stdout protocol violation: expected one JSON line, got {len(lines)}")
        if len(stdout.encode()) > MAX_RESULT_BYTES:
            raise MeasurementError("bounded-result violation")
        try:
            row = json.loads(lines[0])
        except json.JSONDecodeError as error:
            raise MeasurementError(f"stdout JSON violation: {error}") from error
        raw_rss = parse_rss(stderr, flavor)
        if process.returncode != 0:
            raise MeasurementError(f"child exit/status violation: {process.returncode}; {row}")
        validate(row, raw_rss=raw_rss, stdout_bytes=len(stdout.encode()), rss_limit=rss_limit)
        if sink == "file":
            if not sink_path.exists() or sink_path.stat().st_size != row["output_bytes"]:
                raise MeasurementError("file sink size/accounting violation")
        return row | {
            "process_wall_nanos": process_wall,
            "raw_peak_rss_bytes": raw_rss,
            "exit_code": process.returncode,
        }


def summarize(rows: list[dict]) -> dict:
    digests = {row["digest"] for row in rows}
    if len(digests) != 1:
        raise MeasurementError(f"repeat digest mismatch: {sorted(digests)}")
    return {
        "runs": len(rows),
        "digest": rows[0]["digest"],
        "source_bytes": rows[0]["source_bytes"],
        "pages": rows[0]["pages"],
        "output_bytes": rows[0]["output_bytes"],
        "median_total_nanos": int(statistics.median(row["total_nanos"] for row in rows)),
        "median_process_wall_nanos": int(statistics.median(row["process_wall_nanos"] for row in rows)),
        "median_raw_peak_rss_bytes": int(statistics.median(row["raw_peak_rss_bytes"] for row in rows)),
        "maximum_raw_peak_rss_bytes": max(row["raw_peak_rss_bytes"] for row in rows),
        "rows": rows,
    }


def run_case(case: dict, *, probe: Path, repeats: int, timeout: float, temporary_root: Path | None) -> dict:
    rows = [
        run_once(
            probe=probe,
            path=Path(case["path"]),
            input_mode=case.get("input", "file"),
            operation=case.get("operation", "pages"),
            sink=case.get("sink", "null"),
            timeout=timeout,
            rss_limit=case.get("rss_limit"),
            temporary_root=temporary_root,
        )
        for _ in range(repeats)
    ]
    return {"name": case["name"], **{key: value for key, value in case.items() if key != "name"}, **summarize(rows)}


def _solve_three(matrix: list[list[float]], vector: list[float]) -> list[float]:
    for column in range(3):
        pivot = max(range(column, 3), key=lambda row: abs(matrix[row][column]))
        matrix[column], matrix[pivot] = matrix[pivot], matrix[column]
        vector[column], vector[pivot] = vector[pivot], vector[column]
        divisor = matrix[column][column]
        if abs(divisor) < 1e-12:
            raise MeasurementError("index calibration matrix is singular")
        for item in range(column, 3):
            matrix[column][item] /= divisor
        vector[column] /= divisor
        for row in range(3):
            if row == column:
                continue
            factor = matrix[row][column]
            for item in range(column, 3):
                matrix[row][item] -= factor * matrix[column][item]
            vector[row] -= factor * vector[column]
    return vector


def _round_up(value: float, quantum: int) -> int:
    return max(0, ((int(value) + quantum - 1) // quantum) * quantum)


def freeze_caps(raw: dict, significance: dict) -> dict:
    points = []
    for case in raw["cases"]:
        if "index_objects" in case:
            points.append((
                [1.0, float(case["index_objects"]), float(case["index_pages"])],
                float(case["median_raw_peak_rss_bytes"]),
            ))
    if len(points) < 6:
        raise MeasurementError("index calibration requires at least six object/page points")
    matrix = [[sum(x[i] * x[j] for x, _ in points) for j in range(3)] for i in range(3)]
    vector = [sum(x[i] * y for x, y in points) for i in range(3)]
    fixed, per_object, per_page = _solve_three(matrix, vector)
    residuals = [y - (fixed + per_object * x[1] + per_page * x[2]) for x, y in points]

    named_targets = {}
    for case in significance["cases"]:
        values = sorted(row["raw_peak_rss_bytes"] for row in case["rows"])
        median = int(statistics.median(values))
        observed_range = values[-1] - values[0]
        threshold = max(16 * 1024 * 1024, round(0.05 * median), 2 * observed_range)
        named_targets[case["name"]] = {
            "runs": len(values),
            "minimum_raw_peak_rss_bytes": values[0],
            "median_raw_peak_rss_bytes": median,
            "maximum_raw_peak_rss_bytes": values[-1],
            "observed_range_bytes": observed_range,
            "minimum_material_reduction_bytes": threshold,
        }

    normal_model = next(case for case in raw["cases"] if case["name"] == "normal-model-spool")
    return {
        "schema": 1,
        "raw_rss_unsubtracted": True,
        "normal_auto_eager_complete_wall_ratio_cap": 1.035,
        "timing_materiality_threshold": 0.03,
        "calibration_budget_bytes": 128 * 1024 * 1024,
        "capped_single_oversize_bytes": 64 * 1024 * 1024,
        "window": {"formula": "min(2 * workers, 16)", "multiplier": 2, "absolute_cap_pages": 16},
        "model_process_reservation_bytes": _round_up(normal_model["maximum_raw_peak_rss_bytes"], 8 * 1024 * 1024),
        "index_reservation_cap": {
            "formula": "fixed + objects * per_object + pages * per_page",
            "fixed_bytes": _round_up(fixed, 8 * 1024 * 1024),
            "bytes_per_unique_object": _round_up(per_object, 256),
            "bytes_per_page": _round_up(per_page, 512),
            "raw_fit": {"fixed_bytes": fixed, "bytes_per_object": per_object, "bytes_per_page": per_page},
            "maximum_absolute_fit_residual_bytes": round(max(abs(value) for value in residuals)),
            "interpretation": "conservative future compact-index reservation cap fitted to fresh-child eager-open raw RSS; no RSS component was subtracted",
        },
        "rss_admission_rule": {
            "runs_per_named_target": 7,
            "interval": "observed min/max (98.4375% distribution-free interval for a median with n=7)",
            "required": "candidate and eager min/max intervals do not overlap, and eager_median - candidate_median >= max(16 MiB, 5% of eager median, 2 * max(eager range, candidate range))",
            "named_targets": named_targets,
        },
        "sink_contract": {
            "streaming_retained_output_bytes": 0,
            "collected_measured_separately": True,
            "file_sink_closed_inside_child_timer": True,
            "model_spool_write_reread_close_inside_child_timer": True,
        },
        "provenance": {
            "fork_commit": "44fb08006a1a069cf87422dc73eb04dbeb433ba5",
            "window_basis": "admitted L0 direct-staging evidence reached 7-8 live pages at four workers; L1 freezes 2x workers with a 16-page ceiling",
            "oversize_basis": "one 4096x4096 RGBA image/SMask decoded pair is exactly 64 MiB",
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("path", nargs="?", type=Path)
    parser.add_argument("--spec", type=Path)
    parser.add_argument("--freeze-from", type=Path)
    parser.add_argument("--significance-from", type=Path)
    parser.add_argument("--probe", type=Path, default=Path(__file__).with_name("lazy_engine_probe.py"))
    parser.add_argument("--name", default="case")
    parser.add_argument("--input", choices=("file", "bytes"), default="file")
    parser.add_argument("--operation", choices=("open", "pages", "text", "html", "images", "ocr", "model"), default="pages")
    parser.add_argument("--sink", choices=("null", "file", "collected"), default="null")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=60.0)
    parser.add_argument("--temporary-root", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    if args.freeze_from:
        if not args.significance_from or args.path is not None or args.spec is not None:
            raise SystemExit("--freeze-from requires --significance-from and no path/--spec")
        result = freeze_caps(
            json.loads(args.freeze_from.read_text(encoding="utf-8")),
            json.loads(args.significance_from.read_text(encoding="utf-8")),
        )
        encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
        if args.out:
            args.out.parent.mkdir(parents=True, exist_ok=True)
            args.out.write_text(encoded, encoding="utf-8")
        print(encoded, end="")
        return
    if args.repeats < 3:
        raise SystemExit("measurement evidence requires at least three fresh children")
    if (args.path is None) == (args.spec is None):
        raise SystemExit("provide exactly one path or --spec")
    if args.spec:
        cases = json.loads(args.spec.read_text(encoding="utf-8"))["cases"]
    else:
        cases = [{
            "name": args.name,
            "path": str(args.path),
            "input": args.input,
            "operation": args.operation,
            "sink": args.sink,
        }]
    result = {
        "schema": 1,
        "raw_rss_unsubtracted": True,
        "probe": str(args.probe.resolve()),
        "repeats": args.repeats,
        "cases": [
            run_case(case, probe=args.probe, repeats=args.repeats, timeout=args.timeout, temporary_root=args.temporary_root)
            for case in cases
        ],
    }
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(encoded, encoding="utf-8")
    print(encoded, end="")


if __name__ == "__main__":
    main()
