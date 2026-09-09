# R4 — Extract input state and interaction handlers

**Priority:** P1  
**Blocks:** R8  
**Blocked by:** R1

## Goal

Separate keyboard/mouse policy from process/session lifecycle.

## Design

Move `InputMode`, focus direction, jump/spawn/tasks/settings state, and the
keyboard/mouse handlers into a private `input` module or `UiState` aggregate.
Handlers may return semantic commands (focus pane, close pane, spawn command,
send bytes) that `App` executes. Avoid exposing `App` internals wholesale.

## Acceptance criteria

- `app.rs` no longer contains the large `handle_key`/modal state block.
- Normal/Panes/Jump/Spawn/Tasks/Settings/Activity/Dashboard behavior is
  unchanged and covered by existing tests.
- Mouse selection, scroll routing, zoom focus, and focus clamping retain their
  current behavior.
- No handler performs blocking GitHub or daemon network I/O directly after this
  ticket unless the operation is explicitly left for R7.
