---
name: phased-plan
description: Run a large feature or refactor as a gated, phased project. Starts with an investigation phase (read-only Explore agents map scale and impacted paths) — NOT standard plan mode — then builds a custom gated phased plan, creates a branch + draft PR against main for CI tracking, and executes each phase autonomously (code → test → lint → commit → push) until done. Ships only via the release skill.
---

# Phased plan

For any large feature or non-trivial refactor. **Demand this skill** when the
user kicks off such work. Do **not** use standard plan mode
(`EnterPlanMode` / `ExitPlanMode`) — this skill builds its own gated phased
plan instead of the harness's generic plan.

## Working dir: `dev-docs/` (gitignored)
All plans, scratch, and intermediates live under **`dev-docs/`** — gitignored
local working state. **The canonical layout + lifecycle is `dev-docs/README.md`
— read it; it's the source of truth, this is just the phased-plan-relevant
subset:**
- This project's plan → **`dev-docs/plans/<slug>.md`** (durable).
- Design choices/trade-offs you weigh → **`dev-docs/designs/`** (durable).
- Open threads → a lean one-line backlink in **`dev-docs/todos.md`** (detail in
  the linked durable doc, never inline).
- **Offload large output to `dev-docs/temp/` and report the path** (>1-day
  purge) instead of printing it — stays under the response token gate.
- Benchmarks: harnesses → **`bench/scripts/`**, regression rows →
  **`bench/results/results.csv`**, heavy generated dumps → **`bench/out/`**
  (>14-day purge; never write artifacts next to the script).

## Phase −2 — Doctrine sync (first action, always)
Before anything else, read `../doctrine/VERSION` and compare it to
`dev-docs/.doctrine-synced` (one line, a `YYYY.MM.DD` date serial). **If they
match, stop here** — that is one file read, which is why this step is never
worth skipping. If doctrine is ahead, read `../doctrine/CHANGELOG.md` forward
from the marker and act on every entry newer than it, by class:

- `[skills-update]` — merge into the **declared authority** (`AGENTS.md` /
  `.agents/skills/`, per the declaration at the top of `AGENTS.md`), then
  regenerate the `.claude/` adapters from it. Never hand-port into an adapter.
  Adjudicate any existing divergence per R14 as *local improvement* or
  *staleness* before touching either side; blind sync deletes improvements.
- `[local-sweep]` — the entry states a concrete check command. Run it. A failure
  becomes scoped, visible work in the current plan, never a silent side-task.
- `[info]` — no action.

**Write the new version into `dev-docs/.doctrine-synced` only after those
actions have completed.** Marking first permanently hides the entry it skipped:
the next sync compares against the marker and sees nothing.

## Phase −1 — Start fresh (recommend cleanup first)
Before investigating, **recommend the user run the `dev-docs-cleanup` skill**
so we start from a tidy `dev-docs/` and a current `todos.md`. Relevant
carried-over todos can then be folded into this plan — **only with the user's
go-ahead.** If they decline, proceed without it.

## Phase 0 — Investigation (get a feel for scale before committing to a plan)
- **Do not enter plan mode.** Investigate first, plan second.
- **Read-only until approval.** The main loop makes **zero edits** during
  Phase 0 and Phase 1 — no branch, no PR, no code, no file writes. All
  investigation goes through **read-only `Explore` agents**; nothing touches
  the working tree until the user approves the plan in Phase 1.
- Kick off **investigator agents** (`Explore`). Fan them out in parallel — one
  per subsystem / suspected blast-radius area. distillPDF's blast-radius map:
  the pure-Rust core modules (`src/extract.rs`, `src/layout.rs`, `src/html.rs`,
  `src/markdown.rs`, `src/text.rs`, `src/headings.rs`, `src/vector.rs`,
  `src/img.rs`, …), the document model (`src/model/`), the OCR subsystem
  (`src/ocr/`), the **PyO3 layer** (`#[pymethods]` in `src/lib.rs`), and the
  Python wrapper (`python/distillpdf/`). **Scale the count to blast radius:**
  1–2 for a medium change, more only for a genuinely large one; don't over-spend
  on investigation. Have them report: structure of the affected area, impacted
  paths / callers, hidden couplings, existing test coverage (`tests/test_*.py`
  + Rust `#[cfg(test)]` units), and a rough size estimate.
- If this is a bug-driven refactor: reproduce and confirm the **root cause with
  evidence** before planning the fix (AGENTS.md "Working style"). For an
  extraction/layout bug, capture the failing PDF as a **minimal generated
  fixture** — extend `tests/gen_fixtures.py` (or `gen_tables.py`/`gen_demo.py`)
  so the input is self-generated and committable, never a third-party PDF.
- **For a behaviour-preserving refactor, probe current behaviour first.** Write
  a throwaway scratch script (or a temporary test) that exercises the code paths
  you're about to move and capture their *actual* outputs (extracted HTML /
  Markdown / block model) — don't trust your mental model. Catching latent bugs
  before planning beats discovering them mid-execution.
- **Confirm your intended safety net actually catches *this class* of change.**
  Decide the net in Phase 0: which `cargo test --lib` cases / `pytest` modules /
  generated fixtures prove the behaviour, and whether they actually exercise the
  path you're changing (the PyO3 layer and the pure-Rust core need *separate*
  coverage — a pytest-only run misses Rust-core unit regressions and vice-versa).
- Synthesize their findings into a scale read: small/medium/large, risk hot
  spots, what could invalidate a naive plan.

## Phase 1 — Build the gated phased plan
- Write the plan to **`dev-docs/plans/<slug>.md`** (the durable copy; the PR
  description in Phase 2 mirrors it as a checklist).
- Break the work into numbered phases. Each phase must be independently
  **buildable, testable, committable** (bisectable).
- For each phase spell out: the change, the tests that prove it, the green gate.
- No phase touches `Cargo.toml`/`pyproject.toml` version or publish config —
  shipping is the `release` skill's job.
- Present the plan, then **invite revision: ask the user to revise or approve,
  and loop on their feedback until they approve.**
- **Hard stop — wait for an explicit go-ahead.** Do not create the branch, open
  the PR, or write any code until the user says proceed (e.g. "proceed", "go
  ahead", "approved", "ship it"). A simple proceed is enough — no formal
  sign-off. Until then, stay read-only.
- Once approved, **do not pause between phases.**

## Phase 2 — Branch + draft PR (the CI tracking handle)
- Create a feature branch: `feat/<slug>` or `refactor/<slug>` (never work the
  project directly on `main`).
- Push the branch and **open a draft PR against `main`**. This is what makes
  CI run on the branch: `ci.yml` triggers on `pull_request`, so every push to
  the branch now runs the full CI suite (Rust units + the bundled-Tesseract
  wheel build + pytest) — while **nothing publishes** (`publish.yml` triggers
  only on push to `main` and is version-gated, so it never fires for a branch PR).
- Put the phased plan into the **PR description as a checklist** (one box per
  phase). The PR tab then shows plan + progress + CI status in one place.

## Phase 3 — Execute each phase (the autonomous loop)
For every phase, in order:
1. Implement the phase's code + its tests.
2. **Local green gate before committing** (mirror CI):
   - `cargo test --lib` (the pure-Rust core — `extension-module` is opt-in, so
     the test binary links libpython and the pure-logic unit tests run; the
     headline gate).
   - `cargo clippy --lib` (lint hygiene — surfaced, not CI-blocking, but keep
     it clean).
   - Build + install the wheel so the PyO3 layer + Python suite run against the
     real extension:
     `maturin build --release --features extension-module,tesseract --out dist`
     then `pip install --force-reinstall --no-deps dist/*.whl` (confirm maturin
     prints a successful build — don't read status through a `tail`/`head`
     pipe). For a fast inner loop, `maturin develop --features
     extension-module,tesseract` is fine; the release-shaped build is what CI
     runs.
   - `pytest tests/ -q`.
   Run **both** `cargo test --lib` and `pytest` after any Rust behaviour change
   — a pytest-only subset skips the pure-Rust-core assertions and vice-versa.
3. **Commit** the phase (`feat(...)` / `refactor(...)` / `fix(...)`), one
   commit per phase. (distillPDF keeps no CHANGELOG file — the commit message is
   the user-visible record.)
4. **Push to the branch** → CI runs on the PR for that phase. Tick the phase's
   checkbox in the PR description.
5. **Retire any `todos.md` action this phase completed.** If the phase shipped
   work that fully closes a backlog thread, do the same soft-delete tidy
   `dev-docs-cleanup` performs — at phase-commit time, not as a separate pass:
   - **Fully done** → remove the backlink line from `todos.md` and move its
     supporting `plans/<doc>.md` to `dev-docs/bin/` (7-day grace, `mv`).
   - **Partially done** → leave the doc; trim the entry to only what's left.
   - **Shared doc** (a `plans/` file backing several todos, e.g.
     `consider-for-future.md`) → remove only the closed entry; move the doc to
     `bin/` *only* once no live backlink points at it.
   `dev-docs/` is gitignored, so this is local bookkeeping alongside the commit,
   not part of the git change. Note each retirement in the report-out.
6. Continue into the next phase. If a phase's CI comes back red, fold the fix
   into the loop before the project merges — don't leave the PR red.

Stop mid-plan only for a genuine blocker (unfixable test, architectural
surprise invalidating a later phase). Surface it; don't push through.

**Bugs that surface mid-plan — fix them as they surface, don't step over them.**
When executing a phase reveals a defect:
- **In scope** (same file/subsystem you're touching): reproduce + confirm the
  root cause, then fix it as its **own bisectable phase** — insert a `Phase Nb`
  with its own test/fixture + commit. Don't fold a behaviour change into a
  mechanical-refactor commit — keep bisection clean.
- **Out of scope** (different subsystem): don't silently leave it. Reproduce,
  confirm, file it to `dev-docs/plans/consider-for-future.md` with a `todos.md`
  backlink, and add a cheap generated regression fixture if one fits.
Either way, record it in the **report-out** below — a discovered bug never
vanishes.

## Phase 4 — Fidelity / perf gate (only for extraction- or perf-sensitive work)
Before declaring done on extraction-fidelity or throughput work, run the
**corpus regression gate** — `scripts/release-check.sh` (it runs `cargo test
--lib`, the wheel build, `pytest tests/`, and the local 37-metric corpus gate
over the real-world PDF set). Extraction changes must not regress a frozen
metric. Record any new throughput numbers to `bench/results/results.csv`. Fix
regressions now, not in a follow-up.

## Report out (when the plan completes, before Ship)
Surface a concise summary so nothing actioned silently — keep it under the
400-token rule and link the plan doc for detail:
- **Phases** done (one line each) + the PR link / final commit shas.
- **Bugs surfaced** during execution and each one's disposition: *fixed in
  Phase Nb* (name the fix) or *filed to backlog* (`consider-for-future.md` +
  `todos.md`). This list is mandatory even if empty ("no bugs surfaced").
- **Fidelity/perf gate** result if run (corpus gate verdict: all metrics held /
  regressions; pre/post throughput if measured).
- **`todos.md` changes**: actions *retired* (entry removed + doc → `bin/`,
  per phase) and carried-over / out-of-scope items *added*.
- **Plan deviations** (inserted phases, re-scopes) and why.

## Phase 5 — Ship (only on request)
When the user asks to ship, run the **`release`** skill. It ff-merges the
branch into `main`, bumps the version in **both** `Cargo.toml` and
`pyproject.toml` (they must stay in sync), commits, and — with explicit approval
— pushes `main` (triggering the auto-version-gated PyPI publish) and verifies
PyPI. This skill never bumps, never pushes `main`.

## Notes
- Keep responses under 400 tokens; write long diffs/logs to a file, report the path.
- Branch pushes during the loop are routine (no publish). Only the **`main`**
  push at `release` time is the approval-gated one.
- Changed a `#[pymethods]` fn in `src/lib.rs`? Add/adjust the matching `pytest`
  coverage **and** the Rust-side unit test — the binding and the core are tested
  separately.
