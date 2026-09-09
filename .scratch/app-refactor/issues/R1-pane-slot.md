# R1 — Replace parallel pane vectors with `PaneSlot`

**Priority:** P0  
**Blocks:** R3, R4, R5  
**Blocked by:** R0

## Goal

Make pane/session metadata one aggregate so callers cannot accidentally break
the current lockstep invariant.

## Design

Introduce a private `PaneSlot` (or `PaneEntry`) containing the pane and all
per-pane state currently stored in parallel vectors:

- `Pane`
- local `Option<PtySession>`
- command argv
- coordinator task ID
- daemon session ID
- reconnect state and deadline
- pin flag
- last derived status

Keep app-wide state (`focus`, daemon client, scheduler, config, overlays) on
`App`. Use stable pane IDs for routing; vector position is only a transient
index for layout/focus.

## Acceptance criteria

- `App` has one `Vec<PaneSlot>` instead of the per-pane parallel vectors.
- Spawn, close, reconnect, reap, update routing, snapshot, sidebar data, and
  orchestration still behave identically.
- No `assert_parallel_lockstep` helper or lockstep comments remain.
- Existing close/spawn/reconnect tests are updated to inspect `PaneSlot` and
  continue to cover all mutation paths.
- `cargo fmt --check` and focused `cargo test app::tests` pass when Cargo is
  available.

## Notes

Do not extract input or rendering in this ticket. The purpose is a mechanical
state-shape change with behavior-preserving tests.
