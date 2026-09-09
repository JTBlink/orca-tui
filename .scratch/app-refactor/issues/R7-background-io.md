# R7 — Move blocking external I/O off the UI loop

**Priority:** P2  
**Blocks:** R8  
**Blocked by:** R3, R4, R5, R6

## Goal

Prevent slow GitHub, daemon connect, and RPC operations from freezing input and
rendering.

## Design

Use a worker thread and typed app events/results. The UI loop should show a
loading/error state and apply results on the next tick. Keep the existing
`AgentBus` or introduce a separate typed channel; do not add async throughout
the rendering path merely to run one command.

## Acceptance criteria

- Tasks repo fetch does not block keyboard/render polling.
- Daemon connect/reconnect and session creation have bounded behavior visible
  as connection/error state.
- Cancellation or stale-result handling prevents a late fetch from replacing a
  newer Tasks view.
- Tests cover success, failure, and a result arriving after the view closes.
