import json
import os
from pathlib import Path

from eager_oracle import build_manifest


def test_python_eager_public_surface_matches_frozen_oracle(tmp_path):
    actual = json.dumps(build_manifest(tmp_path), indent=2, sort_keys=True) + "\n"
    snapshot = Path(__file__).with_name("oracles") / "eager-python.json"
    if os.environ.get("DISTILLPDF_UPDATE_EAGER_ORACLES"):
        snapshot.parent.mkdir(parents=True, exist_ok=True)
        snapshot.write_text(actual, encoding="utf-8")
    expected = snapshot.read_text(encoding="utf-8")
    assert actual == expected, "Python eager oracle drift; inspect behavior before updating the snapshot"


def test_oracles_are_independent_and_cover_required_surface_groups():
    oracle_dir = Path(__file__).with_name("oracles")
    rust = json.loads((oracle_dir / "eager-rust.json").read_text(encoding="utf-8"))
    python = json.loads((oracle_dir / "eager-python.json").read_text(encoding="utf-8"))

    assert rust["layer"] == "rust"
    assert python["layer"] == "python"
    assert rust["surfaces"] != python["surfaces"]
    prefixes = {
        "open", "text", "images", "fonts", "tables", "links", "html", "markdown",
        "toc", "outline", "section", "front_matter", "ocr", "stream", "malformed",
        "model", "container", "searchable_pdf", "error",
    }
    for manifest in (rust, python):
        present = {name.split(".", 1)[0] for name in manifest["surfaces"]}
        assert prefixes <= present
        assert manifest["normalizations"] == [
            "source.generated_at=<normalized>",
            "embedding_spaces[*].generated_at=<normalized>",
        ]
