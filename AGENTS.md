# distillPDF — working agreements

> **Authority declaration (R7).** `AGENTS.md` and `.agents/skills/` are the
> **authority** for this repo's conventions and skills. `CLAUDE.md` and
> `.claude/skills/` are **generated adapters**: copies with `AGENTS.md`
> substituted to `CLAUDE.md`. Edit the authority and regenerate; never hand-edit
> an adapter. **This declaration block is exempt from that substitution** — it
> names the same authority in both copies, and substituting it would invert its
> meaning. Divergences are adjudicated per R14 (improvement vs staleness) before
> either side is touched, never blind-synced.

## Releases: the user decides when to bump and push

Do NOT bump the version (`Cargo.toml` / `pyproject.toml`) or push on your own.
Pushing `main` triggers the publish workflow, which releases to PyPI — that is the
user's call, not yours.

- Implement, build, and test changes locally, then **stop and report**. Leave the
  version number untouched.
- Only bump the version and/or `git push` when the user explicitly asks for it in that
  turn (e.g. "bump and push", "ship it", "release this").
- When work is ready to release, you may *remind* the user it's ready and ask whether to
  bump + push — but wait for their go-ahead.
- Committing locally without pushing is fine when the user asks to commit; never push as
  part of "commit".

## Working style

- **Root-cause before fixing.** For any bug, reproduce it and confirm the root
  cause *with evidence* before changing code — for an extraction/layout defect,
  capture the failing PDF as a **self-generated, committable** fixture (extend
  `tests/gen_fixtures.py` / `gen_tables.py` / `gen_demo.py`), never a
  third-party PDF.
- **No bugs left behind.** A defect you surface while doing something else never
  vanishes: fix it in scope, or file it to `dev-docs/plans/consider-for-future.md`
  with a `dev-docs/todos.md` backlink (via the `add-todo` skill).
- **Offload, don't print.** Write long output (diffs, logs, rendered HTML/PDF
  dumps, profiles) to `dev-docs/temp/` (ephemeral, >1-day purge) or
  `dev-docs/bench/out/` (>14-day purge) and **report the path**. Keep responses
  under ~400 tokens. Both are *purged* tiers, so route by value as well as
  weight: **unique evidence** a plan or todo will reference — findings, audits,
  patches, parity/aggregate summaries — goes to `dev-docs/bench/results/`
  (durable) or into the `plans/` doc itself, never to a purged tier.
- **Separate coverage for the two layers.** The pure-Rust core (`cargo test
  --lib`) and the PyO3 layer + Python API (`pytest tests/`) are tested
  independently — after a Rust behaviour change, run **both**. `extension-module`
  is opt-in so `cargo test --lib` links libpython normally.
- **The release gate is `scripts/release-check.sh`**, not the CI badge — it adds
  the local corpus regression gate (37 metrics over real-world PDFs under the
  gitignored `benchmarking/`) that CI can't run. Treat a green run there as the
  gate.

## Code analysis

For structural questions — where is X defined, what calls what, blast radius —
prefer the **code-graph MCP** (`graph_overview` → `cypher_query`) or read-only
**`Explore`** agents over ad-hoc grepping. Reserve `grep`/`read` for literal
text search. `phased-plan`'s Phase 0 fans these out across the affected
subsystems before any plan is written.

## Inbox hygiene

`inbox/` (gitignored) is the cross-project coordination channel — see
`inbox/README.md`. Don't hand-edit it; the **`read-inbox`** skill receives and
the **`notify`** skill sends. `inbox/unread/` must hold **only what still needs
action** — every triaged message is lifted into `dev-docs/` + a `todos.md`
backlink, routed to the party who can act, or logged as a no-action ack, then
moved to `inbox/read/` (auto-purged after 7 days).

## Skills — when to reach for each

The gitignored `dev-docs/` working folder (canonical map: `dev-docs/README.md`)
and `inbox/` are operated by six skills under `.claude/skills/`:

- **`phased-plan`** — *demand this* for any large feature or non-trivial
  refactor: investigate (read-only) → gated phased plan → branch + draft PR
  against `main` → autonomous code/test/commit/push loop → fidelity gate → hand
  to `release`. Do **not** use generic plan mode for such work.
- **`add-todo`** — the single authority on todo-entry shape; capture any
  ask/finding into a scoped `plans/` doc + a lean `todos.md` backlink.
- **`dev-docs-cleanup`** — tidy `dev-docs/` (auto-purge time-boxed dirs +
  todos-driven soft-delete). Run before a new `phased-plan` and at end of
  `release`.
- **`read-inbox`** / **`notify`** — receive / send cross-project mail.
- **`release`** — ship: goal-check, run `scripts/release-check.sh`, bump **both**
  `Cargo.toml` and `pyproject.toml` (kept in lockstep), push `main` (triggers
  the version-gated PyPI publish), verify PyPI, tidy. Only on the user's
  explicit go-ahead (see the release policy above).

`dev-docs/todos.md` is the *local* working thread index; `ROADMAP.md` at the
repo root is the *committed, shared* worklist. Keep the two distinct.
