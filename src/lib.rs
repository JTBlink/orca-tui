//! # orca-tui (library)
//!
//! Terminal multi-agent coding orchestrator — a TUI port of
//! [Orca GUI](https://github.com/stablyai/orca). Runs N coding agents
//! (Claude Code, Codex, OpenCode, …) side-by-side in split terminal panes,
//! each in its own PTY / git worktree, monitored from one screen.
//!
//! The `orca-tui` **binary** is a thin wrapper around [`cli::run`]; every piece
//! of logic lives in this library crate so it can be unit-tested, benchmarked
//! (`benches/`), and reused as a dependency. Built on
//! [`ratatui-ppalla`](https://crates.io/crates/ratatui-ppalla).

pub mod activity;
pub mod agent;
pub mod app;
pub mod bus;
pub mod cli;
pub mod clipboard;
pub mod config;
pub mod coordinator;
pub mod crashlog;
pub(crate) mod daemon_connection;
pub mod daemon_server;
pub(crate) mod debug_log;
pub mod hangul;
pub(crate) mod input;
pub mod integrations;
pub mod layout;
pub mod mobile;
pub mod orca_daemon;
pub mod orca_workspaces;
pub mod osc;
pub(crate) mod overlay;
pub mod pane;
pub(crate) mod pane_slot;
pub mod perf_probe;
pub mod pty_session;
pub mod query;
pub(crate) mod render_model;
pub mod scheduler;
pub mod sidebar;
pub mod ssh;
pub mod sync;
pub mod terminal_emu;
pub mod toast;
pub(crate) mod workspace_view;
pub mod worktree;
