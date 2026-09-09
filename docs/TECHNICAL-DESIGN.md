# Orca TUI Technical Design

## Overview

orca-tui is a **standalone terminal UI client** for Orca, the AI coding agent orchestrator. It connects to a running Orca instance's daemon via Unix socket, providing remote monitoring and full control from any SSH-capable device (phone, laptop, server).

**Architecture principle: fully decoupled.** orca-tui is an independent project. Orca's codebase is not modified. The Orca daemon's NDJSON-over-Unix-socket protocol is the stable contract between the two.

## Architecture

```
┌─────────────────────────┐     ┌─────────────────────────┐
│   Orca (unmodified)      │     │   orca-tui (this repo)   │
│                         │     │                         │
│  ┌─────────────────┐   │     │  ┌───────────────────┐  │
│  │ Daemon           │◄──────────│ DaemonClient       │  │
│  │ (Unix socket)    │   │     │  │ (orca_daemon.rs)   │  │
│  │ NDJSON + token   │   │     │  └───────────────────┘  │
│  └─────────────────┘   │     │           │              │
│           │             │     │           ▼              │
│  ┌─────────────────┐   │     │  ┌───────────────────┐  │
│  │ PTY Sessions     │   │     │  │ Ratatui Renderer   │  │
│  │ (node-pty)       │   │     │  │ + vt100 emulator   │  │
│  └─────────────────┘   │     │  └───────────────────┘  │
│                         │     │           │              │
│  ┌─────────────────┐   │     │           ▼              │
│  │ orcad            │   │     │  ┌───────────────────┐  │
│  │ (WebSocket RPC)  │   │     │  │ Terminal output     │  │
│  └─────────────────┘   │     │  └───────────────────┘  │
└─────────────────────────┘     └─────────────────────────┘
```

## Connection Paths

| Path | Protocol | Use Case |
|------|----------|----------|
| Daemon socket | NDJSON + binary frames over Unix socket | Same-machine SSH access, PTY session sharing |
| orcad WebSocket | JSON-RPC over WebSocket | Cross-network access (future) |

## Daemon Protocol Specification

### Transport

- **Unix domain socket** at a platform-specific path (see Discovery below)
- **Two-socket model**: `control` (NDJSON RPC) + `stream` (binary frames for PTY output)
- Both sockets authenticate via the same hello handshake

### Protocol Version

Current Orca daemon version: **36** (as of 2026-09, see `src/main/daemon/daemon-protocol-version.ts`)

Version history of notable features:
- v36: content-addressed shell wrapper trees
- v35: async CWD validation
- v32: snapshot serializer fidelity
- v30: history seed transfer
- v29: mode 2031 unsubscribe fact
- v27: completion process inspection
- v25: PTY startup ingress
- v24: clean disconnect
- v18: getSize
- v11: getForegroundProcess

### Hello Handshake

Client sends on each socket:
```json
{"type":"hello","version":36,"token":"<uuid>","clientId":"<uuid>","role":"control"}
```
```json
{"type":"hello","version":36,"token":"<uuid>","clientId":"<uuid>","role":"stream"}
```

Server responds:
```json
{"type":"hello","ok":true,"daemon_identity":{"pid":1234,"startedAtMs":1234567.0,"launchNonce":"abc"}}
```

On rejection:
```json
{"type":"hello","ok":false,"error":"reason","retryable":true}
```

### RPC Request/Response Format

Request (NDJSON on control socket):
```json
{"id":"rpc-1","type":"methodName","payload":{...}}
```

Response:
```json
{"id":"rpc-1","ok":true,"payload":{...}}
```

Error:
```json
{"id":"rpc-1","ok":false,"error":"message"}
```

### RPC Methods

| Method | Payload | Response | Description |
|--------|---------|----------|-------------|
| `ping` | `{}` | `{pong:true}` | Health check |
| `listSessions` | `{}` | `{sessions:[...]}` | List active PTY sessions |
| `createOrAttach` | `{sessionId,cols,rows,cwd?,env?,command?,...}` | session info | Create or attach to a PTY session |
| `cancelCreateOrAttach` | `{sessionId,requestId?}` | `{canceled:bool}` | Cancel pending spawn |
| `write` | `{sessionId,data}` | `{}` | Write input to PTY |
| `resize` | `{sessionId,cols,rows}` | `{}` | Resize PTY |
| `kill` | `{sessionId,immediate?}` | `{}` | Kill PTY session |
| `signal` | `{sessionId,signal}` | `{}` | Send signal to PTY |
| `detach` | `{sessionId}` | `{}` | Detach from session |
| `getCwd` | `{sessionId}` | `{cwd:string}` | Get current working directory |
| `getForegroundProcess` | `{sessionId}` | `{foregroundProcess:string}` | Get foreground process name |
| `inspectProcess` | `{sessionId,expectedIncarnationId?,steadyState?}` | process info | Inspect running process |
| `confirmForegroundProcess` | `{sessionId}` | `{foregroundProcess:string}` | Confirm foreground process (async) |
| `confirmShellForeground` | `{sessionId}` | `{confirmed:bool}` | Check if shell is in foreground |
| `clearScrollback` | `{sessionId}` | `{}` | Clear scrollback buffer |
| `getSnapshot` | `{sessionId,scrollbackRows?}` | `{snapshot:...}` | Get terminal snapshot |
| `getSize` | `{sessionId}` | `{size:{cols,rows}}` | Get terminal dimensions |
| `takePendingOutput` | `{sessionId,includeSnapshot?,teardownSnapshot?}` | pending data | Take buffered output |
| `pausePty` | `{sessionId}` | `{}` | Pause PTY output |
| `resumePty` | `{sessionId}` | `{}` | Resume PTY output |
| `setSessionBackground` | `{sessionId,background}` | result | Set background mode |
| `shutdownIfIdle` | `{}` | `{retiring:bool}` | Shutdown if no sessions |
| `shutdown` | `{killSessions}` | `{}` | Shutdown daemon |

### Binary Stream Frame Format

```
[1 byte: type] [4 bytes: big-endian u32 payload length] [payload bytes]
```

| Type | Value | Payload |
|------|-------|---------|
| Data | 1 | Raw PTY output bytes |
| Event | 2 | NDJSON event object |

Max payload: 16 MiB.

### Socket Discovery

Orca stores daemon artifacts in its data directory:
- **macOS**: `~/Library/Application Support/Orca/` (or `~/Library/Application Support/orca/`)
- **Linux**: `$XDG_DATA_HOME/Orca/` or `~/.local/share/Orca/`
- **orcad**: `$ORCA_USER_DATA` or `~/.orca/`

Files:
- `daemon.sock` — Unix domain socket
- `daemon.token` — Auth token (UUID plaintext)

## Technology Stack

| Area | Crate | Version | Purpose |
|------|-------|---------|---------|
| UI rendering | `ratatui` | latest | Terminal widgets and rendering |
| Terminal backend | `crossterm` | 0.28 | Raw mode, input, alternate screen |
| PTY management | `portable-pty` | 0.9 | Standalone agent PTYs |
| Terminal emulation | `vt100` | 0.15 | ANSI parsing for embedded agent output |
| Async runtime | `tokio` | latest | Channels, timers, I/O |
| CLI | `clap` | 4 | Argument parsing |
| Config | `serde` + `toml` | latest | Configuration serialization |
| WebSocket | `tokio-tungstenite` | latest | Mobile companion / future orcad path |

## Key Design Decisions

### Terminal-in-Terminal Rendering

The central challenge: rendering another terminal's output inside orca-tui's own terminal.

Solution:
1. PTY output from daemon → `vt100` crate parses ANSI into styled cells
2. Ratatui renders cells into bounded viewport regions
3. Terminal capability queries from embedded agents are intercepted and responded to
4. Synchronized output (mode 2026) sequences are buffered and emitted atomically

### Input Mode Switching

`Ctrl+Alt+P` toggles between:
- **Passthrough mode**: all keystrokes forwarded to the focused agent
- **Control mode**: navigate panes, manage agents, open overlays

### Frame Scheduling

- Target: 60 FPS with frame skipping
- Idle backoff: reduce rendering when no PTY activity
- Performance: <3ms for 20 concurrent panes

## Protocol Compatibility Analysis (Verified)

### Confirmed Compatible

| Aspect | Orca Daemon | orca-tui (current) | Status |
|--------|------------|-------------------|--------|
| Transport | Unix domain socket | Unix domain socket | OK |
| Framing | NDJSON (`\n`-delimited JSON) | NDJSON | OK |
| Max line size | 16 MiB | 16 MiB | OK |
| Dual-socket model | control + stream | control + stream | OK |
| Binary frame format | `[1B type][4B BE len][payload]` | `[1B type][4B BE len][payload]` | OK |
| Frame types | Data=1, Event=2 | Data=1, Event=2 | OK |
| Frame max payload | 16 MiB | 16 MiB | OK |
| RPC request format | `{id, type, payload}` | `{id, type, payload}` | OK |
| RPC response format | `{id, ok, payload, error}` | `{id, ok, payload, error}` | OK |
| Token auth | UUID in plaintext file, mode 0o600 | UUID from file, trimmed | OK |
| Clean detach on drop | Sends `{type:"detach"}` | Sends `{type:"detach"}` | OK |

### Gaps Requiring Fix

#### 1. Protocol Version: 28 → 36 (CRITICAL)

- **File**: `src/orca_daemon.rs` line 37
- **Current**: `pub const PROTOCOL_VERSION: u32 = 28;`
- **Required**: `pub const PROTOCOL_VERSION: u32 = 36;`
- **Impact**: Orca daemon rejects hello if version doesn't match exactly. Connection will fail.

#### 2. Hello Field Name: `client_id` vs `clientId` (CRITICAL)

- **orca-tui sends**: `{"type":"hello","version":28,"token":"...","client_id":"...","role":"control"}`
- **Orca expects**: `{"type":"hello","version":36,"token":"...","clientId":"...","role":"control"}`
- **File**: `src/orca_daemon.rs` line 105 — `HelloMessage` struct uses `client_id` (snake_case)
- **Fix**: Add `#[serde(rename = "clientId")]` to the `client_id` field

#### 3. Hello Response Field: `daemon_identity` vs `daemonIdentity` (MODERATE)

- **Orca sends**: `{"type":"hello","ok":true,"daemonIdentity":{...}}`
- **orca-tui expects**: `{"type":"hello","ok":true,"daemon_identity":{...}}`
- **File**: `src/orca_daemon.rs` line 143 — `HelloResponse` deserializes `daemon_identity`
- **Fix**: Add `#[serde(alias = "daemonIdentity")]` or rename field with `#[serde(rename = "daemonIdentity")]`

#### 4. Socket Discovery Path (MODERATE)

- **orca-tui searches**: `~/.config/orca/daemon.sock`, `~/.local/share/orca/daemon.sock`
- **Orca actual path**: `<userData>/daemon/daemon-v36.sock` (versioned filename!)
  - macOS: `~/Library/Application Support/Orca/daemon/daemon-v36.sock`
  - Linux: `~/.config/Orca/daemon/daemon-v36.sock` (note: capitalized "Orca")
- **File**: `src/orca_daemon.rs` lines 325-341 — `DaemonEndpoint::discover()`
- **Fix**: Update path candidates to match Orca's actual layout, include versioned filename pattern `daemon-v*.sock`

#### 5. Stream Events: NDJSON vs Binary (LOW)

- **Orca daemon** sends stream events as **binary frames** (type=2) containing NDJSON
- **orca-tui** correctly reads binary frames via `read_frame()` and `FrameType::Event`
- Status: Compatible. No fix needed.

#### 6. Fire-and-Forget Notifications (LOW)

- **Orca daemon** skips response when `id` starts with `notify_` prefix
- **orca-tui** always reads a response after `rpc()` call
- **Impact**: If orca-tui sends `write`/`resize` as regular RPC (with `rpc-N` id), it works fine — just slower than using notify prefix
- **Optimization**: Add `notify()` method using `notify_` prefix for write/resize/pausePty/resumePty

#### 7. Missing High-Level RPC Methods (LOW)

orca-tui only wraps `ping` and `listSessions`. These additional methods are available via generic `rpc()` but would benefit from typed wrappers:
- `createOrAttach` — spawn or attach to a PTY session
- `getSnapshot` — get terminal state with optional scrollback
- `getSize` — get terminal dimensions
- `write` / `resize` / `kill` / `signal` — session control
- `getCwd` / `getForegroundProcess` / `inspectProcess` — process inspection

### Fix Priority

| # | Fix | Priority | Effort |
|---|-----|----------|--------|
| 1 | Protocol version 28→36 | P0 (blocks connection) | 1 line |
| 2 | Hello `client_id`→`clientId` | P0 (blocks connection) | 1 serde attr |
| 3 | Hello response `daemon_identity`→`daemonIdentity` | P1 (breaks identity parse) | 1 serde attr |
| 4 | Socket discovery paths | P1 (can't find daemon) | ~20 lines |
| 5 | Notify prefix optimization | P2 (perf only) | ~15 lines |
| 6 | Typed RPC wrappers | P2 (ergonomics) | ~50 lines |
