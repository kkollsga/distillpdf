import json
import os
from pathlib import Path

from l2_eager_oracle import build_manifest
from lazy_engine_fixtures import generate_small


def test_python_l2_eager_targets_match_frozen_oracle(tmp_path):
    actual = json.dumps(build_manifest(tmp_path), indent=2, sort_keys=True) + "\n"
    snapshot = Path(__file__).with_name("oracles") / "l2-eager-python.json"
    if os.environ.get("DISTILLPDF_UPDATE_L2_ORACLES"):
        snapshot.write_text(actual, encoding="utf-8")
    expected = snapshot.read_text(encoding="utf-8")
    assert actual == expected, "Python L2 eager target drift"


def test_l2_targets_are_separate_and_independently_collected():
    oracle_dir = Path(__file__).with_name("oracles")
    rust = json.loads((oracle_dir / "l2-eager-rust.json").read_text(encoding="utf-8"))
    python = json.loads((oracle_dir / "l2-eager-python.json").read_text(encoding="utf-8"))
    assert rust["layer"] == "rust"
    assert python["layer"] == "python"
    assert rust["searchable_pdf.build_failure_contract"]["kind"] == "unreachable-after-open"
    assert "searchable_pdf.write_failure" in python
    assert rust != python


def test_committed_rust_malformed_fixtures_match_the_generator(tmp_path):
    generated = tmp_path / "small"
    generate_small(generated)
    committed = Path(__file__).with_name("fixtures_pdf") / "adversarial"
    for name in ("stream-missing-length.pdf", "stream-short-length.pdf"):
        assert (committed / name).read_bytes() == (generated / name).read_bytes()
