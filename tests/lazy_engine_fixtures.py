#!/usr/bin/env python3
"""Deterministic, on-demand fixtures for the lazy parallel engine plan.

Unlike the small committed fidelity corpus, stress fixtures produced here live in a
caller-selected directory.  This keeps 5k/10k and memory-budget profiles out of the
broad ``owned_pdfs()`` sweeps while preserving a reproducible manifest.

The writer is intentionally small and independent of distillpdf/lopdf.  It can emit
classic xrefs, xref streams (including compressed-object entries), incremental
updates, non-zero generations, and malformed stream dictionaries without first
materialising a third-party PDF.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Iterator


HEADER = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n"
FONT = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"


def _stream(payload: bytes, dictionary: bytes = b"") -> bytes:
    extra = b" " + dictionary.strip() if dictionary.strip() else b""
    return b"<< /Length %d%s >>\nstream\n%s\nendstream" % (
        len(payload), extra, payload
    )


def _text_stream(text: str) -> bytes:
    escaped = text.encode("ascii").replace(b"\\", b"\\\\").replace(b"(", b"\\(").replace(b")", b"\\)")
    return _stream(b"BT /F1 12 Tf 72 720 Td (" + escaped + b") Tj ET")


@dataclass(frozen=True)
class RenderedPdf:
    data: bytes
    startxref: int


class PdfWriter:
    """Minimal deterministic indirect-object writer with checked xref widths."""

    def __init__(self, root: tuple[int, int] = (1, 0)) -> None:
        self.root = root
        self._objects: dict[int, tuple[int, bytes]] = {}

    def add(self, number: int, body: bytes, generation: int = 0) -> None:
        if number <= 0 or number in self._objects:
            raise ValueError(f"invalid or duplicate object number: {number}")
        if not 0 <= generation <= 65_534:
            raise ValueError(f"invalid generation: {generation}")
        self._objects[number] = (generation, body)

    def _ordinary_body(self) -> tuple[bytearray, dict[int, int]]:
        body = bytearray(HEADER)
        offsets: dict[int, int] = {}
        for number, (generation, value) in sorted(self._objects.items()):
            offsets[number] = len(body)
            body += f"{number} {generation} obj\n".encode("ascii")
            body += value + b"\nendobj\n"
        return body, offsets

    def render_classic(self, trailer: bytes = b"") -> RenderedPdf:
        body, offsets = self._ordinary_body()
        startxref = len(body)
        size = max(self._objects, default=0) + 1
        body += f"xref\n0 {size}\n".encode("ascii")
        body += b"0000000000 65535 f \n"
        for number in range(1, size):
            entry = self._objects.get(number)
            if entry is None:
                body += b"0000000000 00000 f \n"
            else:
                generation, _ = entry
                offset = offsets[number]
                if offset > 9_999_999_999:
                    raise OverflowError("classic xref offset exceeds ten digits")
                body += f"{offset:010d} {generation:05d} n \n".encode("ascii")
        root_number, root_generation = self.root
        suffix = b" " + trailer.strip() if trailer.strip() else b""
        body += (
            b"trailer\n<< /Size %d /Root %d %d R%s >>\nstartxref\n%d\n%%%%EOF\n"
            % (size, root_number, root_generation, suffix, startxref)
        )
        return RenderedPdf(bytes(body), startxref)
    def render_xref_stream(
        self,
        compressed: dict[int, tuple[int, int]] | None = None,
        xref_number: int | None = None,
    ) -> RenderedPdf:
        compressed = compressed or {}
        overlap = set(compressed).intersection(self._objects)
        if overlap:
            raise ValueError(f"compressed objects also emitted normally: {sorted(overlap)}")
        body, offsets = self._ordinary_body()
        largest = max([*self._objects, *compressed], default=0)
        xref_number = xref_number or largest + 1
        if xref_number in self._objects or xref_number in compressed:
            raise ValueError("xref stream object collides with another object")
        startxref = len(body)
        size = max(largest, xref_number) + 1
        entries = bytearray()
        for number in range(size):
            if number == 0:
                kind, field1, field2 = 0, 0, 65_535
            elif number == xref_number:
                kind, field1, field2 = 1, startxref, 0
            elif number in compressed:
                container, index = compressed[number]
                kind, field1, field2 = 2, container, index
            elif number in self._objects:
                generation, _ = self._objects[number]
                kind, field1, field2 = 1, offsets[number], generation
            else:
                kind, field1, field2 = 0, 0, 0
            if field1 >= 1 << 64 or field2 >= 1 << 16:
                raise OverflowError("xref stream field exceeds /W [1 8 2]")
            entries += bytes([kind]) + field1.to_bytes(8, "big") + field2.to_bytes(2, "big")
        root_number, root_generation = self.root
        dictionary = (
            b"/Type /XRef /Size %d /Root %d %d R /W [1 8 2] /Index [0 %d]"
            % (size, root_number, root_generation, size)
        )
        xref_body = _stream(bytes(entries), dictionary)
        body += f"{xref_number} 0 obj\n".encode("ascii") + xref_body + b"\nendobj\n"
        body += b"startxref\n%d\n%%%%EOF\n" % startxref
        return RenderedPdf(bytes(body), startxref)


def _write_classic_streaming(
    path: Path,
    objects: Iterable[tuple[int, int, bytes]],
    root: tuple[int, int] = (1, 0),
) -> int:
    """Write ordered objects without retaining their bodies or the finished PDF."""
    entries: list[tuple[int, int, bool]] = [(0, 65_535, False)]
    previous = 0
    with path.open("wb") as output:
        output.write(HEADER)
        for number, generation, body in objects:
            if number <= previous:
                raise ValueError("streaming objects must be strictly increasing")
            if not 0 <= generation <= 65_534:
                raise ValueError(f"invalid generation: {generation}")
            while len(entries) < number:
                entries.append((0, 0, False))
            offset = output.tell()
            if offset > 9_999_999_999:
                raise OverflowError("classic xref offset exceeds ten digits")
            output.write(f"{number} {generation} obj\n".encode("ascii"))
            output.write(body)
            output.write(b"\nendobj\n")
            entries.append((offset, generation, True))
            previous = number
        startxref = output.tell()
        output.write(f"xref\n0 {len(entries)}\n".encode("ascii"))
        for offset, generation, live in entries:
            marker = "n" if live else "f"
            output.write(f"{offset:010d} {generation:05d} {marker} \n".encode("ascii"))
        root_number, root_generation = root
        output.write(
            b"trailer\n<< /Size %d /Root %d %d R >>\nstartxref\n%d\n%%%%EOF\n"
            % (len(entries), root_number, root_generation, startxref)
        )
    return startxref


def _base_writer(probe: bytes, text: str = "lazy fixture") -> PdfWriter:
    writer = PdfWriter()
    writer.add(1, b"<< /Type /Catalog /Pages 2 0 R /Probe " + probe + b" >>")
    writer.add(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
    writer.add(
        3,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
    )
    writer.add(4, _text_stream(text))
    writer.add(5, FONT)
    return writer


def _reference_chain(hops: int) -> PdfWriter:
    if hops < 1:
        raise ValueError("a chain must contain at least one reference hop")
    writer = _base_writer(b"6 0 R", f"reference chain {hops}")
    final_number = 6 + hops - 1
    for number in range(6, final_number):
        writer.add(number, f"{number + 1} 0 R".encode("ascii"))
    writer.add(final_number, b"(resolved-value)")
    return writer


def _incremental_pdf() -> RenderedPdf:
    base = _base_writer(b"(incremental)", "base revision").render_classic()
    body = bytearray(base.data)
    object_offset = len(body)
    body += b"4 0 obj\n" + _text_stream("incremental revision") + b"\nendobj\n"
    startxref = len(body)
    body += b"xref\n4 1\n%010d 00000 n \n" % object_offset
    body += (
        b"trailer\n<< /Size 6 /Root 1 0 R /Prev %d >>\nstartxref\n%d\n%%%%EOF\n"
        % (base.startxref, startxref)
    )
    return RenderedPdf(bytes(body), startxref)


def _object_stream_pdf() -> RenderedPdf:
    writer = _base_writer(b"7 0 R", "object stream")
    first_member = b"(compressed-value)"
    second_member = b"<< /Kind /Compressed >>"
    header = b"7 0 8 %d " % (len(first_member) + 1)
    payload = header + first_member + b" " + second_member
    writer.add(6, _stream(payload, b"/Type /ObjStm /N 2 /First %d" % len(header)))
    return writer.render_xref_stream({7: (6, 0), 8: (6, 1)}, xref_number=9)


def _malformed_stream_pdf(kind: str) -> RenderedPdf:
    writer = _base_writer(b"(malformed-stream)", "malformed stream")
    payload = b"BT /F1 12 Tf 72 720 Td (malformed stream) Tj ET"
    if kind == "missing-length":
        body = b"<< >>\nstream\n" + payload + b"\nendstream"
    elif kind == "short-length":
        body = b"<< /Length 4 >>\nstream\n" + payload + b"\nendstream"
    else:
        raise ValueError(kind)
    writer._objects[4] = (0, body)
    return writer.render_classic()


def _small_fixtures() -> Iterable[tuple[str, RenderedPdf, dict]]:
    classic = _base_writer(b"(direct-value)", "classic xref").render_classic()
    yield "classic.pdf", classic, {"xref": "classic", "probe": "direct"}

    xref_stream = _base_writer(b"(direct-value)", "xref stream").render_xref_stream()
    yield "xref-stream.pdf", xref_stream, {"xref": "stream", "probe": "direct"}

    yield "incremental.pdf", _incremental_pdf(), {
        "xref": "classic-incremental", "revisions": 2, "probe": "direct"
    }
    yield "object-stream.pdf", _object_stream_pdf(), {
        "xref": "stream", "object_stream": 6, "compressed_objects": [7, 8]
    }

    for hops, name in ((1, "reference-one-hop.pdf"), (128, "reference-at-limit.pdf"), (129, "reference-over-limit.pdf")):
        yield name, _reference_chain(hops).render_classic(), {
            "xref": "classic", "probe": "reference-chain", "reference_hops": hops
        }

    dangling = _base_writer(b"999 0 R", "dangling reference").render_classic()
    yield "reference-dangling.pdf", dangling, {
        "xref": "classic", "probe": "dangling", "target": [999, 0]
    }

    cycle = _base_writer(b"6 0 R", "cyclic reference")
    cycle.add(6, b"7 0 R")
    cycle.add(7, b"6 0 R")
    yield "reference-cycle.pdf", cycle.render_classic(), {
        "xref": "classic", "probe": "cycle", "cycle": [[6, 0], [7, 0]]
    }

    matched = _base_writer(b"6 1 R", "matched generation")
    matched.add(6, b"(generation-one)", generation=1)
    yield "generation-match.pdf", matched.render_classic(), {
        "xref": "classic", "probe": "reference", "target": [6, 1], "actual": [6, 1]
    }

    mismatched = _base_writer(b"6 1 R", "mismatched generation")
    mismatched.add(6, b"(generation-zero)", generation=0)
    yield "generation-mismatch.pdf", mismatched.render_classic(), {
        "xref": "classic", "probe": "generation-mismatch", "target": [6, 1], "actual": [6, 0]
    }

    yield "stream-missing-length.pdf", _malformed_stream_pdf("missing-length"), {
        "xref": "classic", "malformed": "missing-stream-length"
    }
    yield "stream-short-length.pdf", _malformed_stream_pdf("short-length"), {
        "xref": "classic", "malformed": "short-stream-length"
    }


SCALE_AXES = ("pages", "objects", "links", "headings", "tags", "assets")


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _scale_objects(axis: str, count: int) -> Iterator[tuple[int, int, bytes]]:
    if axis == "pages":
        first_page = 3
        first_content = first_page + count
        font = first_content + count
        kids = b" ".join(f"{first_page + i} 0 R".encode("ascii") for i in range(count))
        yield 1, 0, b"<< /Type /Catalog /Pages 2 0 R >>"
        yield 2, 0, b"<< /Type /Pages /Kids [" + kids + b"] /Count %d >>" % count
        for index in range(count):
            yield first_page + index, 0, (
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
                b"/Resources << /Font << /F1 %d 0 R >> >> /Contents %d 0 R >>"
                % (font, first_content + index)
            )
        for index in range(count):
            yield first_content + index, 0, _text_stream(f"unique page {index:05d}")
        yield font, 0, FONT
        return

    if axis == "objects":
        refs = b" ".join(f"{6 + i} 0 R".encode("ascii") for i in range(count))
        writer = _base_writer(b"[" + refs + b"]", f"{count} unique objects")
        for number, (generation, body) in sorted(writer._objects.items()):
            yield number, generation, body
        for index in range(count):
            yield 6 + index, 0, b"<< /Ordinal %d /Token (object-%05d) >>" % (index, index)
        return

    if axis == "links":
        first_annot = 6
        refs = b" ".join(f"{first_annot + i} 0 R".encode("ascii") for i in range(count))
        yield 1, 0, b"<< /Type /Catalog /Pages 2 0 R >>"
        yield 2, 0, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"
        yield 3, 0, (
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R /Annots ["
            + refs + b"] >>"
        )
        yield 4, 0, _text_stream(f"{count} unique links")
        yield 5, 0, FONT
        for index in range(count):
            x = index % 500
            y = (index // 500) % 700
            yield first_annot + index, 0, (
                b"<< /Type /Annot /Subtype /Link /Rect [%d %d %d %d] "
                b"/A << /S /URI /URI (https://example.invalid/%05d) >> >>"
                % (x, y, x + 1, y + 1, index)
            )
        return

    if axis == "headings":
        operations = []
        for index in range(count):
            x = 36 + (index % 8) * 70
            y = 756 - (index % 50) * 15
            operations.append(
                b"BT /F1 18 Tf %d %d Td (Heading %05d) Tj ET" % (x, y, index)
            )
        writer = _base_writer(b"(headings)", f"{count} unique headings")
        writer._objects[4] = (0, _stream(b"\n".join(operations)))
        for number, (generation, body) in sorted(writer._objects.items()):
            yield number, generation, body
        return

    if axis == "tags":
        first_elem = 8
        refs = b" ".join(f"{first_elem + i} 0 R".encode("ascii") for i in range(count))
        operations = []
        for index in range(count):
            x = 36 + (index % 8) * 70
            y = 756 - (index % 50) * 15
            operations.append(
                b"/P <</MCID %d>> BDC BT /F1 10 Tf %d %d Td (Tag %05d) Tj ET EMC"
                % (index, x, y, index)
            )
        yield 1, 0, b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 6 0 R >>"
        yield 2, 0, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"
        yield 3, 0, (
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /StructParents 0 "
            b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
        )
        yield 4, 0, _stream(b"\n".join(operations))
        yield 5, 0, FONT
        yield 6, 0, (
            b"<< /Type /StructTreeRoot /K [" + refs + b"] /ParentTree 7 0 R /ParentTreeNextKey 1 >>"
        )
        yield 7, 0, b"<< /Nums [0 [" + refs + b"]] >>"
        for index in range(count):
            yield first_elem + index, 0, (
                b"<< /Type /StructElem /S /P /P 6 0 R /Pg 3 0 R /K %d >>" % index
            )
        return

    if axis == "assets":
        first_image = 5
        resources = b" ".join(
            b"/Im%d %d 0 R" % (index, first_image + index) for index in range(count)
        )
        draws = b"\n".join(
            b"q 1 0 0 1 %d %d cm /Im%d Do Q" % (index % 612, (index // 612) % 792, index)
            for index in range(count)
        )
        yield 1, 0, b"<< /Type /Catalog /Pages 2 0 R >>"
        yield 2, 0, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"
        yield 3, 0, (
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            b"/Resources << /XObject << " + resources + b" >> >> /Contents 4 0 R >>"
        )
        yield 4, 0, _stream(draws)
        for index in range(count):
            samples = index.to_bytes(4, "big")[1:]
            yield first_image + index, 0, _stream(
                samples,
                b"/Type /XObject /Subtype /Image /Width 1 /Height 1 "
                b"/ColorSpace /DeviceRGB /BitsPerComponent 8",
            )
        return

    raise ValueError(f"unknown scale axis: {axis}")


def generate_small(output: Path) -> dict:
    output.mkdir(parents=True, exist_ok=True)
    rows = []
    for name, rendered, facts in _small_fixtures():
        path = output / name
        path.write_bytes(rendered.data)
        rows.append({
            "name": name,
            "bytes": len(rendered.data),
            "sha256": hashlib.sha256(rendered.data).hexdigest(),
            "startxref": rendered.startxref,
            "facts": facts,
        })
    manifest = {"schema": 1, "profile": "small", "fixtures": rows}
    (output / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return manifest


def generate_scale(output: Path, axis: str, count: int) -> dict:
    if axis not in SCALE_AXES:
        raise ValueError(f"unknown scale axis: {axis}")
    if not 1 <= count <= 100_000:
        raise ValueError("scale count must be between 1 and 100000")
    output.mkdir(parents=True, exist_ok=True)
    name = f"{axis}-{count}.pdf"
    path = output / name
    startxref = _write_classic_streaming(path, _scale_objects(axis, count))
    facts = {
        "axis": axis,
        "unique_count": count,
        "pages": count if axis == "pages" else 1,
        "generated_on_demand": True,
        "output_retained_by_generator": False,
    }
    row = {
        "name": name,
        "bytes": path.stat().st_size,
        "sha256": _sha256_file(path),
        "startxref": startxref,
        "facts": facts,
    }
    manifest = {"schema": 1, "profile": "scale", "fixtures": [row]}
    (output / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return manifest


def verify(output: Path) -> dict:
    manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
    if manifest.get("schema") != 1:
        raise ValueError("unsupported lazy-engine fixture manifest schema")
    expected = {row["name"] for row in manifest["fixtures"]} | {"manifest.json"}
    actual = {path.name for path in output.iterdir() if path.is_file()}
    if actual != expected:
        raise ValueError(f"fixture file set mismatch: expected={sorted(expected)} actual={sorted(actual)}")
    for row in manifest["fixtures"]:
        data = (output / row["name"]).read_bytes()
        if len(data) != row["bytes"]:
            raise ValueError(f"size mismatch: {row['name']}")
        if hashlib.sha256(data).hexdigest() != row["sha256"]:
            raise ValueError(f"digest mismatch: {row['name']}")
        marker = b"startxref\n" + str(row["startxref"]).encode("ascii") + b"\n"
        if marker not in data:
            raise ValueError(f"startxref mismatch: {row['name']}")
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("generate", "verify"))
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--profile", choices=("small", "scale"), default="small")
    parser.add_argument("--axis", choices=SCALE_AXES)
    parser.add_argument("--count", type=int)
    args = parser.parse_args()
    if args.command == "verify":
        manifest = verify(args.out)
    elif args.profile == "small":
        manifest = generate_small(args.out)
    else:
        if args.axis is None or args.count is None:
            parser.error("scale generation requires --axis and --count")
        manifest = generate_scale(args.out, args.axis, args.count)
    print(json.dumps({"profile": manifest["profile"], "fixtures": len(manifest["fixtures"])}, sort_keys=True))


if __name__ == "__main__":
    main()
