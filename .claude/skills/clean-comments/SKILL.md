---
name: clean-comments
description: Coordinator-run comment cleanup over a measured scope — the invoking agent measures comment density, briefs one sub-agent per dense file to delete zero-information comments, compress low-density ones and fix false claims (R17), then verifies the whole diff mechanically, never from worker self-reports. Deliberately smaller than a phased plan — no branch ceremony, no plan doc. Run on a subtree after a large program lands, when review keeps hitting stale comments, or on request.
---

# clean-comments

Make the comments in a measured scope **true and lean**: delete what carries no
information, compress the rest to what it carries, fix comments the code
contradicts (R17), and never touch what the tooling reads (R18). The steady
state is R17's same-change duty (CLAUDE.md "Working style"); this skill is for
the residue, and a big residue is the headline of the report, not the line
counts.

## 0. Shape of the run

The invoking agent is the **coordinator**: it measures, briefs, dispatches,
verifies, reports — and **does not edit comments itself**. Sub-agents
(**workers**) do the edits, one file each, because de-duplication needs the
whole-file read and self-reports need an independent checker. One exception: if
measurement returns **≤ 2 files**, skip the workers and apply the brief
yourself — a coordinator with one worker is ceremony.

Invocation authorizes the whole run: it ends in the report or a named blocker,
never in "workers are running".

## 1. Measure first, and be ready to stop

Count comment lines per file over the scope (default: the whole repo; a subtree
argument narrows it):

```bash
rg -c --no-messages '^\s*(//|#)' -t rust -t py <scope> | sort -t: -k2 -rn | head -40
```

Take the **head**: the files that jointly hold ~half the scope's comment lines.
**Stop rule, decided before counting:** if the head is empty or trivially small,
report "already lean — nothing to do" and stop. A cleanup that runs regardless
of what measurement says is a formality with a diff attached. A heavy head right
after a recent cleanup is itself the finding: R17's same-change duty is being
skipped — say so in the report.

distillPDF's density is concentrated in the extraction core (`distillpdf/src/`
— `extract.rs`, `layout.rs`, `html.rs`, `vector.rs`, `text.rs`), the fixture
generators (`tests/gen_*.py`) and the bench/gate scripts. Expect the rate to
split by file character: emitters lose the most, specification-shaped files
(the model/format definitions, the generators that *are* the fixture contract)
correctly lose the least.

## 2. Assemble the worker brief (once, fixed)

**The two tests, per comment paragraph** — *does this add a fact the reader
cannot get from the code or an earlier paragraph?*
- Zero information → **delete**: restates the next line or the signature,
  generic banner, self-referential bookkeeping, dead scaffolding.
- Low density → **compress** to the information carried: repetition across
  paragraphs, throat-clearing, narration of the journey, over-explained
  mechanics, four variations of one example, hedging.

**The floor — never delete:** why-not-what; invariants and safety
preconditions; PDF-spec citations that justify a branch (`§7.3.8`, an operator's
defined behaviour, a producer quirk the code works around); the reason a bail /
fallback path exists in the extraction and layout code; data-format lifecycle
(how an older `.dpdf` model is detected/refused/read); regression rationale in
tests and in the generated fixtures; and a repeated comment that is a *local
contract* (a precondition restated at each call site stays at each call site —
collapsing them parks the protocol in one arbitrary function).

**What reads our comments (R18) — hands off, or handle deliberately.** This
list is the run's precondition: **a missing or unverified reader list stops the
run** rather than being improvised. Verified 2026-08-24 against the tree.

- **Published Python docstrings — the shipped contract.** `#[pymethods]` docs in
  `distillpdf-python/src/` and the pure-Python docstrings in
  `python/distillpdf/*.py` are what a PyPI user reads through `help()` and
  `?`. Falsehood-fixes and attachment repairs only — a deletion strands a public
  item with no doc at all, and nothing in the build says so.
  *Breaks:* the published surface, silently. Verify with the doc-surface check
  in step 5.
- **`argparse` `help=` / `description=` strings in `python/distillpdf/cli.py`** —
  rendered verbatim as `distillpdf --help`. These are **string literals, not
  comments**, so a comment sweep must not touch them at all; named here because
  they read like prose and invite "compression".
  *Breaks:* the CLI's published help text.
- **`docs/` is a hand-maintained mirror, not a generated one.** mkdocs-material
  with **no** mkdocstrings/autodoc (verified: no `:::` directives, no autodoc
  extension), so a docstring edit does **not** propagate to
  `docs/reference/python.md` or `docs/guide/*.md`. The corollary is the
  dangerous half: a docstring corrected here leaves the site's copy false unless
  the same change updates it.
  *Breaks:* the readthedocs site drifts from the API, with no gate to notice.
- **clippy `collapsible_if` / `collapsible_else_if`** — suppressed by a
  comment's mere *presence* inside the block, so deleting the only comment there
  makes the lint fire. In this repo clippy is **surfaced, not blocking**
  (`scripts/release-check.sh` runs `cargo clippy -p distillpdf --lib … || true`;
  CI runs no clippy job), so this is new noise, never a red gate. Don't delete
  the last comment inside such a block just to silence a reviewer.
- **Not a reader, stated so no worker guesses:** the L2 production-boundary gate
  (`distillpdf/src/structure/boundary_audit.rs` + the frozen oracle
  `tests/oracles/l2-boundary-ast.json`) digests `to_token_stream()` output of
  **expressions and signatures**. `//` comments are stripped by the lexer and
  `///` lowers to a `#[doc]` attribute on an *item*, which those nodes do not
  carry — and the gate has its own test pinning this
  (`audit_ignores_comments_strings_and_test_only_items`). Comment edits cannot
  move that oracle.
- **Not a reader:** `scripts/check-fixture-drift.sh` compares committed fixture
  **bytes** against what `tests/gen_*.py` regenerates. Comments in a generator
  do not enter the PDF — unless a generator writes its own source or docstring
  into the document, which none currently does. If a worker touches a generator,
  step 5 re-runs the drift check anyway.
- **No allowance/justification checker, no cbindgen, no `#[no_mangle]`, no
  `.pyi` stubs** in this repo (verified 2026-08-24). If one is ever added, it
  belongs in this list in the same change that adds it.

**De-duplication: within-file only.** Keep the fullest statement at the
most-read location and point the others at it. A fact repeated *across* files is
**flagged to the coordinator**, never collapsed by a worker — that decision needs
cross-file sight.

## 3. Calibrate — one worker, and this gate can fail

Dispatch **one** worker on **one** representative head file. Read its **full
diff** against the brief. If it holds, fan out. If not, fix the brief and
calibrate again on a *different* file; after two failed calibrations, stop and
surface the diffs to the user. This gate has failed for real upstream: an
edit doctrine of "keep the fact, drop the label" passed 231 files while moving
comment volume 0.2 %, and only a human read of the calibration diff caught it.
The unit of value is **information, not fact-count** — an agent tuned on line
count fails hardest on exactly the files whose comments matter most.

## 4. Fan out

One worker per remaining head file, in parallel batches. Each dispatch is the
brief plus the file path plus this contract:

- Read the **entire** file before editing anything.
- **Comment and doc lines only.** Apply the two tests per paragraph; respect the
  floor and the reader list above.
- Fix false comments — claims the adjacent code contradicts, and expired
  predictions ("a later phase will…", "once L3 lands…") that the work landing
  should have retired.
- **Re-attach stranded doc blocks.** A `///` split from its item by a blank line,
  or an item inserted into the middle of a block, silently documents the *next*
  item; the compiler never complains. Check doc fences balance, tracking fence
  **width** (a narrower fence inside a wider one is literal content; a parity
  count calls four unbalanced fences even).
- Return a **structured result**: lines deleted / lines compressed (from → to),
  false comments fixed, cross-file duplicates flagged, code defects noticed (not
  fixed), anything left untouched and why.

A worker that fails is **retried once**, then its file is reported unprocessed.
**Never hand a worker a bulk fixer script** — a regex sweep matches every fence
opener and re-indents well-formed blocks in user-facing docs. Hand-fix or revert.

## 5. Verify mechanically — the diff, never the self-reports

Worker summaries are wrong often enough that they are not evidence. In this
order:

1. **Comment-only diff check.** Rust: every changed line, on **both** sides of
   the diff, is a comment line or blank. Python: parse both revisions
   (`git show HEAD:<file>` vs the worktree), strip docstrings, compare ASTs —
   equal, or the file is **reverted and reported**.
2. **Do NOT run `cargo fmt`.** distillPDF's tree is not rustfmt-normalised
   (`cargo fmt --check` reports pre-existing diffs across the core), so running
   the formatter would rewrite unrelated code and bury the sweep. That makes
   step 1 the **whole** mechanical gate here, and it is therefore strict: any
   non-comment changed line reverts the file, with no formatter-motion
   exemption.
3. **Re-run what the touched surfaces answer to.** `cargo test -p distillpdf
   --lib` (the pure-Rust core) and `pytest tests/ -q` for anything under
   `python/` or `distillpdf-python/` — the two layers are covered separately
   (CLAUDE.md "Separate coverage for the two layers"). If a generator under
   `tests/gen_*.py` was touched, run `scripts/check-fixture-drift.sh`.
4. **Doc-surface check** if any docstring or `#[pymethods]` doc moved: rebuild +
   install the wheel, then assert every public callable still carries a
   non-empty `__doc__` that names *that* item, read from the **installed**
   module (`.venv/bin/python -c "import distillpdf; …"`) — never from the source.
   An item that lost its doc reverts the file that caused it.

## 6. Report

- **Deletion and compression separately, per file — never one percentage.** A
  −2 % on a dense specification file is a success, and the same number on an
  emitter is a worker that did nothing.
- Findings fixed in-run are part of the diff; anything larger — code defects
  workers noticed, cross-file de-dup decisions, a gap in the reader list — goes
  through **`add-todo`** under its entry rules. Anything reported as a finding
  names a **concrete failure**, or it is not reported.
- Offload the long form to `dev-docs/temp/clean-comments-report.md` and give the
  path; keep the inline summary under the 400-token rule.

## Relationship to phased-plan

Not part of one, on purpose. A first-ever whole-tree audit at campaign scale —
hundreds of head files, release integration — wraps this in a `phased-plan`; for
everything else this skill is complete on its own.
