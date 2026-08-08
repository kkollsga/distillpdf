import json
import subprocess
import sys
from pathlib import Path

import pytest

import distillpdf
from lazy_engine_fixtures import (
    IMAGE_VARIANTS,
    OBJSTM_BOUNDARY_CASES,
    OBJSTM_CONTAINER_CASES,
    OBJSTM_ENCRYPTED_NAME,
    SCALE_AXES,
    generate_image,
    generate_objstm_container,
    generate_objstm_boundary,
    generate_scale,
    generate_semantic,
    generate_small,
    verify,
)


OBJSTM_ORACLE = Path(__file__).with_name("oracles") / "l3b-objstm-container.json"
OBJSTM_COMMITTED = Path(__file__).with_name("fixtures_pdf") / "objstm"
OBJSTM_R4_RC4_SHA256 = "afadac4e544281ce0abf12f459f1927da983469a2cc40c2a0c1f7d26f2894694"
OBJSTM_CACHE_BOUNDARY_ORACLES = {
    "plain-cache-below": {
        "bytes": 33_424_113,
        "decoded": 33_423_360,
        "sha256": "c3f723fdae1022e20240309f01d1de77460debc3f2d8cd15aee576cfb2e82d6c",
        "startxref": 33_423_883,
        "expected": "retained-below-32m-candidate",
    },
    "plain-cache-above": {
        "bytes": 33_686_257,
        "decoded": 33_685_504,
        "sha256": "aa4206ba1fba2505b81798098980921344dd407018f469639cda8d22132a99b7",
        "startxref": 33_686_027,
        "expected": "retained-above-32m-candidate",
    },
}


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


def test_objstm_container_profile_is_frozen_dependency_free_and_self_verifying(tmp_path):
    left = tmp_path / "left"
    right = tmp_path / "right"
    first = generate_objstm_container(left)
    second = generate_objstm_container(right)
    frozen = json.loads(OBJSTM_ORACLE.read_text(encoding="utf-8"))

    assert first == second == frozen
    assert verify(left) == first
    assert verify(right) == second
    assert {row["name"] for row in first["fixtures"]} == {
        *OBJSTM_CONTAINER_CASES,
        OBJSTM_ENCRYPTED_NAME,
    }
    for row in first["fixtures"]:
        name = row["name"]
        assert (left / name).read_bytes() == (right / name).read_bytes()
        assert row["facts"]["containers"]
        assert row["facts"]["xref_declarations"]
        assert row["facts"]["expected_preparation"]
        assert row["facts"]["storage"] in {"generated-small", "committed-small"}


def test_committed_r4_rc4_objstm_manifest_and_hash_are_independent_runtime_authorities():
    import hashlib

    manifest = json.loads((OBJSTM_COMMITTED / "manifest.json").read_text(encoding="utf-8"))
    assert manifest["schema"] == 1
    assert manifest["profile"] == "objstm-encrypted"
    assert len(manifest["fixtures"]) == 1
    row = manifest["fixtures"][0]
    raw = (OBJSTM_COMMITTED / OBJSTM_ENCRYPTED_NAME).read_bytes()
    assert hashlib.sha256(raw).hexdigest() == OBJSTM_R4_RC4_SHA256 == row["sha256"]
    assert len(raw) == row["bytes"] == 1128
    assert row["startxref"] == 817
    assert row["facts"] == {
        "cipher": "RC4-128",
        "containers": [2],
        "decoded_lengths": [252],
        "encoded_lengths": [182],
        "encoding": "flate-encrypted-r4-rc4",
        "encrypt_metadata": False,
        "encrypted": True,
        "encryption_revision": 4,
        "expected_preparation": "ready",
        "generator": "pikepdf-10.7.3/libqpdf-12.3.2",
        "owner_password": "owner",
        "storage": "committed-small",
        "user_password": "",
        "xref_declarations": [
            {"container": 2, "index": 0, "object": 3},
            {"container": 2, "index": 1, "object": 4},
            {"container": 2, "index": 2, "object": 5},
            {"container": 2, "index": 3, "object": 6},
        ],
    }
    assert b"/R 4" in raw
    assert b"/V 4" in raw
    assert b"/CFM /V2" in raw
    assert b"owner" not in raw


def test_objstm_subdirectory_is_excluded_from_generic_root_fixture_sweeps():
    from _htmlcheck import doc_pdfs, owned_pdfs

    committed = (OBJSTM_COMMITTED / OBJSTM_ENCRYPTED_NAME).resolve()
    assert committed.exists()
    assert committed not in {Path(path).resolve() for path in owned_pdfs()}
    assert committed not in {Path(path).resolve() for path in doc_pdfs()}


@pytest.mark.parametrize(
    "variant",
    ["encoded-over-64m", "encoded-within-cap-truncated", "flate-growth"],
)
def test_objstm_small_boundary_authorities_are_deterministic_and_self_verifying(tmp_path, variant):
    left = tmp_path / "left"
    right = tmp_path / "right"
    first = generate_objstm_boundary(left, variant)
    second = generate_objstm_boundary(right, variant)
    assert first == second
    assert verify(left) == first
    row = first["fixtures"][0]
    assert row["facts"]["storage"] == "on-demand-large"
    assert row["facts"]["containers"] == [6]
    assert row["facts"]["xref_declarations"] == [
        {"object": 7, "container": 6, "index": 0}
    ]
    assert (left / row["name"]).read_bytes() == (right / row["name"]).read_bytes()


def test_objstm_large_boundary_authorities_are_explicit_and_never_generated_by_small_profile():
    assert set(OBJSTM_BOUNDARY_CASES) == {
        "encoded-over-64m",
        "encoded-within-cap-truncated",
        "flate-growth",
        "plain-cache-below",
        "plain-cache-above",
        "plain-cap-edge",
    }


@pytest.mark.parametrize("variant", ["plain-cache-below", "plain-cache-above"])
def test_objstm_cache_boundary_authorities_have_frozen_hashes_and_raii_cleanup(tmp_path, variant):
    import hashlib
    from tempfile import TemporaryDirectory

    oracle = OBJSTM_CACHE_BOUNDARY_ORACLES[variant]
    generated_path = None
    with TemporaryDirectory(dir=tmp_path) as generated:
        generated_path = Path(generated)
        manifest = generate_objstm_boundary(generated_path, variant)
        assert verify(generated_path) == manifest
        row = manifest["fixtures"][0]
        raw_path = generated_path / row["name"]
        assert row["name"] == f"objstm-{variant}.pdf"
        assert row["bytes"] == raw_path.stat().st_size == oracle["bytes"]
        assert row["sha256"] == hashlib.sha256(raw_path.read_bytes()).hexdigest() == oracle["sha256"]
        assert row["startxref"] == oracle["startxref"]
        assert row["facts"] == {
            "containers": [6],
            "encoding": "plain",
            "encoded_lengths": [oracle["decoded"]],
            "decoded_lengths": [oracle["decoded"]],
            "xref_declarations": [{"object": 7, "container": 6, "index": 0}],
            "expected_preparation": oracle["expected"],
            "storage": "on-demand-large",
            "output_retained_by_generator": False,
        }
    assert generated_path is not None
    assert not generated_path.exists()


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


def _jpeg_ordinal(image):
    marker = b"L1OCR"
    start = image.index(marker) + len(marker)
    return int(image[start:start + 6])


def test_ocr_candidate_fixture_freezes_order_failure_and_cache_semantics(tmp_path):
    from distillpdf import ocr

    manifest = generate_semantic(tmp_path, "ocr", 7)
    row = manifest["fixtures"][0]
    pdf = distillpdf.Pdf.open(str(tmp_path / row["name"]))
    plan = pdf.ocr_plan()
    assert [item["page"] for item in plan if item["needs_ocr"]] == list(range(1, 8))
    assert [_jpeg_ordinal(item["image"]) for item in plan] == list(range(1, 8))

    class ReverseOracle(ocr.OcrBackend):
        tier = "fast"

        def __init__(self, fail=None):
            super().__init__()
            self.fail = fail
            self.calls = []

        def ocr_page(self, image):
            page = _jpeg_ordinal(image)
            self.calls.append(page)
            if page == self.fail:
                raise RuntimeError(f"barrier failure page {page}")
            return f"<text>Recovered OCR page {page:06d}</text>"

    backend = ReverseOracle()
    result = ocr.run(pdf, backend, progress=False)
    assert backend.calls == list(range(1, 8))
    assert list(result) == list(range(1, 8))
    assert pdf.get_ocr() == result

    failing_pdf = distillpdf.Pdf.open(str(tmp_path / row["name"]))
    failing = ReverseOracle(fail=row["facts"]["failure_page"])
    with pytest.raises(RuntimeError, match="barrier failure page 4"):
        ocr.run(failing_pdf, failing, progress=False)
    assert failing.calls == [1, 2, 3, 4]
    assert failing_pdf.get_ocr() == {}


def test_tagged_table_spans_pages_and_preserves_authored_rows(tmp_path):
    manifest = generate_semantic(tmp_path, "tagged-table", 5)
    row = manifest["fixtures"][0]
    pdf = distillpdf.Pdf.open(str(tmp_path / row["name"]))
    assert pdf.page_count() == 5
    html = pdf.to_html(mode="page", return_string=True, image_mode="drop")
    for page in range(1, 6):
        assert f"Row {page:04d} key" in html
        assert f"value {page:04d}" in html
    # This phase freezes authored structure, not today's table-admission policy. The
    # structure tree names one table with one row tied to each distinct page.
    raw = (tmp_path / row["name"]).read_bytes()
    assert raw.count(b"/S /Table") == 1
    assert raw.count(b"/S /TR") == 5
    for page_object in range(10, 15):
        assert b"/Pg %d 0 R" % page_object in raw


def test_cli_semantic_profile(tmp_path):
    script = Path(__file__).with_name("lazy_engine_fixtures.py")
    result = subprocess.run(
        [
            sys.executable,
            str(script),
            "generate",
            "--profile",
            "semantic",
            "--semantic",
            "ocr",
            "--count",
            "9",
            "--out",
            str(tmp_path),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(result.stdout) == {"fixtures": 1, "profile": "semantic"}
    assert verify(tmp_path)["fixtures"][0]["facts"]["ocr_candidates"] == 9


def test_native_ocr_child_accepts_generated_candidate(tmp_path):
    from distillpdf import _distillpdf as core

    if "tesseract" not in core.native_engines():
        pytest.skip("tesseract feature not in this build")
    manifest = generate_semantic(tmp_path, "ocr", 1)
    name = manifest["fixtures"][0]["name"]
    code = (
        "import json,distillpdf; "
        "p=distillpdf.Pdf.open(__import__('sys').argv[1]); "
        "b=p.ocr_plan()[0]['image']; "
        "r=distillpdf._distillpdf.ocr_page_native('tesseract',b,{'languages':['eng']}); "
        "print(json.dumps({'type':type(r).__name__,'engines':distillpdf._distillpdf.native_engines()}))"
    )
    child = subprocess.run(
        [sys.executable, "-c", code, str(tmp_path / name)],
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )
    result = json.loads(child.stdout)
    assert result["type"] == "str"
    assert "tesseract" in result["engines"]
