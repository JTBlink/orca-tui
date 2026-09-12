//! # orca-tui (library)
//!
//! Terminal multi-agent coding orchestrator — a TUI port of
//! [Orca GUI](https://github.com/stablyai/orca). Runs N coding agents
//! (Claude Code, Codex, OpenCode, …) as terminal tabs. Standalone mode owns
//! local PTYs; Orca GUI mode uses the public `orca terminal` CLI for
//! read-only screen projections and sends input only to terminals explicitly
//! created by this TUI, never opening the GUI daemon's private sockets.
//! The mode keeps one active terminal surface and a Herdr-style workspace
//! sidebar.
//!
//! The `orca-tui` **binary** is a thin wrapper around [`cli::run`]; every piece
//! of logic lives in this library crate so it can be unit-tested, benchmarked
//! (`benches/`), and reused as a dependency. Built on
//! [`ratatui-ppalla`](https://crates.io/crates/ratatui-ppalla).

// Core domain: agent identity, observable activity, and task coordination.
#[path = "core/activity.rs"]
pub mod activity;
#[path = "core/agent.rs"]
pub mod agent;
#[path = "core/coordinator.rs"]
pub mod coordinator;

// Application layer: command dispatch, state transitions, and event flow.
#[path = "app/runtime.rs"]
pub mod app;
#[path = "app/bus.rs"]
pub mod bus;
#[path = "app/cli.rs"]
pub mod cli;
#[path = "app/input.rs"]
pub(crate) mod input;
#[path = "app/pane_slot.rs"]
pub(crate) mod pane_slot;
#[path = "app/scheduler.rs"]
pub mod scheduler;

// TUI presentation: layout, view models, panes, overlays, and feedback.
#[path = "ui/layout.rs"]
pub mod layout;
#[path = "ui/overlay.rs"]
pub(crate) mod overlay;
#[path = "ui/pane.rs"]
pub mod pane;
#[path = "ui/render_model.rs"]
pub(crate) mod render_model;
#[path = "ui/sidebar.rs"]
pub mod sidebar;
#[path = "ui/tab_bar.rs"]
pub(crate) mod tab_bar;
#[path = "ui/toast.rs"]
pub mod toast;
#[path = "ui/workspace_view.rs"]
pub(crate) mod workspace_view;

// Terminal engine: PTY lifecycle, emulation, and byte-stream protocols.
#[path = "terminal/hangul.rs"]
pub mod hangul;
#[path = "terminal/osc.rs"]
pub mod osc;
#[path = "terminal/pty_session.rs"]
pub mod pty_session;
#[path = "terminal/query.rs"]
pub mod query;
#[path = "terminal/sync.rs"]
pub mod sync;
#[path = "terminal/terminal_emu.rs"]
pub mod terminal_emu;

// Orca adapters: daemon protocol and the cross-host workspace catalog.
#[path = "orca/daemon_connection.rs"]
pub(crate) mod daemon_connection;
#[path = "orca/cli_bridge.rs"]
pub(crate) mod orca_cli_bridge;
#[path = "orca/daemon.rs"]
pub mod orca_daemon;
#[path = "orca/workspaces.rs"]
pub mod orca_workspaces;

// External adapters: OS facilities and optional runtime integrations.
#[path = "adapters/clipboard.rs"]
pub mod clipboard;
#[path = "adapters/daemon_server.rs"]
pub mod daemon_server;
#[path = "adapters/integrations.rs"]
pub mod integrations;
#[path = "adapters/mobile.rs"]
pub mod mobile;
#[path = "adapters/ssh.rs"]
pub mod ssh;
#[path = "adapters/worktree.rs"]
pub mod worktree;

// Cross-cutting support kept out of the domain and presentation modules.
#[path = "support/config.rs"]
pub mod config;
#[path = "support/crashlog.rs"]
pub mod crashlog;
#[path = "support/debug_log.rs"]
pub(crate) mod debug_log;
#[path = "support/perf_probe.rs"]
pub mod perf_probe;
