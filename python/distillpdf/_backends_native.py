"""Backends for Rust-native OCR engines (compiled into the distillPDF core).

A "native" engine is one that lives in the Rust extension — the bundled **Tesseract**
(fast tier, no extra, no download) and the bring-your-own **server** engine (HTTP to a
llama.cpp/granite ``llama-server``). They surface through the *same* `OcrBackend` registry
and selection as the Python granite backends, so users have one way to pick an engine.

Each native engine is invoked via the ``_distillpdf.ocr_page_native(engine, image, opts)``
pyfunction and returns DocTags — the identical contract every other backend emits, so the
pure-Rust parse/render pipeline is unchanged. ``_distillpdf.native_engines()`` reports which
engines were compiled into this wheel; a backend only registers if its engine is present, so
an engine that wasn't built never *appears* runnable.
"""
from __future__ import annotations

from typing import List

from .ocr import OcrBackend, register_backend

# The native pyfunctions may be absent on an older compiled core (e.g. running the pure-Python
# layer against a not-yet-rebuilt extension). Degrade gracefully to "no native engines".
try:
    from ._distillpdf import native_engines as _native_engines
    from ._distillpdf import ocr_page_native as _ocr_page_native
    _NATIVE: set = set(_native_engines())
except Exception:  # pragma: no cover - only when the extension predates this feature
    _NATIVE = set()

    def _ocr_page_native(engine, image, opts):  # type: ignore
        raise RuntimeError("native OCR engines are not compiled into this distillPDF build")


class NativeBackend(OcrBackend):
    """Wraps a Rust-native OCR engine behind the normal backend API. Subclasses set
    ``engine`` (the Rust-side key) and their capability attributes."""

    #: the Rust-side engine key passed to ``ocr_page_native``
    engine: str = "abstract-native"

    def ocr_page(self, image: bytes) -> str:
        return _ocr_page_native(self.engine, bytes(image), self._opts())

    def _opts(self) -> dict:
        """The engine-agnostic options dict crossing the PyO3 boundary. Only non-empty
        values are sent; the Rust engine ignores keys it doesn't use."""
        c = self.config
        raw = {
            "languages": list(c.languages or []),
            "dpi": c.dpi,
            "prompt": c.prompt,
            "max_tokens": c.max_tokens,
        }
        return {k: v for k, v in raw.items() if v not in (None, [], "")}

    @classmethod
    def is_available(cls) -> bool:
        return cls.engine in _NATIVE


class TesseractBackend(NativeBackend):
    """Bundled Tesseract — the fast, dependency-free default. Flat text (no tables).

    English, Portuguese and Norwegian (Bokmål) language data ship in the wheel, so all
    three work offline out of the box; more languages can be added via ``config.languages``
    (resolved from a ``TESSDATA`` path or downloaded on demand)."""

    name = "tesseract"
    engine = "tesseract"
    tier = "fast"
    structure_aware = False
    bundled = True
    offline = True
    languages = ("eng", "por", "nor")
    detail = "Bundled Tesseract (eng/por/nor) — fast, no extra, no download. Flat text, no tables."


class ServerBackend(NativeBackend):
    """Bring-your-own OCR: a llama.cpp/granite ``llama-server`` over HTTP. Accurate-tier
    quality, but you run the server (so it is not bundled / offline)."""

    name = "server"
    engine = "server"
    tier = "accurate"
    structure_aware = True
    bundled = False
    offline = False
    detail = "Your own llama.cpp/granite llama-server over HTTP (set host/port via config)."


# Register only engines actually compiled into this wheel, so selection/`--list-ocr-engines`
# never offers an engine that can't run.
def _register_available() -> None:
    if "tesseract" in _NATIVE:
        register_backend("tesseract", TesseractBackend)
    if "server" in _NATIVE:
        register_backend("server", ServerBackend)


_register_available()
