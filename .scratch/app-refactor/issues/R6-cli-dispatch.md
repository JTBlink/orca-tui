# R6 — Split CLI parsing from command startup

**Priority:** P2  
**Blocks:** R8  
**Blocked by:** none

## Goal

Keep `cli.rs` focused on clap types and dispatch selection instead of owning
all startup wiring.

## Scope

Extract private functions for `run`, `orchestrate`, daemon/attach, mobile,
issues, and PRs. Preserve argument semantics, especially `::` splitting,
`--remote`, `--worktree`, `--daemon`, and `--mobile`.

## Acceptance criteria

- `try_main` is a short command dispatcher.
- `split_agents` behavior and all CLI parsing tests remain unchanged.
- Startup errors retain actionable context and no credentials/absolute private
  paths are logged.
