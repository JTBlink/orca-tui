# R8 — Documentation, verification, and cleanup

**Priority:** P1  
**Blocks:** none  
**Blocked by:** R1, R2, R3, R4, R5, R6, R7

## Goal

Close the loop after the structural changes.

## Scope

- Update `docs/TECHNICAL-DESIGN.md` module responsibilities and data flow.
- Remove stale Feature/parallel-vector comments and dead helpers.
- Run `cargo fmt --check`, focused tests, full `cargo test`, and clippy.
- Inspect `git diff`/`git status` for recordings, credentials, tokens, or local
  absolute paths.

## Acceptance criteria

- Documentation names the new seams and ownership model.
- Verification commands and results are recorded in the PR/commit message.
- No unrelated behavior or generated artifacts are included.
