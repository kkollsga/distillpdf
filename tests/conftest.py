"""pytest config for the distillpdf test suite.

The real PDF corpora (``tests/corpus/``, ``tests/corpus_headers/``) are third-party
documents and are **gitignored**, so they exist only on a local checkout. On CI (a fresh
clone) they are absent, so every corpus-backed test is skipped and only the tests that run
on tracked data execute for real — chiefly ``test_tables.py`` against the synthetic,
committed ``tests/corpus_tables/``. Run locally (with the corpora present) for full
coverage.
"""
import glob
import os

import pytest

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS_PRESENT = bool(glob.glob(os.path.join(HERE, "corpus", "*.pdf")))

# Tests that only need tracked data and must always run (incl. on CI).
TRACKED_DATA_ONLY = {"test_tables.py"}


def pytest_collection_modifyitems(config, items):
    if CORPUS_PRESENT:
        return
    skip = pytest.mark.skip(reason="PDF corpus is gitignored (third-party); present only on a local checkout")
    for item in items:
        if os.path.basename(str(item.fspath)) in TRACKED_DATA_ONLY:
            continue
        item.add_marker(skip)
