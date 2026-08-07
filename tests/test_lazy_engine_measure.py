import importlib.util
import subprocess
import sys
import time
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
PROBE = ROOT / "scripts" / "lazy_engine_probe.py"
MEASURE = ROOT / "scripts" / "lazy-engine-measure.py"
FIXTURE = Path(__file__).with_name("fixtures_pdf") / "sec_structure.pdf"


def _load_measure():
    spec = importlib.util.spec_from_file_location("lazy_engine_measure", MEASURE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_rss_parsers_normalize_to_bytes():
    measure = _load_measure()
    assert measure.parse_rss("  12345  maximum resident set size", "darwin") == 12345
    assert measure.parse_rss("Maximum resident set size (kbytes): 12345", "linux") == 12345 * 1024


@pytest.mark.parametrize(
    ("inject", "message"),
    [
        ("order", "ordered-output"),
        ("digest", "digest"),
        ("error", "exit/status"),
        ("accounting", "accounting"),
        ("rss", "RSS contract"),
        ("stdout", "stdout protocol"),
        ("retain", "retained output"),
    ],
)
def test_parent_catches_injected_contract_violations(tmp_path, inject, message):
    measure = _load_measure()
    with pytest.raises(measure.MeasurementError, match=message):
        measure.run_once(
            probe=PROBE,
            path=FIXTURE,
            inject=inject,
            timeout=5,
            temporary_root=tmp_path,
        )


@pytest.mark.parametrize("inject", ["sleep", "grandchild"])
def test_watchdog_terminates_timed_out_process_group(tmp_path, inject):
    measure = _load_measure()
    started = time.monotonic()
    with pytest.raises(measure.MeasurementError, match="timeout"):
        measure.run_once(
            probe=PROBE,
            path=FIXTURE,
            inject=inject,
            timeout=0.2,
            temporary_root=tmp_path,
        )
    assert time.monotonic() - started < 3


@pytest.mark.parametrize("sink", ["null", "file", "collected"])
@pytest.mark.parametrize("input_mode", ["file", "bytes"])
def test_fresh_child_validates_all_sink_and_input_classes(tmp_path, sink, input_mode):
    measure = _load_measure()
    row = measure.run_once(
        probe=PROBE,
        path=FIXTURE,
        input_mode=input_mode,
        operation="pages",
        sink=sink,
        timeout=10,
        temporary_root=tmp_path,
    )
    assert row["raw_peak_rss_bytes"] > 0
    assert row["process_wall_nanos"] >= row["total_nanos"]
    assert row["retained_output_bytes"] == (row["output_bytes"] if sink == "collected" else 0)


def test_cli_rejects_fewer_than_three_fresh_children():
    result = subprocess.run(
        [sys.executable, str(MEASURE), str(FIXTURE), "--repeats", "2"],
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0
    assert "at least three" in result.stderr


def test_cap_freeze_fits_six_points_and_rounds_conservatively():
    measure = _load_measure()
    raw = {"cases": []}
    for objects, pages in ((1000, 1), (5000, 1), (10000, 1), (2000, 1000), (10000, 5000), (20000, 10000)):
        rss = 30_000_000 + 1300 * objects + 2500 * pages
        raw["cases"].append({
            "name": f"fit-{objects}-{pages}",
            "index_objects": objects,
            "index_pages": pages,
            "median_raw_peak_rss_bytes": rss,
        })
    raw["cases"].append({"name": "normal-model-spool", "maximum_raw_peak_rss_bytes": 33_000_000})
    significance = {"cases": [{
        "name": "normal",
        "rows": [{"raw_peak_rss_bytes": 30_000_000 + delta} for delta in (0, 10, 20, 30, 40, 50, 60)],
    }]}
    frozen = measure.freeze_caps(raw, significance)
    cap = frozen["index_reservation_cap"]
    assert cap["fixed_bytes"] >= 30_000_000
    assert cap["bytes_per_unique_object"] >= 1300
    assert cap["bytes_per_page"] >= 2500
    assert frozen["model_process_reservation_bytes"] == 4 * 8 * 1024 * 1024
    assert frozen["rss_admission_rule"]["named_targets"]["normal"]["minimum_material_reduction_bytes"] == 16 * 1024 * 1024
