import json
import struct
import subprocess
import sys
import zipfile
from pathlib import Path

import pytest

import distillpdf
from lazy_engine_zip_fixtures import CENTRAL, EOCD, LOCAL, generate, verify


EXPECTED = {
    "valid-v1.dpdf",
    "hostile-name-parent.dpdf",
    "hostile-name-absolute.dpdf",
    "hostile-name-windows.dpdf",
    "hostile-name-backslash.dpdf",
    "duplicate-member.dpdf",
    "overlapping-central-offset.dpdf",
    "central-crc-mismatch.dpdf",
    "central-size-mismatch.dpdf",
    "truncated-local-header.dpdf",
    "truncated-entry-data.dpdf",
    "truncated-central-directory.dpdf",
    "partial-destination.dpdf",
    "duplicate-logical-asset.dpdf",
    "zip64-small-valid.dpdf",
    "zip64-overflow-claim.dpdf",
}


def test_hostile_zip_profile_is_deterministic_and_self_verifying(tmp_path):
    left = tmp_path / "left"
    right = tmp_path / "right"
    first = generate(left)
    second = generate(right)

    assert first == second
    assert verify(left) == first
    assert {row["name"] for row in first["fixtures"]} == EXPECTED
    for name in EXPECTED:
        assert (left / name).read_bytes() == (right / name).read_bytes()


def test_manifest_pins_v2_dispositions_and_u64_boundary(tmp_path):
    manifest = generate(tmp_path)
    rows = {row["name"]: row for row in manifest["fixtures"]}

    assert rows["valid-v1.dpdf"]["facts"]["expected_v2"] == "accept"
    assert rows["zip64-small-valid.dpdf"]["facts"]["expected_v2"] == "accept"
    assert rows["zip64-overflow-claim.dpdf"]["facts"] == {
        "actual_size": rows["zip64-small-valid.dpdf"]["facts"]["declared_size"],
        "class_": "zip64_overflow_claim",
        "declared_size": (1 << 32) + 17,
        "expected_v2": "reject",
    }
    assert all(
        row["facts"]["expected_v2"] == "reject"
        for name, row in rows.items()
        if name not in {"valid-v1.dpdf", "zip64-small-valid.dpdf"}
    )


def test_valid_archives_have_independently_readable_model(tmp_path):
    generate(tmp_path)
    for name in ("valid-v1.dpdf", "zip64-small-valid.dpdf"):
        with zipfile.ZipFile(tmp_path / name) as archive:
            model = json.loads(archive.read("model.json"))
        assert model["schema_version"] == 0

    # Freeze the current v1 eager loader only for its native subset; ZIP64 support is L7.
    assert json.loads(distillpdf.load_model(str(tmp_path / "valid-v1.dpdf")))["schema_version"] == 0


@pytest.mark.parametrize(
    ("name", "member"),
    [
        ("hostile-name-parent.dpdf", "../escape"),
        ("hostile-name-absolute.dpdf", "/absolute"),
        ("hostile-name-windows.dpdf", "C:\\escape"),
        ("hostile-name-backslash.dpdf", "..\\escape"),
    ],
)
def test_hostile_member_names_are_exact_archive_names(tmp_path, name, member):
    generate(tmp_path)
    with zipfile.ZipFile(tmp_path / name) as archive:
        assert member in archive.namelist()


def test_duplicate_and_overlap_shapes_are_structural(tmp_path):
    generate(tmp_path)
    duplicate = (tmp_path / "duplicate-member.dpdf").read_bytes()
    assert duplicate.count(b"img/dup.bin") == 4  # local + central, twice

    overlap = (tmp_path / "overlapping-central-offset.dpdf").read_bytes()
    positions = [i for i in range(len(overlap)) if overlap.startswith(struct.pack("<I", CENTRAL), i)]
    assert len(positions) == 2
    assert struct.unpack_from("<I", overlap, positions[1] + 42)[0] == 0


def test_truncation_boundaries_are_not_complete_archives(tmp_path):
    manifest = generate(tmp_path)
    rows = {row["name"]: row for row in manifest["fixtures"]}
    for name in (
        "truncated-local-header.dpdf",
        "truncated-entry-data.dpdf",
        "truncated-central-directory.dpdf",
        "partial-destination.dpdf",
    ):
        data = (tmp_path / name).read_bytes()
        assert rows[name]["facts"]["expected_v2"] == "reject"
        assert not data.endswith(struct.pack("<I", EOCD) + data[-18:])


def test_cli_generate_and_verify(tmp_path):
    script = Path(__file__).with_name("lazy_engine_zip_fixtures.py")
    generated = subprocess.run(
        [sys.executable, str(script), "generate", "--out", str(tmp_path)],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(generated.stdout) == {"fixtures": 16, "profile": "hostile-zip"}
    verified = subprocess.run(
        [sys.executable, str(script), "verify", "--out", str(tmp_path)],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(verified.stdout) == {"fixtures": 16, "profile": "hostile-zip"}


def test_verifier_rejects_mutation(tmp_path):
    generate(tmp_path)
    target = tmp_path / "valid-v1.dpdf"
    target.write_bytes(target.read_bytes() + b"corrupt")
    with pytest.raises(ValueError, match="size mismatch: valid-v1.dpdf"):
        verify(tmp_path)
