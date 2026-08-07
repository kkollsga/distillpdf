#!/usr/bin/env python3
"""Independent hostile ZIP/ZIP64 fixtures for the lazy-engine measurement plan.

The product's v1 ``.dpdf`` reader intentionally is not used here.  These archives
pin the inputs that the bounded v2 reader must later classify in L7: hostile names,
duplicate/overlapping members, inconsistent metadata, truncation, logical model
collisions, partial writes, and ZIP64-sized fields.
"""
from __future__ import annotations

import argparse
import binascii
import hashlib
import json
import struct
from dataclasses import dataclass
from pathlib import Path


LOCAL = 0x04034B50
CENTRAL = 0x02014B50
EOCD = 0x06054B50
ZIP64_EOCD = 0x06064B50
ZIP64_LOCATOR = 0x07064B50


def _model(*, assets: list[dict] | None = None) -> bytes:
    value = {
        "assets": assets or [],
        "blocks": [],
        "indexes": {
            "coverage": {"sectioned": 0.0},
            "kinds": {},
            "pages": {},
            "sections": {},
        },
        "metadata": {},
        "pages": [],
        "schema_version": 0,
        "sections": [],
        "source": {
            "distillpdf": "l1-fixture",
            "file": "fixture.pdf",
            "generated_at": "2000-01-01T00:00:00Z",
            "pages": 0,
            "sha256": "0" * 64,
        },
    }
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


@dataclass(frozen=True)
class Entry:
    name: str
    data: bytes


@dataclass(frozen=True)
class Archive:
    data: bytes
    local_offsets: tuple[int, ...]
    central_offsets: tuple[int, ...]
    eocd_offset: int


def _zip(entries: list[Entry]) -> Archive:
    """Build a deterministic STORE archive without zipfile or product code."""
    output = bytearray()
    local_offsets: list[int] = []
    records: list[tuple[Entry, int, int]] = []
    for entry in entries:
        name = entry.name.encode("utf-8")
        crc = binascii.crc32(entry.data) & 0xFFFF_FFFF
        offset = len(output)
        local_offsets.append(offset)
        output += struct.pack(
            "<IHHHHHIIIHH", LOCAL, 20, 0, 0, 0, 0, crc,
            len(entry.data), len(entry.data), len(name), 0,
        )
        output += name + entry.data
        records.append((entry, offset, crc))

    central_offsets: list[int] = []
    cd_start = len(output)
    for entry, local_offset, crc in records:
        name = entry.name.encode("utf-8")
        central_offsets.append(len(output))
        output += struct.pack(
            "<IHHHHHHIIIHHHHHII", CENTRAL, 20, 20, 0, 0, 0, 0,
            crc, len(entry.data), len(entry.data), len(name), 0, 0, 0, 0,
            0, local_offset,
        )
        output += name
    cd_size = len(output) - cd_start
    eocd_offset = len(output)
    output += struct.pack(
        "<IHHHHIIH", EOCD, 0, 0, len(entries), len(entries), cd_size,
        cd_start, 0,
    )
    return Archive(bytes(output), tuple(local_offsets), tuple(central_offsets), eocd_offset)


def _zip64(entry: Entry, *, declared_size: int | None = None) -> bytes:
    """Build a one-member ZIP64 archive, optionally with a hostile size claim."""
    name = entry.name.encode("utf-8")
    actual = len(entry.data)
    claimed = actual if declared_size is None else declared_size
    crc = binascii.crc32(entry.data) & 0xFFFF_FFFF
    local_extra = struct.pack("<HHQQ", 1, 16, claimed, claimed)
    output = bytearray(
        struct.pack(
            "<IHHHHHIIIHH", LOCAL, 45, 0, 0, 0, 0, crc,
            0xFFFF_FFFF, 0xFFFF_FFFF, len(name), len(local_extra),
        )
    )
    output += name + local_extra + entry.data
    cd_start = len(output)
    central_extra = struct.pack("<HHQQQ", 1, 24, claimed, claimed, 0)
    output += struct.pack(
        "<IHHHHHHIIIHHHHHII", CENTRAL, 45, 45, 0, 0, 0, 0, crc,
        0xFFFF_FFFF, 0xFFFF_FFFF, len(name), len(central_extra), 0, 0, 0,
        0, 0xFFFF_FFFF,
    )
    output += name + central_extra
    cd_size = len(output) - cd_start
    zip64_eocd = len(output)
    output += struct.pack(
        "<IQHHIIQQQQ", ZIP64_EOCD, 44, 45, 45, 0, 0, 1, 1,
        cd_size, cd_start,
    )
    output += struct.pack("<IIQI", ZIP64_LOCATOR, 0, zip64_eocd, 1)
    output += struct.pack(
        "<IHHHHIIH", EOCD, 0, 0, 0xFFFF, 0xFFFF, 0xFFFF_FFFF,
        0xFFFF_FFFF, 0,
    )
    return bytes(output)


def _put_u32(data: bytes, offset: int, value: int) -> bytes:
    changed = bytearray(data)
    struct.pack_into("<I", changed, offset, value)
    return bytes(changed)


def _row(name: str, data: bytes, **facts: object) -> tuple[str, bytes, dict]:
    return name, data, {
        "name": name,
        "size": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
        "facts": facts,
    }


def _fixtures() -> list[tuple[str, bytes, dict]]:
    model = _model()
    valid = _zip([Entry("model.json", model), Entry("img/a.bin", b"first")])
    rows = [_row("valid-v1.dpdf", valid.data, class_="valid", expected_v2="accept")]

    for label, hostile_name in (
        ("parent", "../escape"),
        ("absolute", "/absolute"),
        ("windows", "C:\\escape"),
        ("backslash", "..\\escape"),
    ):
        archive = _zip([Entry("model.json", model), Entry(hostile_name, b"x")])
        rows.append(_row(
            f"hostile-name-{label}.dpdf", archive.data, class_="hostile_name",
            member=hostile_name, expected_v2="reject",
        ))

    duplicate = _zip([
        Entry("model.json", model), Entry("img/dup.bin", b"first"),
        Entry("img/dup.bin", b"second"),
    ])
    rows.append(_row(
        "duplicate-member.dpdf", duplicate.data, class_="duplicate_member",
        member="img/dup.bin", occurrences=2, expected_v2="reject",
    ))

    overlap = _zip([Entry("model.json", model), Entry("img/a.bin", b"asset")])
    # Point the second central record at the first member's local header.
    overlap_data = _put_u32(overlap.data, overlap.central_offsets[1] + 42, 0)
    rows.append(_row(
        "overlapping-central-offset.dpdf", overlap_data, class_="overlap",
        expected_v2="reject",
    ))

    crc_mismatch = _put_u32(valid.data, valid.central_offsets[1] + 16, 0xDEAD_BEEF)
    rows.append(_row(
        "central-crc-mismatch.dpdf", crc_mismatch, class_="metadata_disagreement",
        field="crc32", expected_v2="reject",
    ))
    size_mismatch = _put_u32(valid.data, valid.central_offsets[1] + 20, 3)
    rows.append(_row(
        "central-size-mismatch.dpdf", size_mismatch, class_="metadata_disagreement",
        field="compressed_size", expected_v2="reject",
    ))

    rows.extend([
        _row(
            "truncated-local-header.dpdf", valid.data[:18], class_="truncated",
            boundary="local_header", expected_v2="reject",
        ),
        _row(
            "truncated-entry-data.dpdf", valid.data[:30 + len("model.json") + len(model) - 3],
            class_="truncated", boundary="entry_data", expected_v2="reject",
        ),
        _row(
            "truncated-central-directory.dpdf", valid.data[:valid.central_offsets[0] + 20],
            class_="truncated", boundary="central_directory", expected_v2="reject",
        ),
        _row(
            "partial-destination.dpdf", valid.data[:len(valid.data) // 2],
            class_="partial_write", expected_v2="reject",
        ),
    ])

    asset = {
        "bytes": 5,
        "id": "img/dup.bin",
        "kind": "figure",
        "sha256": hashlib.sha256(b"asset").hexdigest(),
        "storage": "embedded",
    }
    logical = _zip([
        Entry("model.json", _model(assets=[asset, dict(asset)])),
        Entry("img/dup.bin", b"asset"),
    ])
    rows.append(_row(
        "duplicate-logical-asset.dpdf", logical.data, class_="logical_collision",
        asset_id="img/dup.bin", occurrences=2, expected_v2="reject",
    ))

    zip64 = _zip64(Entry("model.json", model))
    rows.append(_row(
        "zip64-small-valid.dpdf", zip64, class_="zip64", declared_size=len(model),
        expected_v2="accept",
    ))
    claimed = (1 << 32) + 17
    zip64_overflow = _zip64(Entry("model.json", model), declared_size=claimed)
    rows.append(_row(
        "zip64-overflow-claim.dpdf", zip64_overflow, class_="zip64_overflow_claim",
        declared_size=claimed, actual_size=len(model), expected_v2="reject",
    ))
    return rows


def generate(output: Path) -> dict:
    output.mkdir(parents=True, exist_ok=True)
    fixtures = []
    for name, data, row in _fixtures():
        (output / name).write_bytes(data)
        fixtures.append(row)
    manifest = {"format": 1, "profile": "hostile-zip", "fixtures": fixtures}
    (output / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return manifest


def verify(output: Path) -> dict:
    manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
    if manifest.get("profile") != "hostile-zip":
        raise ValueError("not a hostile-zip fixture manifest")
    for row in manifest["fixtures"]:
        data = (output / row["name"]).read_bytes()
        if len(data) != row["size"]:
            raise ValueError(f"size mismatch: {row['name']}")
        if hashlib.sha256(data).hexdigest() != row["sha256"]:
            raise ValueError(f"digest mismatch: {row['name']}")
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("generate", "verify"))
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    manifest = generate(args.out) if args.action == "generate" else verify(args.out)
    print(json.dumps({"fixtures": len(manifest["fixtures"]), "profile": manifest["profile"]}))


if __name__ == "__main__":
    main()
