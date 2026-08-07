import json
import subprocess
import sys
from pathlib import Path

import pytest

import distillpdf
from lazy_engine_fixtures import generate_small, verify


EXPECTED_SMALL = {
    "classic.pdf",
    "xref-stream.pdf",
    "incremental.pdf",
    "object-stream.pdf",
    "reference-one-hop.pdf",
    "reference-at-limit.pdf",
    "reference-over-limit.pdf",
    "reference-dangling.pdf",
    "reference-cycle.pdf",
    "generation-match.pdf",
    "generation-mismatch.pdf",
    "stream-missing-length.pdf",
    "stream-short-length.pdf",
}


def test_small_profile_is_deterministic_and_self_verifying(tmp_path):
    left = tmp_path / "left"
    right = tmp_path / "right"
    first = generate_small(left)
    second = generate_small(right)

    assert first == second
    assert verify(left) == first
    assert verify(right) == second
    assert {row["name"] for row in first["fixtures"]} == EXPECTED_SMALL
    for name in EXPECTED_SMALL:
        assert (left / name).read_bytes() == (right / name).read_bytes()


def test_manifest_pins_reference_and_xref_contracts(tmp_path):
    manifest = generate_small(tmp_path)
    rows = {row["name"]: row for row in manifest["fixtures"]}

    assert rows["classic.pdf"]["facts"]["xref"] == "classic"
    assert rows["xref-stream.pdf"]["facts"]["xref"] == "stream"
    assert rows["incremental.pdf"]["facts"]["revisions"] == 2
    assert rows["object-stream.pdf"]["facts"]["compressed_objects"] == [7, 8]
    assert rows["reference-at-limit.pdf"]["facts"]["reference_hops"] == 128
    assert rows["reference-over-limit.pdf"]["facts"]["reference_hops"] == 129
    assert rows["generation-mismatch.pdf"]["facts"]["target"] == [6, 1]
    assert rows["generation-mismatch.pdf"]["facts"]["actual"] == [6, 0]


@pytest.mark.parametrize(
    "name",
    [
        "classic.pdf",
        "xref-stream.pdf",
        "incremental.pdf",
        "object-stream.pdf",
        "generation-match.pdf",
    ],
)
def test_valid_small_documents_open_through_python_surface(tmp_path, name):
    generate_small(tmp_path)
    pdf = distillpdf.Pdf.open(str(tmp_path / name))
    assert pdf.page_count() == 1


def test_cli_generate_and_verify(tmp_path):
    script = Path(__file__).with_name("lazy_engine_fixtures.py")
    generated = subprocess.run(
        [sys.executable, str(script), "generate", "--out", str(tmp_path)],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(generated.stdout) == {"fixtures": 13, "profile": "small"}
    verified = subprocess.run(
        [sys.executable, str(script), "verify", "--out", str(tmp_path)],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(verified.stdout) == {"fixtures": 13, "profile": "small"}


def test_verifier_rejects_mutation(tmp_path):
    generate_small(tmp_path)
    target = tmp_path / "classic.pdf"
    target.write_bytes(target.read_bytes() + b"corrupt")
    with pytest.raises(ValueError, match="size mismatch: classic.pdf"):
        verify(tmp_path)
