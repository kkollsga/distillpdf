# Deviations from the agent baseline

Local improvements over doctrine's `reference/AGENTS.baseline.md`, in `AGENTS.md` and the skills.
Adaptations (this repo's own commands, paths and thresholds) are not listed. Read at sync time.

## Pushes need the user's word on every branch

- **Where:** `AGENTS.md` → "Releases: the user decides when to bump and push"
- **Baseline says:** branch pushes need no approval; a push to `main` needs the user's go-ahead in the same turn.
- **Local rule:** no `git push` of any branch, and no version bump, unless the user asks in that turn.
- **Why it adds value here:** pushing `main` publishes to PyPI through the version-gated workflow, and a release is irreversible once downstream installs it, so the repo keeps one rule an agent cannot get wrong by choosing the wrong branch.
- **Upstream:** not proposed
- **Last reviewed:** doctrine 0.2.0

## Failing PDFs become self-generated fixtures

- **Where:** `AGENTS.md` → "Code health" (Root-cause before fixing)
- **Baseline says:** fix a bug and scan for its class; it names no fixture policy.
- **Local rule:** reproduce an extraction or layout defect as a self-generated, committable fixture, never a third-party PDF.
- **Why it adds value here:** the third-party corpus is gitignored and never published, so a regression test built on it could not run in CI.
- **Upstream:** not proposed
- **Last reviewed:** doctrine 0.2.0

## Bugs are fixed, only missing capabilities are filed

- **Where:** `AGENTS.md` → "Code health" (No bugs left behind)
- **Baseline says:** fix a pre-existing bug you meet, or surface it explicitly.
- **Local rule:** classify first: a bug is fixed, never filed; only a missing capability goes to `consider-for-future.md`.
- **Why it adds value here:** a known defect can't be parked as future work, and `consider-for-future.md` stays a list of ideas an agent can skip without missing a bug.
- **Upstream:** candidate
- **Last reviewed:** doctrine 0.2.0

## Recipe hygiene

- **Where:** `AGENTS.md` → "Testing and verification" (Recipe hygiene in anything another reader will run)
- **Baseline says:** a verification must be able to fail (R1) and a reported status is not the result (R2); it does not address recipe wording.
- **Local rule:** name the interpreter, never verify with `>/dev/null 2>&1; echo $?`, and a failure message's pointers must resolve for every reader.
- **Why it adds value here:** a shell alias's failure reads as the checker's verdict, and a gate whose failure text cites a gitignored path fails exactly when it is read.
- **Upstream:** candidate
- **Last reviewed:** doctrine 0.2.0

## Comments stay on the item they document

- **Where:** `AGENTS.md` → "Code health" (A comment is a claim)
- **Baseline says:** a change that falsifies a nearby comment corrects it in the same change.
- **Local rule:** also confirm a moved `///` block still renders on its own item, since it attaches only by adjacency.
- **Why it adds value here:** the Python API's `help()` is generated from these blocks, so a misattached block is a wrong public document.
- **Upstream:** not proposed
- **Last reviewed:** doctrine 0.2.0
