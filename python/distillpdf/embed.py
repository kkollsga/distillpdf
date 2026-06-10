"""Text embedding for distillpdf — BAAI/bge-m3 vectors for semantic search over a `.dpdf`.

Like :mod:`distillpdf.ocr`, the heavy dependencies are OPTIONAL and imported lazily: this
module is always importable, and the moment you actually embed without the runtime installed
you get a precise, actionable :class:`EmbedDependencyError` (the exact ``pip install`` line),
not an opaque ``ModuleNotFoundError``.

THE EMBEDDER — one model, two interchangeable backends, identical vectors
-------------------------------------------------------------------------
The model is **BAAI/bge-m3** (1024-dim, 8192-token cap), run via ONNX Runtime exactly as the
user's kglite stack runs it, so a vector distillpdf writes is byte-for-byte comparable with one
kglite writes. Backend resolution (:func:`make_embedder`):

* **(a) kglite present** — if ``kglite.mcp_server.bge_m3.BgeM3Embedder`` imports, we use it
  directly. Zero duplication; guaranteed-identical tokenization/pooling.
* **(b) vendored twin** — otherwise :class:`BgeM3Embedder` here is a faithful port of that
  class: SAME ONNX files (``onnx/model.onnx`` + ``onnx/model.onnx_data`` + ``onnx/tokenizer.json``
  from ``BAAI/bge-m3`` via ``huggingface_hub``), SAME 8192 truncation, SAME pad-to-batch-max,
  SAME CLS pooling (hidden state of token 0). Same cache conventions
  (``FASTEMBED_CACHE_PATH`` env, HF-hub layout under ``~/.cache/fastembed``), so a cache one
  stack downloaded is reused by the other.

Both backends return RAW CLS vectors; :func:`embed_normalized` (used by the embed/search paths)
L2-normalizes them so cosine similarity is a plain dot product and the stored space is
``normalized: true``. Normalizing on top of either backend yields identical unit vectors — the
two paths are interchangeable to within float noise.
"""
from __future__ import annotations

import importlib.util
import math
import os
from pathlib import Path
from typing import Any, List, Optional, Sequence

#: The embedding model — recorded verbatim in each embedding space's ``model`` field.
MODEL_ID = "BAAI/bge-m3"
DIMENSION = 1024
MAX_LENGTH = 8192  # bge-m3 model_max_length

#: The optional runtime an embed/search needs (the same trio kglite uses).
_REQUIRED = ("onnxruntime", "tokenizers", "huggingface_hub")


class EmbedDependencyError(ImportError):
    """Raised when the optional embedding runtime (onnxruntime / tokenizers / huggingface_hub)
    is not installed. Carries the exact ``pip install`` line."""


def install_help() -> str:
    """What to install to embed / semantic-search — the user-facing helper, mirroring
    :func:`distillpdf.ocr.install_help`. The embedder is BAAI/bge-m3 on ONNX Runtime; the
    weights download from HuggingFace on first use (or point ``HF_HOME`` / ``cache_dir`` at an
    existing copy to run offline)."""
    return (
        "Semantic search needs the optional embedding runtime (BAAI/bge-m3 on ONNX Runtime):\n"
        "    pip install onnxruntime tokenizers huggingface_hub\n"
        "    pip install numpy        # optional but recommended (faster cosine; onnxruntime "
        "pulls it in anyway)\n"
        "The ~2.3 GB bge-m3 weights download from HuggingFace on first run and cache under "
        "~/.cache/fastembed (override with FASTEMBED_CACHE_PATH, or point HF_HOME / a cache_dir "
        "at an existing copy to run offline)."
    )


def _require_runtime() -> None:
    """Raise :class:`EmbedDependencyError` (with the install line) if any required module is
    missing — checked import-light via ``find_spec`` so the error is precise and no heavy import
    happens just to report it."""
    missing = [m for m in _REQUIRED if importlib.util.find_spec(m) is None]
    if missing:
        raise EmbedDependencyError(
            f"distillpdf's semantic search needs the optional package(s) "
            f"{', '.join(repr(m) for m in missing)}, which aren't installed.\n\n{install_help()}"
        )


def runtime_available() -> bool:
    """True when the embedding runtime is importable — lets callers (the CLI, tests) branch to
    an actionable message before touching a model. Import-light."""
    return all(importlib.util.find_spec(m) is not None for m in _REQUIRED)


def kglite_available() -> bool:
    """True when kglite's own BgeM3Embedder is importable — then :func:`make_embedder` uses it
    directly (identical vectors, zero duplication)."""
    return importlib.util.find_spec("kglite") is not None and importlib.util.find_spec(
        "kglite.mcp_server.bge_m3"
    ) is not None


def _default_cache_dir() -> Path:
    """Where bge-m3 weights land by default — matches kglite's BgeM3Embedder so a cache one
    stack downloaded is shared. ``FASTEMBED_CACHE_PATH`` overrides; HF-hub layout
    (``<cache>/models--BAAI--bge-m3/``) underneath."""
    return Path(os.environ.get("FASTEMBED_CACHE_PATH", Path.home() / ".cache" / "fastembed"))


class BgeM3Embedder:
    """Vendored twin of kglite's ``BgeM3Embedder`` (used when kglite isn't installed).

    A faithful port: same model files, same 8192-token truncation, same pad-to-batch-max, same
    CLS pooling (hidden state at token 0), same cache conventions. Returns RAW CLS vectors
    (un-normalized) — exactly like kglite — so the two are interchangeable; the embed/search
    paths L2-normalize on top via :func:`embed_normalized`.
    """

    dimension = DIMENSION

    def __init__(self, cache_dir: Optional[Path] = None) -> None:
        self._cache_dir = Path(cache_dir) if cache_dir is not None else _default_cache_dir()
        self._session: Any = None
        self._tokenizer: Any = None
        self._input_names: List[str] = []

    def load(self) -> None:
        """Materialise the ONNX session + tokenizer (idempotent). Downloads the weights on first
        use unless they're already cached; raises :class:`EmbedDependencyError` if the runtime
        is missing."""
        if self._session is not None:
            return
        _require_runtime()
        from huggingface_hub import hf_hub_download
        import onnxruntime as ort
        from tokenizers import Tokenizer

        model_path = hf_hub_download(
            repo_id=MODEL_ID, filename="onnx/model.onnx", cache_dir=str(self._cache_dir)
        )
        # The external weights shard must sit next to model.onnx (onnxruntime resolves the
        # relative path inside the graph).
        hf_hub_download(
            repo_id=MODEL_ID, filename="onnx/model.onnx_data", cache_dir=str(self._cache_dir)
        )
        tokenizer_path = hf_hub_download(
            repo_id=MODEL_ID, filename="onnx/tokenizer.json", cache_dir=str(self._cache_dir)
        )

        sess_opts = ort.SessionOptions()
        sess_opts.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        session = ort.InferenceSession(
            model_path, sess_options=sess_opts, providers=["CPUExecutionProvider"]
        )
        self._input_names = [i.name for i in session.get_inputs()]
        tokenizer = Tokenizer.from_file(tokenizer_path)
        tokenizer.enable_truncation(max_length=MAX_LENGTH)
        tokenizer.enable_padding(pad_id=1, pad_token="<pad>")
        self._session, self._tokenizer = session, tokenizer

    def release(self) -> None:
        """Drop the ONNX session + tokenizer (reclaim ~2 GB)."""
        self._session = self._tokenizer = None

    def embed(self, texts: Sequence[str]) -> List[List[float]]:
        """RAW CLS embeddings for ``texts`` (un-normalized, like kglite). One ONNX run for the
        whole batch; pad to the batch's longest sequence."""
        if not texts:
            return []
        self.load()
        import numpy as np

        encoded = self._tokenizer.encode_batch(list(texts))
        max_len = max((len(e.ids) for e in encoded), default=1)
        input_ids = np.array(
            [e.ids + [1] * (max_len - len(e.ids)) for e in encoded], dtype=np.int64
        )
        attention_mask = np.array(
            [e.attention_mask + [0] * (max_len - len(e.attention_mask)) for e in encoded],
            dtype=np.int64,
        )
        feeds: dict[str, Any] = {}
        if "input_ids" in self._input_names:
            feeds["input_ids"] = input_ids
        if "attention_mask" in self._input_names:
            feeds["attention_mask"] = attention_mask
        if "token_type_ids" in self._input_names:
            feeds["token_type_ids"] = np.zeros_like(input_ids)
        outputs = self._session.run(None, feeds)
        # bge-m3 ONNX export: output[0] is last_hidden_state (batch, seq, hidden). CLS pool =
        # token 0 — matches fastembed-rs's Pooling::Cls and kglite's port.
        return outputs[0][:, 0, :].tolist()


def make_embedder(cache_dir: Optional[str | Path] = None) -> Any:
    """Construct the embedder, preferring kglite's own (identical vectors, zero duplication) and
    falling back to the vendored twin here. Both expose ``load()`` / ``embed(texts)`` /
    ``release()`` and produce the same RAW CLS vectors. Raises :class:`EmbedDependencyError` if
    the ONNX runtime is missing."""
    _require_runtime()
    cd = Path(cache_dir) if cache_dir is not None else None
    if kglite_available():
        from kglite.mcp_server.bge_m3 import BgeM3Embedder as _Kglite  # type: ignore

        # kglite's embedder takes the same cache_dir; cooldown=0 keeps it resident for a batch.
        return _Kglite(cache_dir=cd, cooldown_seconds=0)
    return BgeM3Embedder(cache_dir=cd)


def _l2_normalize(vec: Sequence[float]) -> List[float]:
    """Unit-normalize one vector (so cosine == dot product). A zero vector stays zero."""
    norm = math.sqrt(sum(x * x for x in vec))
    if norm == 0.0:
        return list(vec)
    return [x / norm for x in vec]


def embed_normalized(embedder: Any, texts: Sequence[str]) -> List[List[float]]:
    """Embed ``texts`` and L2-normalize each row — the form the embed/search paths store and
    compare (``normalized: true``). Works over either backend (raw CLS in, unit vectors out), so
    kglite-produced and vendored-produced spaces are interchangeable to float noise."""
    return [_l2_normalize(v) for v in embedder.embed(texts)]


def backend_name() -> str:
    """Which backend :func:`make_embedder` would pick — for diagnostics / the CLI."""
    return "kglite" if kglite_available() else "vendored"


# ---- the binary vector member (little-endian f32, row-major) ----------------
#
# Vectors are NOT in model.json — they live as a raw f32 matrix in a container member
# (`embeddings/<id>.bin`), `n_rows × dim`, rows in the chunk-id order the space records. Raw
# f32 keeps the file tiny and the read trivial; the row order + dim live in the metadata.

import struct


def pack_vectors(vectors: Sequence[Sequence[float]], dim: int) -> bytes:
    """Pack a list of equal-length vectors to the container-member bytes: little-endian f32,
    row-major. Raises on a row whose length isn't ``dim`` (a half-record is a loud error)."""
    buf = bytearray()
    for i, v in enumerate(vectors):
        if len(v) != dim:
            raise ValueError(f"vector {i} has length {len(v)}, expected dim {dim}")
        buf += struct.pack(f"<{dim}f", *v)
    return bytes(buf)


def unpack_vectors(data: bytes, dim: int) -> List[List[float]]:
    """Unpack container-member bytes into ``n × dim`` rows. Raises when the byte length isn't a
    whole number of ``dim``-float rows (a truncated/corrupt member is loud, never silent)."""
    if dim <= 0:
        raise ValueError(f"invalid dim {dim}")
    row_bytes = dim * 4
    if len(data) % row_bytes != 0:
        raise ValueError(
            f"embedding bytes ({len(data)}) not a multiple of the row size ({row_bytes} = "
            f"{dim} dim × 4); the member is truncated or the dim is wrong"
        )
    n = len(data) // row_bytes
    return [list(struct.unpack_from(f"<{dim}f", data, i * row_bytes)) for i in range(n)]


def cosine_topk(query: Sequence[float], matrix: Sequence[Sequence[float]], k: int) -> List[tuple]:
    """Rank ``matrix`` rows by cosine similarity to ``query`` and return the top ``k`` as
    ``(row_index, score)`` descending. Vectors are stored L2-normalized, so cosine is a plain
    dot product. Uses numpy when importable (onnxruntime pulls it in anyway); falls back to a
    pure-Python dot loop (n is small — one document's chunks). The query is normalized here so a
    raw query vector ranks correctly either way."""
    q = _l2_normalize(query)
    if importlib.util.find_spec("numpy") is not None:
        import numpy as np

        m = np.asarray(matrix, dtype=np.float32)
        if m.size == 0:
            return []
        scores = m @ np.asarray(q, dtype=np.float32)
        order = np.argsort(-scores)[: max(0, k)]
        return [(int(i), float(scores[i])) for i in order]
    scored = [(i, sum(a * b for a, b in zip(q, row))) for i, row in enumerate(matrix)]
    scored.sort(key=lambda t: t[1], reverse=True)
    return [(i, float(s)) for i, s in scored[: max(0, k)]]
