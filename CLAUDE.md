# distillPDF — working agreements

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
