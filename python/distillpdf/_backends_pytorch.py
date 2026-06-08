"""Windows/Linux OCR backend for granite-docling via PyTorch — PLANNED (placeholder).

Paused for now. On Apple Silicon the default MLX backend (`_backends_mlx.py`) is used and
this module is never selected. On Windows/Linux `get_backend()` returns this placeholder,
which raises a clear, actionable error until the path is implemented.

Intended design (grounded in docling's own engines; same `ocr_page(bytes) -> str` contract,
native resolution, no tiling — the idefics3 processor splits the page internally):

  Model: ``ibm-granite/granite-docling-258M`` (official transformers checkpoint).

  Default — Transformers (works on Windows/Linux, CPU or CUDA):
      from transformers import AutoModelForImageTextToText, AutoProcessor
      from transformers import StoppingCriteriaList, StopStringCriteria
      model = AutoModelForImageTextToText.from_pretrained(repo, device_map=device, dtype="bfloat16")
      proc  = AutoProcessor.from_pretrained(repo)
      msgs  = [{"role":"user","content":[{"type":"image"},{"type":"text","text": prompt}]}]
      text  = proc.apply_chat_template(msgs, add_generation_prompt=True)
      inputs= proc(text=[text], images=[pil], return_tensors="pt").to(device)
      ids   = model.generate(**inputs, max_new_tokens=4096, do_sample=False,
                             stopping_criteria=StoppingCriteriaList([StopStringCriteria(
                                 proc.tokenizer, ["</doctag>", "<|end_of_text|>"])]))
      out   = proc.batch_decode(ids, skip_special_tokens=False)[0]   # keep DocTags tokens

  Accelerator — vLLM (Linux + CUDA, high throughput; not Windows, not py3.14):
      from vllm import LLM, SamplingParams
      llm = LLM(model=repo, limit_mm_per_prompt={"image": 1})
      out = llm.generate([{"prompt": text, "multi_modal_data": {"image": pil}}],
                         SamplingParams(temperature=0.0, max_tokens=4096))

  Packaging (a future `ocr-pytorch` extra; keeps Apple-Silicon users off PyTorch):
      "transformers>=4.57,<5; sys_platform != 'darwin' or platform_machine != 'arm64'",
      "torch;               sys_platform != 'darwin' or platform_machine != 'arm64'",
      "vllm;                sys_platform == 'linux'",
"""
from __future__ import annotations

from typing import Optional

from .ocr import OcrBackend, OcrConfig, OcrDependencyError, register_backend

_REPO = "ibm-granite/granite-docling-258M"

_NOT_READY = (
    "Windows/Linux OCR (granite-docling via PyTorch/vLLM) is not yet implemented. "
    "Apple Silicon users get the default MLX backend automatically. The PyTorch/vLLM "
    "path is planned — see python/distillpdf/_backends_pytorch.py for the intended design."
)


class PyTorchGraniteDoclingBackend(OcrBackend):
    """Placeholder for the PyTorch/vLLM path — raises until implemented."""

    name = "granite-docling-pytorch"
    output = "doctags"

    def __init__(self, config: Optional[OcrConfig] = None, **kwargs):
        super().__init__(config, **kwargs)
        if self.config.model_id is None:
            self.config.model_id = _REPO

    def ocr_page(self, image: bytes) -> str:
        raise OcrDependencyError(_NOT_READY)


register_backend("granite-docling-pytorch", PyTorchGraniteDoclingBackend)
