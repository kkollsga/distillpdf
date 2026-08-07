import json
import subprocess
import sys
from pathlib import Path

import pytest

import distillpdf
from lazy_engine_fixtures import (
    IMAGE_VARIANTS,
    SCALE_AXES,
    generate_image,
    generate_scale,
    generate_small,
    verify,
)


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


@pytest.mark.parametrize("axis", SCALE_AXES)
def test_scale_profiles_are_axis_isolated_deterministic_and_openable(tmp_path, axis):
    left = tmp_path / "left"
    right = tmp_path / "right"
    first = generate_scale(left, axis, 17)
    second = generate_scale(right, axis, 17)

    assert first == second
    assert verify(left) == first
    row = first["fixtures"][0]
    assert row["facts"] == {
        "axis": axis,
        "unique_count": 17,
        "pages": 17 if axis == "pages" else 1,
        "generated_on_demand": True,
        "output_retained_by_generator": False,
    }
    assert (left / row["name"]).read_bytes() == (right / row["name"]).read_bytes()
    pdf = distillpdf.Pdf.open(str(left / row["name"]))
    assert pdf.page_count() == row["facts"]["pages"]


def test_cli_scale_profile(tmp_path):
    script = Path(__file__).with_name("lazy_engine_fixtures.py")
    result = subprocess.run(
        [
            sys.executable,
            str(script),
            "generate",
            "--profile",
            "scale",
            "--axis",
            "pages",
            "--count",
            "23",
            "--out",
            str(tmp_path),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(result.stdout) == {"fixtures": 1, "profile": "scale"}
    manifest = verify(tmp_path)
    assert manifest["fixtures"][0]["facts"]["unique_count"] == 23


@pytest.mark.parametrize("variant", IMAGE_VARIANTS)
def test_image_profiles_pin_shared_unique_mosaic_and_encrypted_objstm(tmp_path, variant):
    if variant == "encrypted-objstm":
        pytest.importorskip("pikepdf")
    left = tmp_path / "left"
    right = tmp_path / "right"
    first = generate_image(left, variant, 3, 32)
    second = generate_image(right, variant, 3, 32)

    assert first == second
    assert verify(left) == first
    row = first["fixtures"][0]
    assert row["facts"]["decoded_bytes_per_pair"] == 32 * 32 * 4
    assert row["facts"]["unique_image_pairs"] == (1 if variant in ("shared", "encrypted-objstm") else 3)
    pdf = distillpdf.Pdf.open(str(left / row["name"]))
    assert pdf.page_count() == row["facts"]["pages"]


def test_cli_image_profile(tmp_path):
    script = Path(__file__).with_name("lazy_engine_fixtures.py")
    result = subprocess.run(
        [
            sys.executable,
            str(script),
            "generate",
            "--profile",
            "image",
            "--variant",
            "mosaic",
            "--count",
            "4",
            "--dimension",
            "64",
            "--out",
            str(tmp_path),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(result.stdout) == {"fixtures": 1, "profile": "image"}
    assert verify(tmp_path)["fixtures"][0]["facts"]["decoded_bytes_total"] == 4 * 64 * 64 * 4
