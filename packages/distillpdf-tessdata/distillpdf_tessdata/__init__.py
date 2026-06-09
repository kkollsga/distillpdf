"""Optional Tesseract language data for distillPDF's fast OCR engine.

Installed via ``pip install 'distillpdf[languages]'``. distillPDF's Tesseract backend
discovers this package at runtime and points the engine at :func:`tessdata_dir`, so the
bundled languages work fully offline. The base ``distillpdf`` wheel ships English only.
"""
import os

__version__ = "0.0.1"

#: ISO codes shipped in this package.
LANGUAGES = ("eng", "por", "nor")


def tessdata_dir() -> str:
    """Absolute path to the directory holding the ``*.traineddata`` files."""
    return os.path.join(os.path.dirname(__file__), "tessdata")
