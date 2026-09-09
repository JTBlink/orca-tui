# R2 — Drop unknown daemon sessions instead of routing to pane 0

**Priority:** P0  
**Blocks:** R8  
**Blocked by:** none

## Goal

Make daemon stream routing obey the documented session-ID contract. An event
for a session that is not mapped to a live pane must be ignored.

## Scope

- Change `parse_stream_data` / `parse_stream_event` (or their replacement) to
  return an optional routed update.
- Remove `unwrap_or(0)` fallback for a JSON payload containing a session ID.
- Preserve raw, non-NDJSON compatibility only if there is no session ID to
  resolve; document that compatibility behavior.
- Update tests that currently assert unknown sessions become pane 0.

## Acceptance criteria

- Unknown `sessionId` produces no `AgentUpdate` and cannot mutate pane 0.
- Missing `sessionId` and raw payload behavior is explicitly tested.
- Stream reader handles the dropped update without panicking.
- Technical design documentation matches the implementation.
