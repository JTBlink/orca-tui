# R3 — Extract the daemon connection/stream adapter

**Priority:** P1  
**Blocks:** R8  
**Blocked by:** R1

## Goal

Give daemon protocol and stream-thread lifecycle one deep module, leaving `App`
to consume domain updates and connection state.

## Design

Create a private module such as `src/daemon_connection.rs` containing the
adapter that owns:

- `DaemonClient`
- session-ID → stable pane-ID map
- stream reader thread startup and shutdown signaling
- initial connect and reconnect setup
- common frame parsing/routing

The adapter should expose a small interface, for example `connect`,
`register_session`, and `connection_state`/events. Keep RPC payload details out
of input and rendering code.

## Acceptance criteria

- `try_connect_daemon` and `pump_daemon_reconnect` no longer duplicate stream
  reader thread setup.
- Stream reader emits only `AgentUpdate` values or a typed connection event.
- Stable pane IDs remain correct after close and dynamic spawn.
- Existing daemon protocol tests plus one reconnect-path test pass.
- No daemon socket/token values are written to logs or tests.
