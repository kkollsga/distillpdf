# Core API consumer contract — kglite

**Owner:** distillPDF. This is the single durable copy.
**Status:** producer side **implemented but unmerged** (2026-07-15, PR #1,
branch `refactor/workspace-split`); consumer side **not built** as of
2026-07-29 — see "Current status" below.
**Raised by:** kglite, 2026-07-15
(`inbox/read/2026-07-15-from-kglite-core-rust-api-feedback.md`).

distillPDF produces the API this contract is about (`PdfDocument`, `DocModel`,
`DistillOptions`, `Error`), so distillPDF owns the obligation and keeps the
durable copy. The consumer holds a pointer, not a second copy.

## Current status (checked 2026-07-29)

- **Implemented here, but not on `main`.** The pyo3-free core split was written
  the day it was asked for and reported complete. `distillpdf` exposes
  `PdfDocument::{open, from_bytes}`, `distill`, `load_dpdf`, `DistillOptions`,
  `AssetProfile`, `DocModel`, and a structured `Error`, with wire structs under
  `distillpdf::model`; OCR/Tesseract is behind the opt-in `tesseract` feature
  (which is what pulls `cmake`), so `default-features = false` needs no C
  toolchain. **All of that lives on `refactor/workspace-split`, four commits
  ahead of `main`, still unmerged two weeks later.** `origin/main` is at 0.0.34
  in the pre-split single-crate layout.
- **Not built there.** kglite has no `knowledge_tree`, no `pdf` feature, and no
  reference to `distillpdf` anywhere under `crates/`. kglite acknowledged the
  landing on 2026-07-16 and filed the integration to its backlog as
  "consider-for-future".
- **Therefore:** treat this as a *standing* contract, not an active
  integration. The commitments below still bind distillPDF's core API — they
  are good API hygiene for any Rust consumer — but nothing depends on them
  today, and the crates.io-publish item below cannot even start until the split
  merges. Re-confirm before either side spends effort.

## What kglite wanted to consume

DistillPDF's **typed document model**, not rendered HTML/Markdown and not
canonical JSON re-parsed into graph records. The model already carries the
useful graph boundaries and provenance: pages, sections, blocks, tables,
figures, links, TOC, bounding boxes, confidence, OCR provenance, and optional
chunks. kglite maps those to graph records itself; the core stays
graph-agnostic.

## The Rust boundary

Shape, not exact naming:

```rust
let pdf = distillpdf::PdfDocument::open(path)?;
let distilled = pdf.distill(&DistillOptions::text_only())?;
let model: &DocModel = distilled.model();
```

Hard requirements, all met:

- `open` **and** `from_bytes`.
- A typed `DocModel` result with **borrowed** access to its major collections
  (pages, sections, blocks, links, TOC, chunks). Plain slices/accessors are
  enough — no custom callback or graph-specific adapter belongs in distillPDF.
- A typed `.dpdf` load path, for ingesting an existing artifact without
  re-extracting the PDF (`load_dpdf(path) -> (DocModel, AssetBytes)`).
- `Result<_, distillpdf::Error>` with a structured error implementing
  `std::error::Error + Send + Sync`. **No `String` errors at the consumer
  boundary.**
- **Asset bytes kept separate from the typed model**, so a no-assets profile
  pays nothing for binaries. The result may retain an optional member map for
  other consumers.
- **Stored chunks exposed when present.** The deterministic chunk derivation
  currently implemented in Python should *eventually* move behind a pure-Rust
  helper so consumers don't duplicate the policy — but that is a **follow-up,
  explicitly not part of the behaviour-preserving split**. No embedded vector
  members and no embedding runtime are needed; the consumer owns its own graph
  embedding lifecycle.

## Re-exports

Keep the crate root small: `PdfDocument`, `DocModel`, `DistillOptions`,
`DistilledDocument`, `AssetProfile`, `Error`, plus `open` / `from_bytes` /
typed `.dpdf` load conveniences. Detailed wire types stay under
`distillpdf::model` (`Block`, `BlockKind`, `Section`, `Page`, `Chunk`, `Link`,
and related provenance types). This keeps the common path ergonomic without
turning every model type into permanent root-level surface.

## Features and dependency policy

- **OCR/Tesseract stays opt-in** and absent under `default-features = false`.
  A consumer's PDF feature must build without CMake or a C/C++ toolchain.
- **No embedding-model or ML runtime dependency in the core default feature
  set.** Chunk/model metadata and serde support are lightweight data contracts
  and stay available without an embedding runtime.
- **Rayon needs no gate.** kglite already depends on Rayon directly, so gating
  it buys that consumer nothing. Do not complicate the split for kglite's sake;
  an optional `parallel` feature is only worthwhile if distillPDF independently
  wants to maintain a serial implementation for *other* consumers.
- **Acceptance test** (kglite side): `cargo tree -p kglite | grep -i pyo3`
  produces no output for the Rust engine. distillPDF's own equivalent was
  verified at the split: a scratch consumer with
  `distillpdf = { path = …, default-features = false }` builds with zero pyo3
  in `cargo tree` and no C toolchain.

## Naming and versioning

- Core keeps the `distillpdf` crate name; `distillpdf-python` is the binding
  crate; the PyPI import stays `distillpdf`. **Confirmed.**
- A sibling path dependency is fine for integration development, but a
  consumer **cannot publish a crates.io package containing a path dependency**.
  Publishing the core is therefore not a blocker for the workspace split, but
  it *is* a release blocker for any downstream that wants to ship a `pdf`
  feature. Tracked in distillPDF's backlog as that downstream's blocker.
- While the model/API remains 0.0.x, consumers pin the exact compatible
  release and absorb deliberate API/model changes explicitly.

## History

- 2026-07-15 — kglite raises the contract (core-split API-shape feedback).
- 2026-07-15 — the split is code-complete and green on PR #1
  (`distillpdf` pyo3-free rlib + `distillpdf-python`); reported to kglite.
- 2026-07-16 — kglite acknowledges; integration filed to backlog, never built.
- 2026-07-29 — PR #1 is still unmerged; `main` has none of the split.
- 2026-07-29 — the two gitignored "durable copies" (one here, one in kglite)
  are collapsed into this tracked file, and the dormant-consumer status is
  recorded for the first time.
