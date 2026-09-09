# R0 — Capture invariants before refactoring

**Priority:** P0  
**Blocks:** R1  
**Blocked by:** none

## Goal

Record the behavior that the structural changes must preserve, without
changing production code.

## Scope

- Read the existing `app.rs` tests and technical design.
- Add or consolidate a small invariant helper for pane collection behavior if
  needed (stable pane IDs, focus clamping, session cleanup).
- Record the verification commands in the ticket/PR. If `cargo` is unavailable,
  state that explicitly and do not claim tests passed.

## Acceptance criteria

- Stable pane IDs remain distinct after close + spawn.
- Closing a focused pane clamps focus and clears zoom when the collection is
  empty.
- Output/exit updates for a closed or unknown pane are no-ops.
- A focused, unzoomed, Normal app exits only when all sessions and tasks are
  done.
- At least one test names each invariant, or existing tests are explicitly
  linked to it.

## Likely files

- `src/app.rs` tests only, unless a tiny test helper is required.
