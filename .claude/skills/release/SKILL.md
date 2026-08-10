---
name: release
description: Cut a distillpdf release — goal-check against the phased-plan, run the full local gate (scripts/release-check.sh, incl. the corpus regression gate), bump the version in BOTH Cargo.toml and pyproject.toml (kept in lockstep), commit, and (with explicit approval) ff-merge to main + push, which triggers the auto-version-gated PyPI publish; then verify PyPI and tidy dev-docs.
---

# Release

distillpdf publishes to **PyPI only** (no crates.io), from **two version fields
that must stay identical**:

- `Cargo.toml` — the **first `^version` line** in the workspace root (it lives
  under `[workspace.package]`; `publish.yml` greps `-m 1 '^version'`, which is
  why it must stay first). The field the publish workflow reads and gates on.
- `pyproject.toml` line 7 (`version = "x.y.z"`) — the version the wheel is
  built from.

`publish.yml` **errors the build** if the two differ, so **bump both in the same
commit.** Publishing is **automatic and version-gated**: pushing to `main`
triggers `publish.yml`, which reads `Cargo.toml`'s version, checks PyPI's JSON
API (HTTP 404 → not published → build wheels + publish; anything else → skip),
runs the CI gate, then builds the abi3 wheels (Linux/macOS/Windows, bundled
Tesseract) and uploads via **PyPI Trusted Publishing (OIDC — no secret)**. You
never run `maturin publish` by hand — the two-file bump + the `main` push are
the whole trigger.

`Cargo.lock` is **tracked** (committed since PR #6, 2026-08-08) and the
workspace version bump changes it — refresh it (`cargo check -p distillpdf`)
and **stage it with the two version files**. There is no crates.io publish and
no CHANGELOG; the release commit + git history are the record.

## Preconditions
- Check no release is already staged: `git log origin/main..HEAD --oneline | grep -iE "release"`.
  If it returns a release commit not yet pushed, **keep that version** — fold
  work into the same `[x.y.z]` bump (one version bump per push).
- On `main` (or a fold-into-main branch). Working tree ideally clean — but
  if there's **unrelated uncommitted work**, don't block on it and don't sweep
  it in: **stage the two version files explicitly by path**
  (`git add Cargo.toml pyproject.toml`, never `git add -A`/`.`) and leave the
  unrelated changes untouched. Verify with `git status --porcelain` that only
  those two files are staged.

## Steps
1. **Goal check — did we achieve what we set out to do?** If this release ships
   a `phased-plan` project, read its plan (`dev-docs/plans/<slug>.md`) and the
   PR checklist, and confirm every planned phase actually shipped. List any
   phase that was **dropped, deferred, or only partially done**, and surface
   the gaps to the user before bumping. Each gap is a conscious choice: finish
   it now, or carry it to `dev-docs/todos.md` — don't let it vanish silently.
2. **Gate — run `scripts/release-check.sh` (THE release gate).** This is the one
   reproducible command; treat a green run here, **not** a green CI badge, as
   the gate. It runs, in order:
   - `cargo test --lib` — pure-logic Rust unit tests.
   - `cargo clippy --lib` — lint (surfaced).
   - `maturin build --release --features extension-module` — the release wheel.
   - install the wheel into the run env + `pytest tests/ -q` — CI parity.
   - **the corpus regression gate** (`benchmarking/corpus_tests/`, 37 metrics
     over 25+ real-world PDFs, each held at-or-above a frozen baseline) — the
     real fidelity signal. The script **exits non-zero** if the corpus is absent
     (it's license-encumbered and local-only): a release must not proceed
     without a green gate run.

   If the machine's Python differs from the script defaults, pass the overrides
   it documents (`BUILD_PY`, `RUN_PY`, `UV`). Fix any failure before bumping —
   never bump over a red gate or a skipped corpus gate.
3. **Bump version — patch by default** (`x.y.Z` → `x.y.Z+1`), editing **both**
   `Cargo.toml` line 3 **and** `pyproject.toml` line 7 to the *same* value (the
   publish workflow rejects a mismatch). If the changes warrant a
   **minor/major** bump (new feature, breaking change), STOP and ask one quick
   clarification question first; otherwise proceed with the patch bump.
4. **Commit** as the final step: `release(x.y.z): ...` (both version fields in
   one commit). Stage by path (`git add Cargo.toml pyproject.toml Cargo.lock`),
   never `-A`.
5. **Push — invoking `/release` is the authorization.** Running this skill
   authorizes the `main` push it produces (the publish-triggering one) — no
   separate in-the-moment "push" prompt. Authorization is scoped to this one
   release run (the `release(x.y.z)` push + its CI fix-and-push loop) and lapses
   once published or the user pivots. All pre-push safeguards still apply: gate
   green, surgical staging, ff-merge clean.
   - **ff mechanic — push the branch HEAD straight to `main`, don't
     `checkout main`.** When unrelated WIP sits in the working tree, a local
     `git checkout main` drags it across and risks conflicts. Instead: confirm
     fast-forward (`git merge-base --is-ancestor origin/main HEAD`), then
     `git push origin HEAD:<branch>` (update the PR, if any) and
     `git push origin HEAD:main` (ff `main` → triggers publish). The working
     tree never moves.
6. **Poll CI + publish until green.** Poll the GitHub **Actions API** directly
   (`gh run list`, `gh run view`). On the `main` push, **`publish.yml`** runs:
   a `check` job (version vs PyPI + Cargo/pyproject sync), then — only if the
   version is new — the CI gate, the per-OS wheel builds, and the PyPI publish.
   `ci.yml` also runs on the push. The publish jobs self-skip if the version
   already exists on PyPI, which is normal on a no-op re-push.
   - CI fix-and-push loop: if a push fails on a shipped-code/infra bug (not a
     scope change), push `fix(...)`/`ci(...)` without re-asking until green.
     Stop after ~3 iterations or any release-shape change.
7. **Verify published**: PyPI shows `distillpdf` at `x.y.z`:
   `curl -s https://pypi.org/pypi/distillpdf/json | jq -r .info.version`.
   (There is no crates.io publish to verify.)
8. **Delete the released branch** if this shipped from a feature branch (the
   `/release` invocation authorizes it; no prompt). Once publish is verified the
   branch is fully ff-merged into `main`. Delete local + remote without
   disturbing WIP: `git branch -f main origin/main`, `git switch main`
   (zero-diff when `main == HEAD`, so WIP is preserved), `git branch -d
   <branch>` (refuses if unmerged — don't `-D` past that), `git push origin
   --delete <branch>`. Confirm the PR shows `MERGED`. **Never** delete
   `gh-pages` or open `dependabot/*` branches.
9. **Tidy dev-docs — perform directly, no prompt** (the `/release` invocation
   is the authorization). Follow the **`dev-docs-cleanup`** logic, which is
   todos.md-driven: auto-purge the time-boxed dirs, then read **only
   `todos.md`** — archive the now-shipped plan to `dev-docs/bin/` and prune its
   `todos.md` entry, move any other completed/stale entries' docs to `bin/`,
   and trim the entries (read a backlinked doc only to confirm it shipped).
   Carry the step-1 gaps into `todos.md`. Don't read `designs/` or sweep
   through `plans/`.

## Notes
- Keep responses under 400 tokens; write long diffs/logs to a file and report the path.
- Version source of truth: the first `^version` line in the root `Cargo.toml`
  — but `pyproject.toml` line 7 must match it exactly, or `publish.yml` fails
  the build. Two fields, one number; bump them together.
- Publish is push-triggered and idempotent (version-gated): a re-push at the
  same version is a safe no-op, not a double-publish.
- **Never build the wheel with `--all-features`** — `extension-module` can't
  link a standalone binary. The wheel build is `--features
  extension-module,tesseract` (or `extension-module` alone for a lighter local
  wheel, as `release-check.sh` does); `cargo test --lib` runs with no features.
