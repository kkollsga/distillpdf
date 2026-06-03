"""distillpdf — pure-Rust PDF extraction on lopdf."""
from ._distillpdf import Pdf, __version__, from_bytes, open

__all__ = ["Pdf", "open", "from_bytes", "__version__"]
