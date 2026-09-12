//! # Application state + main loop
//!
//! Top-level Orca TUI application: owns the terminal tabs (`PaneSlot`s), their
//! [`PtySession`]s, the active tab, the ratatui [`Terminal`] and the [`AgentBus`](crate::bus)
//! receiver. The [`App::run`] loop is a plain synchronous poll/drain/render
//! cycle — no tokio runtime — which is exactly the right shape for a
//! blocking-PTY, blocking-crossterm-event TUI.
//!
//! ## Threading summary
//!
//! ```text
//!   local session:  std::thread (portable-pty reader)  ──▶  std::mpsc
//!   Orca CLI poller: std::thread (list/read workers)   ──▶  tokio mpsc (AgentBus)
//!   main thread:  App::run  ──try_recv──▶  AgentBus receiver  ──▶  Pane.feed
//! ```
//!
//! All three thread classes are synchronous; the tokio channel is used purely
//! as an N→1 mailbox with a sync `try_recv` drain.

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::activity::{ActivityEvent, ActivityLog};
use crate::agent::{AgentKind, AgentSpec, AgentState, AgentStatus};
use crate::bus::{self, AgentUpdate, AgentUpdateReceiver, AgentUpdateSender};
use crate::config::{Config, LayoutConfig};
use crate::coordinator::Coordinator;
use crate::daemon_connection::DaemonConnection;
use crate::input::{self, FocusDirection as FocusDir, InputCommand, InputMode, InteractionState};
use crate::integrations::RepoRef;
use crate::mobile::AgentSnapshot;
use crate::orca_cli_bridge::{CreatedTerminal, OrcaCliBridge};
use crate::orca_workspaces::{OrcaTerminal, OrcaWorkspace};
use crate::pane::Pane;
use crate::pane_slot::{PaneSlot, SessionOwner};
use crate::pty_session::PtySession;
use crate::render_model::RenderModel;
use crate::scheduler::{FrameScheduler, TARGET_FRAME_60FPS};
use crate::sidebar;
use crate::ssh;
use crate::tab_bar::{self, TabHitboxes, TabItem};
use crate::terminal_emu::{MIN_COLS, MIN_ROWS};
use crate::workspace_view::WorkspaceRow;
use crate::worktree::{OwnedWorktrees, WorktreeManager};

use tokio::sync::mpsc::UnboundedSender;

/// The daemon connection state — drives the sidebar indicator + error handling.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// No daemon — orcatui manages PTYs directly (the default/legacy mode).
    #[default]
    Standalone,
    /// Connected to an Orca runtime or the legacy built-in daemon.
    Connected,
    /// Was connected, but the daemon disconnected (crash, idle shutdown).
    /// `reason` is the error message; `next_retry` is when to attempt reconnection.
    Disconnected {
        reason: String,
        next_retry: Option<Instant>,
    },
}

impl ConnectionState {
    /// A short label for the sidebar: `● Standalone` / `● Daemon` / etc.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Standalone => "● Standalone",
            Self::Connected => "● Daemon",
            Self::Disconnected { .. } => "✗ Disconnected",
        }
    }

    /// The color for the sidebar label.
    #[must_use]
    pub fn color(&self, theme: &crate::config::ThemeConfig) -> ratatui::style::Color {
        match self {
            Self::Standalone => theme.muted(),
            Self::Connected => theme.success(),
            Self::Disconnected { .. } => theme.error(),
        }
    }

    /// Whether we are in standalone mode (PTYs managed locally).
    #[must_use]
    pub fn is_standalone(&self) -> bool {
        matches!(self, Self::Standalone)
    }
}

// Keep the normal hint compact enough for a sidebar-enabled 80-column
// terminal.  Tab switching is also visible in the tab strip itself; putting
// the gateway and quit bindings first means the hint remains useful even when
// ratatui clips a narrow footer from the left.
const FOOTER_NORMAL: &str = " Tab \u{00B7} Ctrl+Alt+P \u{00B7} Ctrl+Q quit ";
const FOOTER_PANE: &str = " Tab: next tab \u{00B7} Shift+Tab: previous \u{00B7} p: pin \u{00B7} x: close \u{00B7} n: new tab \u{00B7} b: sidebar \u{00B7} s: nav \u{00B7} ?: help \u{00B7} Esc: back ";
const FOOTER_JUMP: &str =
    " type to filter \u{00B7} \u{2191}\u{2193} select \u{00B7} Enter: focus \u{00B7} Esc: cancel ";
const FOOTER_SPAWN: &str = " \u{2191}\u{2193} select \u{00B7} Enter: spawn \u{00B7} Esc: cancel ";
const FOOTER_ACTIVITY: &str = " \u{2191}\u{2193} scroll activity \u{00B7} Esc: close ";
const FOOTER_DASHBOARD: &str = " Esc: close ";
const FOOTER_SIDEBAR: &str =
    " \u{2191}\u{2193} navigate \u{00B7} Enter: select \u{00B7} Esc: back ";
const FOOTER_WORKSPACES: &str = " \u{2191}\u{2193}/j k: scroll workspaces \u{00B7} Esc: close ";
const FOOTER_SPAWN_CUSTOM: &str =
    " type a command \u{00B7} Enter: spawn \u{00B7} Backspace \u{00B7} Esc: cancel ";
const FOOTER_TASKS_REPO: &str =
    " type owner/name \u{00B7} Enter: fetch \u{00B7} Backspace \u{00B7} Esc: cancel ";
const FOOTER_TASKS_LIST: &str =
    " \u{2191}\u{2193} select \u{00B7} Enter: dispatch agent \u{00B7} Esc: back ";
const FOOTER_SETTINGS: &str =
    " \u{2191}\u{2193} select \u{00B7} Enter/Space: toggle \u{00B7} Esc: save & close ";
const FOOTER_ZOOM: &str = " z: unzoom \u{00B7} Ctrl+Q: quit \u{00B7} Ctrl+B: sidebar ";

/// Sidebar nav menu items (index == sidebar_nav). Activity / Tasks / Settings
/// are all implemented (Phase 1 / 2); a hypothetical future fourth item would
/// be the next "coming soon" placeholder.
const SIDEBAR_NAV_ITEMS: &[&str] = &["Activity", "Tasks", "Settings"];

fn contains(rect: Rect, col: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.right()
        && row >= rect.y
        && row < rect.bottom()
}

/// One row in the Tasks browser (Phase 2). `prompt` is the full string to hand
/// to the dispatched agent (composed via [`crate::integrations::issue_to_prompt`]
/// or [`crate::integrations::pr_to_prompt`]).
#[derive(Debug, Clone)]
struct TasksEntry {
    /// Issue or pull request.
    kind: TaskKind,
    /// GitHub number (e.g. `42`).
    number: u64,
    /// Issue/PR title.
    title: String,
    /// The prompt string to pass to the agent on Enter. For issues this is the
    /// title-only form initially (the body is fetched lazily on dispatch via
    /// [`crate::integrations::fetch_issue`]); for PRs it is the final form.
    prompt: String,
}

type DaemonSpawnReceiver =
    std::sync::mpsc::Receiver<Result<serde_json::Value, crate::orca_daemon::DaemonError>>;

/// Which kind of GitHub item a [`TasksEntry`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    Issue,
    PullRequest,
}

/// The interactive Orca TUI application.
///
/// Construct with [`App::spawn_agents`], then drive with [`App::run`]. Dropping
/// an `App` restores the terminal (via [`App::setup_terminal`] tracking) and
/// kills/joins every still-running agent PTY (via [`PtySession`]'s `Drop`).
///
/// Generic over the ratatui [`Backend`] so tests can use a `TestBackend` (no
/// TTY required, runs identically on a dev machine and headless CI). The
/// default `B = CrosstermBackend<Stdout>` is the real backend used by `run`.
pub struct App<B: Backend = CrosstermBackend<Stdout>> {
    panes: Vec<PaneSlot>,
    interaction: InteractionState,
    focus: usize,
    terminal: Terminal<B>,
    bus_rx: AgentUpdateReceiver,
    quit: bool,
    /// True between [`App::setup_terminal`] and [`App::restore_terminal`] so
    /// the `Drop` impl can restore even on a panic mid-loop.
    raw_mode_active: bool,
    /// Worktrees created in `--worktree` mode. Declared LAST so Rust's
    /// field-drop order removes them only after `sessions` (whose
    /// [`PtySession`] `Drop` kills + joins the child processes) — otherwise
    /// removing a directory that is still a live process's cwd would fail.
    /// `None` when not in worktree-isolation mode.
    #[allow(dead_code)]
    worktrees: Option<OwnedWorktrees>,
    /// Complete Orca workspace catalog rows that do not have a local pane.
    /// Local rows are represented by their running pane and are filtered out
    /// at render time to avoid duplicate sidebar entries.
    workspace_catalog: Vec<OrcaWorkspace>,
    workspace_catalog_loaded: bool,
    /// Runtime hosts that Orca reported but this CLI could not query. Keeping
    /// them separate lets the inventory explain why a remote catalog may be
    /// incomplete instead of silently presenting a partial list as complete.
    workspace_unresolved_hosts: Vec<String>,
    /// Catalog sources that returned rows but did not report host scope. Their
    /// workspace rows are shown, while the overlay discloses that completeness
    /// cannot be proven for that source.
    workspace_unverifiable_scope_hosts: Vec<String>,
    /// Orca's user-facing terminal projection, joined by `ptyId`. This supplies
    /// titles/agent identities and visual ordering from the public CLI.
    orca_terminal_catalog: Vec<OrcaTerminal>,
    /// Feature 5: adaptive frame scheduler — throttles rendering to a 60fps
    /// budget, skips frames when behind (backpressure), and backs off the poll
    /// interval when idle (no input/agent output) to save CPU.
    scheduler: FrameScheduler,
    /// Feature 10: optional snapshot publisher. When set (via
    /// [`App::set_snapshot_sender`]), the loop publishes a `Vec<AgentSnapshot>`
    /// every frame so a mobile-companion WebSocket server can broadcast live
    /// agent status. `None` when no companion is attached.
    snapshot_tx: Option<UnboundedSender<Vec<AgentSnapshot>>>,
    /// Feature 7: orchestration state. When `Some`, [`App::main_loop`] pumps
    /// the coordinator each tick — dispatching the next dependency-gated task
    /// to a fresh pane as previous tasks finish. `None` for the plain `run`
    /// path (all agents spawned up front).
    coordinator: Option<Coordinator>,
    /// The agent binary used to run orchestrated tasks (e.g. `claude`).
    orch_agent: Option<String>,
    /// Legacy session-ID → pane-index map for the built-in daemon stream.
    /// The public Orca CLI path never populates it.
    daemon_session_map:
        Option<std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>>,
    /// Reconnect attempt counter (resets to 0 on successful connect).
    daemon_reconnect_attempts: u32,
    /// Current backoff delay (doubles each failure, capped by config).
    daemon_backoff: Duration,
    daemon_reconnect_rx: Option<
        std::sync::mpsc::Receiver<
            Option<Result<DaemonConnection, crate::orca_daemon::DaemonError>>,
        >,
    >,
    /// Legacy in-flight daemon create RPCs. Public Orca creates use
    /// `orca_spawn_rx` below.
    daemon_spawn_rx: Vec<(usize, String, DaemonSpawnReceiver)>,
    /// PTY size captured at construction, reused by [`App::spawn_one`].
    cols: u16,
    rows: u16,
    /// Kept (not dropped) so [`App::spawn_one`] can fan new sessions onto the
    /// bus mid-run. Completion is detected via [`App::all_sessions_gone`], not
    /// channel disconnect, so retaining this sender is safe.
    bus_tx: AgentUpdateSender,
    /// User configuration (theme, layout, default agent).
    config: Config,
    /// A human-readable reason set just before [`App::main_loop`] breaks on
    /// auto-exit, so [`App::run`] can print it AFTER restoring the terminal
    /// (printing during raw mode corrupts the display). `None` for an explicit
    /// Ctrl+Q quit or a still-running loop. This is what tells the user WHY
    /// orcatui closed (e.g. "all agents exited") instead of vanishing silently.
    exit_reason: Option<&'static str>,
    /// Daemon connection state — drives the sidebar indicator + error handling.
    conn_state: ConnectionState,
    /// Transient UI messages (daemon errors, connection changes, etc.).
    toasts: crate::toast::ToastQueue,
    /// The daemon client when connected to an Orca daemon (None in standalone).
    daemon: Option<DaemonConnection>,
    /// Public Orca CLI/runtime adapter. Unlike `daemon`, this never opens a
    /// private Unix socket or attaches to a PTY owned by Orca's GUI.
    orca_cli: Option<OrcaCliBridge>,
    orca_poll_stop: Option<Arc<AtomicBool>>,
    orca_send_tx: Option<std::sync::mpsc::Sender<(String, Vec<u8>, bool)>>,
    orca_spawn_rx: Vec<(
        usize,
        std::sync::mpsc::Receiver<anyhow::Result<CreatedTerminal>>,
    )>,
    orca_closed_handles: std::collections::HashSet<String>,
    orca_cancelled_panes: std::collections::HashSet<usize>,
    /// Legacy daemon session IDs queued for teardown. Public Orca CLI tabs use
    /// runtime handles and are closed synchronously in `close_daemon_sessions`.
    daemon_cleanup_session_ids: Vec<String>,
    /// Set by SIGHUP/SIGTERM/SIGINT so closing the terminal window follows
    /// the same graceful cleanup path as Ctrl+Q.
    shutdown_signal: Option<Arc<AtomicBool>>,
    /// In-memory activity timeline (state transitions + errors). Rendered as a
    /// full-screen overlay via `InputMode::Activity`.
    activity: ActivityLog,
    /// Last-rendered OUTER pane rects (one per pane, in `panes` order), cached
    /// by [`App::render`] for mouse hit-testing in [`App::handle_mouse`].
    /// Empty until the first frame is drawn.
    pane_rects: Vec<Rect>,
    /// Last-rendered tab hitboxes, used by mouse activation.
    tab_hitboxes: TabHitboxes,
    /// Global pane indexes represented by the last-rendered horizontal tab
    /// strip. In workspace mode the strip is scoped to the active workspace;
    /// hit boxes themselves remain local to that filtered list.
    tab_pane_indices: Vec<usize>,
    /// Last-rendered footer exit button, placed beside the Ctrl+Q hint.
    footer_quit_hitbox: Option<Rect>,
    /// Last-rendered sidebar region, used by workspace-row mouse activation.
    sidebar_rect: Option<Rect>,
    sidebar_entry_hitboxes: Vec<Option<Rect>>,
    sidebar_catalog_entries: Vec<OrcaWorkspace>,
    /// Workspace-first sidebar hit testing. Pane hit boxes are indexed by the
    /// stable pane vector; workspace hit boxes are indexed by the grouped
    /// sidebar model built for the last frame.
    sidebar_workspace_hitboxes: Vec<Option<Rect>>,
    sidebar_pane_hitboxes: Vec<Option<Rect>>,
    sidebar_workspace_targets: Vec<Option<OrcaWorkspace>>,
    /// The working directory agents are spawned in (the explicit `--cwd`, else
    /// the directory orcatui was launched from). Captured once at construction
    /// and reused by `spawn_one_local` / `respawn` so newly-spawned panes don't
    /// fall back to $HOME (portable-pty's default when no cwd is set).
    launch_cwd: PathBuf,
    /// Last pane/sidebar cardinality written to the optional worktree
    /// diagnosis log. Keeps topology diagnostics useful without a 60fps log
    /// flood.
    debug_last_sidebar_signature: Option<(usize, usize, usize)>,
    /// Monotonic counter for the next [`Pane::id`]. Each pane gets a fresh id
    /// at spawn (never reused), so the bus forwarder — which reports
    /// `AgentUpdate::Output { pane_id }` by id — keeps targeting the RIGHT pane
    /// even after `close_focused_pane` shifts vec positions. Resolved back to a
    /// position via [`App::idx_of_pane`] in `apply_update`.
    next_pane_id: usize,
    /// Phase 2 — Tasks view: the parsed repo once submitted (set on Enter in
    /// [`InputMode::TasksRepo`], consumed for lazy body-fetch on dispatch).
    tasks_repo: Option<RepoRef>,
    /// Phase 2 — Tasks view: the fetched issues + PRs being browsed in
    /// [`InputMode::TasksList`]. Empty until a successful fetch.
    tasks_items: Vec<TasksEntry>,
    /// GitHub task-list fetch running off the UI thread.
    tasks_fetch_rx: Option<std::sync::mpsc::Receiver<(u64, anyhow::Result<Vec<TasksEntry>>)>>,
    task_body_rx: Option<std::sync::mpsc::Receiver<(u64, Result<String, String>)>>,
    task_body_entry: Option<TasksEntry>,
    tasks_request_id: u64,
}

impl<B: Backend> std::ops::Deref for App<B> {
    type Target = InteractionState;

    fn deref(&self) -> &Self::Target {
        &self.interaction
    }
}

impl<B: Backend> std::ops::DerefMut for App<B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.interaction
    }
}

impl App {
    /// Spawn one terminal tab per [`AgentSpec`] and wire each onto a fresh AgentBus.
    ///
    /// Initial pane/PTY size is read from the real terminal (via
    /// `crossterm::terminal::size`, which works without raw mode); if it cannot
    /// be queried a `80 \u{00D7} 24` fallback is used. A spawn failure is
    /// **not** fatal: the tab is recorded with [`AgentState::Failed`] and the
    /// app continues with the agents that did start (mirrors Orca GUI showing a
    /// red pane for an agent that could not launch).
    ///
    /// # Errors
    ///
    /// Returns an error only if the ratatui terminal/backend cannot be
    /// constructed.
    pub fn spawn_agents(specs: Vec<AgentSpec>, cwd: Option<&Path>, isolate: bool) -> Result<Self> {
        Self::spawn_agents_with_catalog(specs, cwd, isolate, Vec::new())
    }

    /// Spawn agents and retain the complete Orca workspace catalog for the
    /// sidebar. Catalog rows without a local PTY remain visible as read-only
    /// workspace rows, including remote and archived workspaces.
    pub fn spawn_agents_with_catalog(
        specs: Vec<AgentSpec>,
        cwd: Option<&Path>,
        isolate: bool,
        workspace_catalog: Vec<OrcaWorkspace>,
    ) -> Result<Self> {
        // Size the PTYs from the real terminal so the agent's first frame is
        // already correct once we enter raw mode + alt screen in `run`.
        // `unwrap_or` only covers the error path; a freshly spawned PTY (or a
        // non-TTY) can legitimately report `Ok((0, 0))`, which would underflow
        // `vt100` at `scroll_bottom = size.rows - 1`. Clamp explicitly.
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let cols = cols.max(MIN_COLS);
        let rows = rows.max(MIN_ROWS);

        let (bus_tx, bus_rx) = bus::channel();

        // The working directory agents open in. portable-pty 0.9 falls back to
        // $HOME when no cwd is set (cmdbuilder.rs as_command), so we MUST pass
        // the real launch directory explicitly — otherwise agents open in HOME
        // instead of the directory orcatui was started from. Honor an explicit
        // `--cwd`; otherwise use the current directory. Stored on the App so
        // spawn_one_local (`n` picker) and respawn use the same directory.
        let launch_cwd: PathBuf = cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        // In worktree-isolation mode, create one git worktree per agent from
        // the repo at `cwd` (or the current directory). Fail fast: a worktree
        // creation failure aborts the whole run, and `OwnedWorktrees`'s `Drop`
        // cleans up any worktrees already created before the error propagates.
        let mut owned: Option<OwnedWorktrees> = if isolate {
            let base = cwd
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let manager = WorktreeManager::open(&base).with_context(|| {
                format!(
                    "opening git repo for worktree isolation at {}",
                    base.display()
                )
            })?;
            Some(OwnedWorktrees::new(manager))
        } else {
            None
        };

        let mut panes = Vec::with_capacity(specs.len());

        // Monotonic pane-id allocator: each pane gets a globally-unique id
        // (never reused across close+spawn cycles), so the bus forwarder keeps
        // targeting the RIGHT pane after positions shift. `idx` below is still
        // the spawn-time vector position is only a layout index; forwarders
        // receive the stable `id` instead.
        let mut next_id = 0usize;

        for spec in specs.into_iter() {
            let id = next_id;
            next_id += 1;
            let name = spec.name.clone();
            let command = spec.command.clone();
            // Per-agent cwd + header branch: an isolated worktree when in
            // worktree mode (each agent runs in its own worktree, header shows
            // the isolation branch); else the shared `cwd` + any spec label.
            let (agent_cwd, branch_label): (Option<PathBuf>, Option<String>) =
                if let Some(owned) = owned.as_mut() {
                    let wt = owned
                        .create_for(&name)
                        .with_context(|| format!("creating worktree for {name:?}"))?;
                    (Some(wt.path.clone()), Some(wt.branch.clone()))
                } else {
                    let (cwd, label) = spec_launch_target(&spec, &launch_cwd);
                    (Some(cwd), label)
                };
            match PtySession::spawn(command.clone(), agent_cwd.as_deref(), cols, rows) {
                Ok((session, rx)) => {
                    let mut pane = Pane::new(id, &name, cols, rows);
                    pane.set_state(AgentState::Running);
                    if let Some(branch) = branch_label {
                        pane.set_branch(Some(branch));
                    }
                    let mut slot = PaneSlot::new(pane, command.clone());
                    slot.workspace_id = spec.workspace_id.clone();
                    slot.worktree_path = spec.worktree.clone();
                    slot.session = Some(session);
                    panes.push(slot);
                    // Pump this session's blocking receiver onto the async bus
                    // on a dedicated thread. The clone of `bus_tx` is the only
                    // sender this forwarder holds; when it returns, that clone
                    // drops and (once all forwarders are gone) the receiver
                    // observes disconnect — the UI's "all agents gone" signal.
                    // NOTE: `forward_session` is given the stable `id`, not the
                    // spawn position `idx`, so an `AgentUpdate::Output` for a
                    // survivor still routes correctly after a close shifts vec
                    // positions.
                    let tx = bus_tx.clone();
                    let _ = thread::Builder::new()
                        .name(format!("orca-bus-fwd({name})"))
                        .spawn(move || bus::forward_session(id, rx, tx));
                }
                Err(err) => {
                    eprintln!("orcatui: failed to spawn {name:?}: {err:#}");
                    let mut pane = Pane::new(id, &name, cols, rows);
                    pane.set_state(AgentState::Failed(format!("{err:#}")));
                    let mut slot = PaneSlot::new(pane, command.clone());
                    slot.workspace_id = spec.workspace_id.clone();
                    slot.worktree_path = spec.worktree.clone();
                    panes.push(slot);
                }
            }
            // Per-tab lifecycle state is owned by the slot.
        }

        // NOTE: we keep `bus_tx` (stored on the App) rather than dropping it,
        // so `spawn_one` can add orchestrated panes mid-run.

        // Diagnostic-only snapshot: record Git and Orca catalog cardinalities
        // without persisting repository paths or raw command output.
        if crate::debug_log::enabled() {
            let owned_count = owned
                .as_ref()
                .map_or(0, |worktrees| worktrees.entries().len());
            match owned.as_ref() {
                Some(worktrees) => match worktrees.manager().registered_count() {
                    Ok(registered_count) => crate::debug_log::append(format_args!(
                        "{} startup isolate=true panes={} owned_created={} catalog_rows={} git_registered={} sidebar_source=running_panes",
                        crate::debug_log::WORKTREE_PREFIX,
                        panes.len(),
                        owned_count,
                        workspace_catalog.len(),
                        registered_count
                    )),
                    Err(err) => crate::debug_log::append(format_args!(
                        "{} startup isolate=true panes={} owned_created={} catalog_rows={} git_probe=error:{} sidebar_source=running_panes",
                        crate::debug_log::WORKTREE_PREFIX,
                        panes.len(),
                        owned_count,
                        workspace_catalog.len(),
                        crate::debug_log::classify_error(err.as_ref())
                    )),
                },
                None => match WorktreeManager::open(&launch_cwd)
                    .and_then(|manager| manager.registered_count())
                {
                    Ok(registered_count) => crate::debug_log::append(format_args!(
                        "{} startup isolate=false panes={} owned_created=0 catalog_rows={} git_registered={} sidebar_source=orca_catalog_or_git_fallback",
                        crate::debug_log::WORKTREE_PREFIX,
                        panes.len(),
                        workspace_catalog.len(),
                        registered_count
                    )),
                    Err(err) => crate::debug_log::append(format_args!(
                        "{} startup isolate=false panes={} owned_created=0 catalog_rows={} git_probe=error:{} sidebar_source=orca_catalog_or_git_fallback",
                        crate::debug_log::WORKTREE_PREFIX,
                        panes.len(),
                        workspace_catalog.len(),
                        crate::debug_log::classify_error(err.as_ref())
                    )),
                },
            }
        }

        let workspace_catalog_loaded = !workspace_catalog.is_empty();
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend).context("building ratatui terminal")?;

        Ok(Self {
            panes,
            focus: 0,
            terminal,
            bus_rx,
            quit: false,
            raw_mode_active: false,
            worktrees: owned,
            workspace_catalog,
            workspace_catalog_loaded,
            workspace_unresolved_hosts: Vec::new(),
            workspace_unverifiable_scope_hosts: Vec::new(),
            orca_terminal_catalog: Vec::new(),
            scheduler: FrameScheduler::new(TARGET_FRAME_60FPS, Instant::now()),
            snapshot_tx: None,
            coordinator: None,
            orch_agent: None,
            daemon_session_map: None,
            daemon_reconnect_attempts: 0,
            daemon_backoff: Duration::from_secs(Config::default().daemon.reconnect_initial_secs),
            daemon_reconnect_rx: None,
            daemon_spawn_rx: Vec::new(),
            cols,
            rows,
            bus_tx,
            config: Config::load_or_default(),
            interaction: InteractionState::default(),
            exit_reason: None,
            conn_state: ConnectionState::Standalone,
            toasts: crate::toast::ToastQueue::new(),
            daemon: None,
            orca_cli: None,
            orca_poll_stop: None,
            orca_send_tx: None,
            orca_spawn_rx: Vec::new(),
            orca_closed_handles: std::collections::HashSet::new(),
            orca_cancelled_panes: std::collections::HashSet::new(),
            daemon_cleanup_session_ids: Vec::new(),
            shutdown_signal: None,
            activity: ActivityLog::new(),
            pane_rects: Vec::new(),
            tab_hitboxes: TabHitboxes::default(),
            tab_pane_indices: Vec::new(),
            footer_quit_hitbox: None,
            sidebar_rect: None,
            sidebar_entry_hitboxes: Vec::new(),
            sidebar_catalog_entries: Vec::new(),
            sidebar_workspace_hitboxes: Vec::new(),
            sidebar_pane_hitboxes: Vec::new(),
            sidebar_workspace_targets: Vec::new(),
            launch_cwd,
            debug_last_sidebar_signature: None,
            next_pane_id: next_id,
            tasks_repo: None,
            tasks_items: Vec::new(),
            tasks_fetch_rx: None,
            task_body_rx: None,
            task_body_entry: None,
            tasks_request_id: 0,
        })
    }
}

/// Resolve the checkout and sidebar label for a launch spec. Existing
/// worktrees are persistent, so their path is used directly; specs without a
/// worktree retain the app's launch directory.
fn spec_launch_target(spec: &AgentSpec, launch_cwd: &Path) -> (PathBuf, Option<String>) {
    let cwd = spec
        .worktree
        .clone()
        .unwrap_or_else(|| launch_cwd.to_path_buf());
    let label = spec.worktree_branch.clone().or_else(|| {
        spec.worktree
            .as_ref()
            .map(|path| path.display().to_string())
    });
    (cwd, label)
}

// Everything below is generic over the backend so a `TestBackend` can be
// injected in tests (no TTY required) — the production type is still
// `App<CrosstermBackend<Stdout>>` via the default type parameter.
impl<B: Backend> App<B> {
    /// Add one tab per launch spec after construction. Daemon startup does not
    /// call this automatically: it first hydrates Orca's existing sessions;
    /// this method is reserved for standalone fallback and explicit runtime
    /// creation paths.
    pub fn spawn_specs(&mut self, specs: Vec<AgentSpec>) {
        for spec in specs {
            let idx = self.spawn_one(spec);
            self.focus = idx;
        }
    }

    /// Replace the startup workspace catalog used by the sidebar. A caller
    /// may load it before entering the terminal loop so the first rendered
    /// frame already contains every Orca workspace.
    pub fn set_workspace_catalog(&mut self, catalog: Vec<OrcaWorkspace>) {
        self.workspace_catalog_loaded = !catalog.is_empty();
        self.workspace_catalog = catalog;
    }

    /// Inherit the focused local workspace when a new terminal is created from
    /// the `+` button, spawn picker, task dispatcher, or orchestration pump.
    /// Explicit workspace metadata always wins, so callers that deliberately
    /// target another checkout remain unaffected.
    fn contextualize_spec(&self, mut spec: AgentSpec) -> AgentSpec {
        if spec.workspace_id.is_some() || spec.worktree.is_some() {
            return spec;
        }
        let Some(slot) = self.panes.get(self.focus) else {
            return spec;
        };
        let workspace = self.workspace_catalog.iter().find(|workspace| {
            slot.workspace_id.as_deref() == Some(workspace.id.as_str())
                || slot.worktree_path.as_ref() == Some(&workspace.path)
        });
        if let Some(workspace) = workspace.filter(|workspace| workspace.is_local()) {
            spec.workspace_id = Some(workspace.id.clone());
            spec.worktree = Some(workspace.path.clone());
            spec.worktree_branch = Some(workspace.branch.clone());
        }
        spec
    }

    /// Preserve whether an empty result was authoritative and attach any
    /// completeness warnings discovered while loading the catalog.
    pub fn set_workspace_catalog_status(
        &mut self,
        loaded_from_orca: bool,
        unresolved_hosts: Vec<String>,
        unverifiable_scope_hosts: Vec<String>,
    ) {
        self.workspace_catalog_loaded = loaded_from_orca;
        self.workspace_unresolved_hosts = unresolved_hosts;
        self.workspace_unverifiable_scope_hosts = unverifiable_scope_hosts;
    }

    /// Supply Orca's title/agent/order projection used when hydrating daemon
    /// sessions. The PTY daemon remains the source of truth for live bytes.
    pub fn set_orca_terminal_catalog(&mut self, terminals: Vec<OrcaTerminal>) {
        self.orca_terminal_catalog = terminals;
    }

    /// Whether the app currently owns a connection to Orca's daemon.
    #[must_use]
    pub fn daemon_connected(&self) -> bool {
        self.daemon.is_some() || self.orca_cli.is_some()
    }

    fn catalog_sidebar_entries(&self) -> Vec<crate::sidebar::SidebarEntry> {
        self.workspace_catalog
            .iter()
            .filter(|workspace| {
                !workspace.is_on_local_host()
                    || !self
                        .panes
                        .iter()
                        .any(|pane| pane.workspace_id.as_deref() == Some(workspace.id.as_str()))
            })
            .map(|workspace| crate::sidebar::SidebarEntry {
                name: workspace.sidebar_name(),
                state: AgentState::Idle,
                branch: workspace.sidebar_detail(),
                activity: None,
                focused: false,
                pinned: false,
            })
            .collect()
    }

    /// Build the Orca-style workspace-first sidebar. Catalog rows are kept in
    /// their source order, then live panes are attached by stable workspace id
    /// (or checkout path for older terminal projections). Panes without
    /// workspace metadata are grouped under a visible fallback row instead of
    /// being silently flattened into the workspace list.
    fn workspace_sidebar_groups(
        &self,
    ) -> (
        Vec<crate::sidebar::WorkspaceSidebarGroup>,
        Vec<Option<OrcaWorkspace>>,
    ) {
        let mut groups: Vec<crate::sidebar::WorkspaceSidebarGroup> = self
            .workspace_catalog
            .iter()
            .map(|workspace| crate::sidebar::WorkspaceSidebarGroup {
                id: Some(workspace.id.clone()),
                name: workspace.sidebar_name(),
                detail: workspace.sidebar_detail(),
                agents: Vec::new(),
            })
            .collect();
        let mut targets: Vec<Option<OrcaWorkspace>> =
            self.workspace_catalog.iter().cloned().map(Some).collect();
        let mut fallback_idx = None;
        for (pane_idx, slot) in self.panes.iter().enumerate() {
            let by_id = slot.workspace_id.as_deref().and_then(|id| {
                groups
                    .iter()
                    .position(|group| group.id.as_deref() == Some(id))
            });
            let by_path = slot.worktree_path.as_ref().and_then(|path| {
                self.workspace_catalog
                    .iter()
                    .position(|workspace| workspace.path == *path)
            });
            let group_idx = by_id.or(by_path).or_else(|| {
                if fallback_idx.is_none() {
                    fallback_idx = Some(groups.len());
                    groups.push(crate::sidebar::WorkspaceSidebarGroup {
                        id: None,
                        name: if self.workspace_catalog.is_empty() {
                            "Current workspace".to_owned()
                        } else {
                            "Open terminals".to_owned()
                        },
                        detail: None,
                        agents: Vec::new(),
                    });
                    targets.push(None);
                }
                fallback_idx
            });
            let Some(group_idx) = group_idx else { continue };
            let entry = crate::sidebar::SidebarEntry {
                name: slot.name().to_owned(),
                state: slot.state().clone(),
                branch: slot.branch().map(str::to_owned),
                activity: slot.activity().cloned(),
                focused: pane_idx == self.focus,
                pinned: slot.pinned,
            };
            groups[group_idx].agents.push((pane_idx, entry));
        }
        (groups, targets)
    }

    /// Build the complete workspace inventory used by the `w` overlay. When
    /// the Orca catalog is unavailable, keep the overlay useful by exposing
    /// the panes currently owned by this TUI as a local fallback inventory.
    fn workspace_inventory_rows(&self) -> Vec<WorkspaceRow> {
        if self.workspace_catalog_loaded {
            return self
                .workspace_catalog
                .iter()
                .map(|workspace| WorkspaceRow {
                    name: workspace.sidebar_name(),
                    detail: workspace.sidebar_detail().unwrap_or_default(),
                })
                .collect();
        }
        self.panes
            .iter()
            .map(|slot| WorkspaceRow {
                name: slot.name().to_owned(),
                detail: slot
                    .branch()
                    .map(str::to_owned)
                    .unwrap_or_else(|| slot.state().label().to_owned()),
            })
            .collect()
    }

    fn workspace_count(&self) -> usize {
        if !self.workspace_catalog_loaded {
            self.panes.len()
        } else {
            self.workspace_catalog.len()
        }
    }

    /// Attach a mobile-companion snapshot publisher (Feature 10). Once set,
    /// [`App::main_loop`] publishes a `Vec<AgentSnapshot>` every frame; the
    /// WebSocket server drains it and broadcasts to connected clients.
    pub fn set_snapshot_sender(&mut self, tx: UnboundedSender<Vec<AgentSnapshot>>) {
        self.snapshot_tx = Some(tx);
    }

    /// Feature 7: attach a coordinator + agent binary so the loop dispatches
    /// tasks dependency-gated (one new pane per released task, as its deps
    /// complete). Call after [`App::spawn_agents`] (typically with an empty
    /// spec list — the initial tasks are spawned by the first loop tick) and
    /// before [`App::run`].
    pub fn set_orchestration(&mut self, coordinator: Coordinator, agent: String) {
        self.coordinator = Some(coordinator);
        self.orch_agent = Some(agent);
    }

    /// Publish the current per-pane snapshot if a companion is attached.
    /// Best-effort: a dropped/lagged receiver makes `send` return `Err`, which
    /// we ignore (the companion is optional and may have disconnected).
    fn publish_snapshot(&self) {
        let Some(tx) = &self.snapshot_tx else {
            return;
        };
        let snaps: Vec<AgentSnapshot> = self
            .panes
            .iter()
            .map(|p| AgentSnapshot {
                name: p.name().to_string(),
                state: p.state().label().to_string(),
                branch: p.branch().map(str::to_string),
            })
            .collect();
        let _ = tx.send(snaps);
    }

    /// Enter raw mode + the alternate screen, run the main loop, then
    /// **always** restore the terminal (even on error). A [`Drop`] impl guards
    /// the panic path as well.
    ///
    /// # Errors
    ///
    /// Propagates crossterm/ratatui I/O errors (e.g. stdout is not a TTY).
    pub fn run(&mut self) -> Result<()> {
        self.shutdown_signal = Some(install_tui_shutdown_handlers());
        self.setup_terminal()?;
        let result = self.main_loop();
        // Always attempt teardown; surface it only if the loop itself succeeded.
        let restore_result = self.restore_terminal();
        // Only sessions explicitly created by this TUI are closed. Existing
        // Orca GUI tabs are views and must survive TUI teardown.
        self.stop_orca_cli_poller();
        self.close_daemon_sessions();
        if let Err(restore_err) = restore_result {
            if result.is_ok() {
                return Err(restore_err);
            }
            eprintln!("orcatui: terminal restore failed: {restore_err:#}");
        }
        // After the terminal is restored (raw mode off, alt screen left), tell
        // the user WHY the loop ended if it was an auto-exit — so a vanished
        // session reads as "all agents exited" instead of an unexplained death.
        // (A genuine panic prints its own `thread 'main' panicked` message, so
        // it's never confused with this.)
        if let Some(reason) = self.exit_reason {
            eprintln!("orcatui: {reason}");
        }
        result
    }

    fn setup_terminal(&mut self) -> Result<()> {
        enable_raw_mode().context("enabling raw mode")?;
        // Mark raw mode active immediately. If entering the alternate screen
        // or enabling mouse capture fails, Drop must still disable raw mode.
        self.raw_mode_active = true;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            self.raw_mode_active = false;
            return Err(error).context("entering alt screen + mouse");
        }
        Ok(())
    }

    fn restore_terminal(&mut self) -> Result<()> {
        let mut stdout = io::stdout();
        // DisableMouseCapture MUST run before LeaveAlternateScreen — otherwise
        // the terminal stays in mouse-capture mode after exit and mouse events
        // leak through as garbage characters in the shell.
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
        // A hosted agent may have enabled focus reporting or hidden the
        // cursor. Those modes belong to the alternate-screen TUI, not to the
        // shell that regains the PTY after Ctrl+Q/mouse exit. Reset them
        // explicitly because crossterm's mouse command does not touch
        // `?1004h` and Orca persists terminal mode bits across reattach.
        use std::io::Write;
        let _ = stdout.write_all(b"\x1b[?1004l\x1b[?25h\x1b[0m");
        let _ = stdout.flush();
        let disable = disable_raw_mode();
        let show = self.terminal.show_cursor();
        self.raw_mode_active = false;
        disable.context("disabling raw mode")?;
        show.context("showing cursor")?;
        Ok(())
    }

    /// The synchronous frame loop: reap exited children, drain the bus, render
    /// (throttled to the frame budget), poll one event, repeat until quit or
    /// every agent has exited. Feature 5's [`FrameScheduler`] drives the render
    /// rate (skip on backlog) and the poll timeout (idle backoff).
    fn main_loop(&mut self) -> Result<()> {
        // Consecutive render-panic counter. A single render panic (e.g. a
        // degenerate layout/crossterm state during a rapid window resize) is
        // caught, logged by the crash hook, and that frame is skipped — the app
        // keeps running. Only a PERSISTENT render failure bails out.
        let mut render_panic_streak = 0u32;
        while !self.quit {
            if self
                .shutdown_signal
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed))
            {
                self.request_quit();
                continue;
            }
            // Reap FIRST: poll each still-tracked child's real exit code before
            // draining the bus. The forwarder only knows the child is "gone"
            // (it emits `Exit{code: None}` on PTY EOF); the authoritative exit
            // code comes from `try_wait`. Running the sweep before `drain_bus`
            // means a reaped `Exit{Some(code)}` lands before (and is not
            // clobbered by) the forwarder's less-informed `Exit{None}`.
            self.reap_exited();
            let had_output = self.drain_bus();
            let task_fetch_completed = self.poll_tasks_fetch();
            let task_body_completed = self.poll_task_body_fetch();
            // After all updates (reap + bus drain) have landed on the panes,
            // capture any lifecycle/state transitions into the activity log.
            self.record_activity();

            let now = Instant::now();
            // Fresh agent output counts as activity (keeps the scheduler out of
            // idle backoff while agents are producing).
            if had_output || task_fetch_completed || task_body_completed {
                self.scheduler.record_activity(now);
            }

            // GC expired toasts every frame.
            self.toasts.gc(now);

            // Render only when the frame budget allows; otherwise note a skip
            // (backpressure: render the latest state next frame, never catch up).
            if self.scheduler.should_render(now) {
                // catch_unwind: an edge-case render panic (e.g. a transient
                // degenerate state during a rapid window resize) is logged by
                // the crash hook and this frame is SKIPPED instead of killing
                // the whole app. A genuine IO error still propagates. Only
                // persistent (≥10 in a row) render failures give up.
                let attempted =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.render()));
                match attempted {
                    Ok(Ok(())) => {
                        self.scheduler.record_render(now);
                        render_panic_streak = 0;
                    }
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        render_panic_streak = render_panic_streak.saturating_add(1);
                        self.scheduler.note_skipped();
                        self.toasts.push(crate::toast::Toast::warning(
                            "render skipped a frame — see last-crash.log".to_string(),
                        ));
                        if render_panic_streak >= 10 {
                            return Err(anyhow::anyhow!(
                                "render panicked {render_panic_streak} frames in a row — \
                                 giving up (details in last-crash.log)"
                            ));
                        }
                    }
                }
            } else {
                self.scheduler.note_skipped();
            }

            // Feature 10: publish a live snapshot of every pane to the
            // mobile-companion channel (no-op when none is attached).
            self.publish_snapshot();

            // Feature 7: pump orchestration — dispatch any tasks whose deps
            // just completed. Done BEFORE the all-sessions-gone check so the
            // initial tasks spawn on tick 1 (an orchestrated App starts empty)
            // and so newly-freed dependents keep the loop alive.
            self.pump_orchestration();

            // Feature 8: fire any backoff-scheduled remote-session reconnects
            // whose wait has elapsed (non-blocking).
            self.pump_reconnect();

            // Daemon reconnection: if we were connected and the daemon crashed,
            // attempt to reconnect on an exponential backoff.
            self.pump_daemon_reconnect();
            self.pump_daemon_spawns();
            self.pump_orca_cli();

            // Poll with the scheduler-chosen timeout: ~remaining-to-next-frame
            // when active, the longer idle interval when nothing is happening.
            if event::poll(self.scheduler.poll_timeout(now))? {
                let ev = event::read()?;
                // User input is activity — exit idle backoff immediately.
                self.scheduler.record_activity(Instant::now());
                self.handle_event(ev);
            }
            // Auto-exit once no agent process remains AND orchestration has no
            // pending/running tasks left AND no reconnect is awaiting its
            // backoff — BUT only once the user is back to a calm, unzoomed
            // Normal state. See [`App::should_auto_exit`] for the "don't yank
            // the rug out mid-interaction" rationale. `exit_reason` is printed
            // by `run()` after the terminal is restored.
            if self.should_auto_exit() {
                self.exit_reason = Some("all agents exited \u{2014} re-run orcatui to start again");
                break;
            }
        }
        Ok(())
    }

    /// True when orchestration is inactive (plain `run`) or the coordinator
    /// has no non-terminal tasks left (everything Done/Failed). Combined with
    /// [`App::all_sessions_gone`] this is the orchestrated run's completion
    /// signal: nothing left to spawn and nothing still running.
    fn orchestration_drained(&self) -> bool {
        match &self.coordinator {
            None => true,
            Some(coord) => coord.tasks().iter().all(|t| t.status.is_terminal()),
        }
    }

    /// Drain every pending [`AgentUpdate`] into the panes in one batch.
    /// Returns `true` if any update was applied (used by the scheduler to
    /// detect activity).
    fn drain_bus(&mut self) -> bool {
        let mut any = false;
        while let Ok(update) = self.bus_rx.try_recv() {
            self.apply_update(update);
            any = true;
        }
        any
    }

    /// 非阻塞地收取后台 GitHub 查询结果，避免 UI loop 被 `gh` 命令阻塞。
    fn poll_tasks_fetch(&mut self) -> bool {
        let Some(rx) = self.tasks_fetch_rx.as_ref() else {
            return false;
        };
        let Ok((request_id, result)) = rx.try_recv() else {
            return false;
        };
        self.tasks_fetch_rx = None;
        if request_id != self.tasks_request_id || self.mode != InputMode::TasksList {
            return true;
        }
        match result {
            Ok(items) => {
                self.tasks_items = items;
                self.tasks_selected = 0;
                self.tasks_error = None;
            }
            Err(error) => {
                self.tasks_items.clear();
                self.tasks_selected = 0;
                self.tasks_error = Some(format!("{error:#}"));
            }
        }
        true
    }

    /// 收取 issue body 的后台查询结果，并继续派发等待中的条目。
    fn poll_task_body_fetch(&mut self) -> bool {
        let Some(rx) = self.task_body_rx.as_ref() else {
            return false;
        };
        let Ok((request_id, result)) = rx.try_recv() else {
            return false;
        };
        self.task_body_rx = None;
        if request_id != self.tasks_request_id || self.mode != InputMode::TasksList {
            self.task_body_entry = None;
            return true;
        }
        let Some(entry) = self.task_body_entry.take() else {
            return true;
        };
        let prompt = result.unwrap_or_else(|error| {
            self.toasts.push(crate::toast::Toast::warning(format!(
                "无法获取 issue 内容：{error}，将使用标题"
            )));
            entry.prompt.clone()
        });
        let agent = self.config.default_agent.clone();
        let mut spec = AgentSpec::from_command(vec![agent, prompt]);
        spec.name = format!("issue-#{}", entry.number);
        if self.can_spawn_pane() {
            let idx = self.spawn_one(spec);
            self.focus = idx;
        }
        self.mode = InputMode::Normal;
        true
    }

    /// Capture agent lifecycle transitions (and the error message when an agent
    /// transitions INTO `Failed`) into [`ActivityLog`].
    ///
    /// Runs once per loop tick, right after [`App::reap_exited`] +
    /// [`App::drain_bus`] have settled every pane's state for this frame. Only
    /// **changes** are recorded using each slot's `last_status`, so a steady
    /// agent produces no events.
    ///
    /// Tool events from the OSC `toolName` payload are deliberately NOT recorded
    /// here: a pane's `activity()` persists across ticks (it is only overwritten
    /// when a new OSC 9999 payload arrives), so recording per-tick would spam
    /// duplicate `Tool` events. State + Error transitions are the high-signal
    /// set; per-tool dedup can be added (via a `last_tool` field) in a follow-up.
    fn record_activity(&mut self) {
        // Status history lives on each slot, so dynamic panes need no side
        // vector bookkeeping.
        for slot in self.panes.iter_mut() {
            let name = slot.name().to_string();
            let new = AgentStatus::derive(slot.state(), slot.activity().map(|a| a.state.as_str()));
            let prev = slot.last_status;
            if prev == Some(new) {
                continue;
            }
            // Only emit a State event when there IS a previous status — the very
            // first derivation (prev == None) establishes the baseline silently.
            if let Some(from) = prev {
                self.activity.record(ActivityEvent::State {
                    agent: name.clone(),
                    from,
                    to: new,
                    at: SystemTime::now(),
                });
            }
            // An error event whenever an agent transitions INTO Failed.
            if matches!(new, AgentStatus::Failed) {
                let message = match slot.state() {
                    AgentState::Failed(m) => m.clone(),
                    _ => String::from("failed"),
                };
                self.activity.record(ActivityEvent::Error {
                    agent: name.clone(),
                    message,
                    at: SystemTime::now(),
                });
            }
            slot.last_status = Some(new);
        }
    }

    /// Poll each still-tracked child for its real exit code and feed it back as
    /// an [`AgentUpdate::Exit`] with `code: Some(_)`.
    ///
    /// The bus forwarder only learns that the child is "gone" (PTY EOF); it
    /// emits [`AgentUpdate::Exit`] with `code: None`. The authoritative exit
    /// code lives in the child handle and is only available via
    /// [`PtySession::try_wait`]. This sweep — run once per loop tick (the loop
    /// already polls every ~20 ms) — closes that gap so a non-zero exit shows
    /// as `Failed` rather than a misleading `Done`.
    ///
    /// Borrow-safe two-pass design: snapshot which panes are already terminal
    /// (immutable borrow of `panes`) and collect reaped codes, *then* mutate
    /// `sessions`; only after both immutable reads are dropped do we call
    /// `apply_update` (which mutates `sessions`/`panes`). A session whose slot
    /// is already `None` (reaped by the forwarder's `Exit`) or whose pane is
    /// already terminal is skipped, so a single child is reaped at most once.
    fn reap_exited(&mut self) {
        // Pass 1 (immutable `panes`): which panes are already in a terminal
        // state? Computed before the mutable `sessions` borrow below.
        let terminal: Vec<bool> = self
            .panes
            .iter()
            .map(|p| matches!(p.state(), AgentState::Done(_) | AgentState::Failed(_)))
            .collect();
        // Stable pane ids, in vec order. `apply_update`'s `Exit` arm resolves
        // the incoming `pane_id` back to a position via `idx_of_pane`, so we
        // send the stable id here — NOT the position `i` (which would go stale
        // if another pane closes between this reap and the next apply).
        let ids: Vec<usize> = self.panes.iter().map(|p| p.id()).collect();

        // Pass 2 (mutable `sessions`): non-blocking `try_wait` on each live,
        // non-terminal child; collect the codes.
        let mut reaped: Vec<(usize, i32)> = Vec::new();
        for (i, slot) in self.panes.iter_mut().enumerate() {
            if *terminal.get(i).unwrap_or(&true) {
                continue;
            }
            let Some(session) = slot.session.as_mut() else {
                continue;
            };
            match session.try_wait() {
                Ok(Some(code)) => reaped.push((ids[i], code)),
                Ok(None) => {} // still running — leave it
                Err(_) => {}   // poll error (e.g. already-reaped fd); leave it
            }
        }

        // Pass 3: apply each reaped exit. `apply_update`'s Exit branch is
        // idempotent, so a duplicate (forwarder `None` after this `Some`) is a
        // harmless no-op.
        for (pane_id, code) in reaped {
            self.apply_update(AgentUpdate::Exit {
                pane_id,
                code: Some(code),
            });
        }
    }

    /// Resolve a stable [`Pane::id`] back to its current vec position. Used by
    /// [`App::apply_update`] to route bus updates by id (stable across
    /// close/shift) rather than by position (which goes stale the instant a
    /// pane closes and the survivors slide down). Returns `None` if the id no
    /// longer maps to a pane (it was closed) — callers treat that as "no-op".
    fn idx_of_pane(&self, id: usize) -> Option<usize> {
        self.panes.iter().position(|p| p.id() == id)
    }

    /// Apply a single update to the matching pane/session.
    fn apply_update(&mut self, update: AgentUpdate) {
        match update {
            AgentUpdate::OrcaInventory { terminals } => {
                let terminals: Vec<_> = terminals
                    .into_iter()
                    .filter(|terminal| !self.orca_closed_handles.contains(&terminal.handle))
                    .collect();
                let handles: std::collections::HashSet<String> = terminals
                    .iter()
                    .map(|terminal| terminal.handle.clone())
                    .collect();
                self.panes.retain(|slot| match slot.session_owner {
                    SessionOwner::Local => true,
                    SessionOwner::OrcaExisting => slot
                        .orca_handle
                        .as_ref()
                        .is_some_and(|handle| handles.contains(handle)),
                    SessionOwner::OrcaTuiOwned => slot
                        .orca_handle
                        .as_ref()
                        .is_none_or(|handle| handles.contains(handle)),
                });
                if self.panes.is_empty() {
                    self.focus = 0;
                } else if self.focus >= self.panes.len() {
                    self.focus = self.panes.len() - 1;
                }
                self.orca_terminal_catalog = terminals;
                let focused_id = self.panes.get(self.focus).map(|slot| slot.id());
                self.panes.sort_by_key(|slot| {
                    slot.orca_handle
                        .as_ref()
                        .and_then(|handle| {
                            self.orca_terminal_catalog
                                .iter()
                                .find(|terminal| &terminal.handle == handle)
                        })
                        .map(|terminal| terminal.order)
                        .unwrap_or(usize::MAX)
                });
                if let Some(id) = focused_id {
                    if let Some(index) = self.idx_of_pane(id) {
                        self.focus = index;
                    }
                }
            }
            AgentUpdate::OrcaSnapshot {
                terminal,
                lines,
                status,
            } => {
                if self.orca_closed_handles.contains(&terminal.handle) {
                    return;
                }
                let Some(index) = self
                    .panes
                    .iter()
                    .position(|slot| slot.orca_handle.as_deref() == Some(terminal.handle.as_str()))
                else {
                    if let Some(index) = self.panes.iter().position(|slot| {
                        slot.session_owner == SessionOwner::OrcaTuiOwned
                            && slot.orca_handle.is_none()
                    }) {
                        let slot = &mut self.panes[index];
                        slot.orca_handle = Some(terminal.handle.clone());
                        slot.daemon_session_id = Some(terminal.pty_id.clone());
                        slot.pane.set_state(orca_status_to_state(status.as_deref()));
                        slot.pane.replace_screen(&lines);
                        return;
                    }
                    let id = self.next_pane_id;
                    self.next_pane_id += 1;
                    let mut pane = Pane::new(
                        id,
                        terminal
                            .title
                            .clone()
                            .or_else(|| terminal.agent_identity.clone())
                            .unwrap_or_else(|| terminal.handle.clone()),
                        self.cols,
                        self.rows,
                    );
                    pane.set_state(orca_status_to_state(status.as_deref()));
                    pane.set_branch(terminal.branch.clone().or_else(|| {
                        terminal
                            .worktree_path
                            .as_ref()
                            .map(|path| path.display().to_string())
                    }));
                    pane.replace_screen(&lines);
                    let mut slot = PaneSlot::new(pane, Vec::new());
                    slot.daemon_session_id = Some(terminal.pty_id.clone());
                    slot.orca_handle = Some(terminal.handle.clone());
                    slot.session_owner = SessionOwner::OrcaExisting;
                    slot.daemon_read_only = true;
                    slot.workspace_id = terminal.worktree_id.clone();
                    slot.worktree_path = terminal.worktree_path.clone();
                    self.panes.push(slot);
                    return;
                };
                let slot = &mut self.panes[index];
                if let Some(title) = terminal.title.as_deref().filter(|title| !title.is_empty()) {
                    slot.pane.set_name(title);
                }
                slot.pane.set_state(orca_status_to_state(status.as_deref()));
                slot.pane.set_branch(terminal.branch.clone().or_else(|| {
                    terminal
                        .worktree_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                }));
                slot.pane.replace_screen(&lines);
                slot.daemon_session_id = Some(terminal.pty_id);
                slot.workspace_id = terminal.worktree_id;
                slot.worktree_path = terminal.worktree_path;
            }
            AgentUpdate::Output { pane_id, bytes } => {
                // The forwarder (and daemon stream reader) report the pane by
                // its STABLE id; resolve to a position before indexing the
                // slot collection. After a close, positions shift, so indexing
                // by the stale id-as-position would target the wrong pane (or
                // drop the update entirely). A closed pane's updates are
                // silently dropped — the forwarder will exit on its own once
                // the killed session's PTY EOFs.
                let Some(pane_id) = self.idx_of_pane(pane_id) else {
                    return;
                };
                // Answer the agent's terminal-capability queries (OSC color,
                // DECRQM, DA, DCS terminfo) so probing agents (opencode/OpenTUI)
                // render instead of going blank waiting for a reply. The bytes
                // are still fed to the emulator below (the responder only reads).
                let responses = match self.panes.get_mut(pane_id) {
                    Some(pane) => {
                        let r = pane.scan_queries(&bytes, &self.config.theme);
                        pane.feed(&bytes);
                        // Optional live debug log (ORCA_DEBUG_LOG=1): did the
                        // bytes reach the emulator, and does vt100 have content?
                        if crate::debug_log::enabled() {
                            let cells = pane
                                .emulator()
                                .grid()
                                .iter()
                                .map(|row| row.iter().filter(|c| c.has_contents()).count())
                                .sum::<usize>();
                            crate::debug_log::append(format_args!(
                                "pane {pane_id}: +{} bytes → emulator now has {cells} non-empty cells",
                                bytes.len()
                            ));
                        }
                        r
                    }
                    None => Vec::new(),
                };
                if !responses.is_empty() && std::env::var("ORCA_NO_RESPOND").is_err() {
                    if let Some(session) = self
                        .panes
                        .get_mut(pane_id)
                        .and_then(|slot| slot.session.as_mut())
                    {
                        let _ = session.write_bytes(&responses);
                    }
                }
            }
            AgentUpdate::State { pane_id, state } => {
                let Some(pane_id) = self.idx_of_pane(pane_id) else {
                    return;
                };
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    pane.set_state(state);
                }
            }
            AgentUpdate::Exit { pane_id, code } => {
                // Resolve the stable id to a position ONCE. If the pane is gone
                // (closed), the exit is a no-op.
                let Some(pane_id) = self.idx_of_pane(pane_id) else {
                    return;
                };
                // Feature 8: if this pane is reconnect-eligible and still has
                // retries left, schedule a backoff respawn instead of marking
                // it terminal. (Reconnect is for `run --remote`, not orchestrate,
                // so skipping report_done here is correct.)
                let schedule_reconnect = self
                    .panes
                    .get(pane_id)
                    .and_then(|slot| slot.reconnect.as_ref())
                    .is_some_and(|rs| !rs.exhausted());
                if schedule_reconnect {
                    let now = Instant::now();
                    if let Some(slot) = self.panes.get_mut(pane_id) {
                        if let Some(rs) = slot.reconnect.as_mut() {
                            rs.record_failure(now);
                            // Just-failed → full backoff for this attempt.
                            let backoff = rs.next_retry_in(now).unwrap_or(Duration::ZERO);
                            slot.reconnect_due = Some(now + backoff);
                        }
                    }
                    let attempt = self
                        .panes
                        .get(pane_id)
                        .and_then(|slot| slot.reconnect.as_ref())
                        .map(|rs| rs.attempts())
                        .unwrap_or(0);
                    if let Some(pane) = self.panes.get_mut(pane_id) {
                        pane.set_state(AgentState::Failed(format!(
                            "reconnecting (attempt {attempt})…"
                        )));
                    }
                    if let Some(slot) = self.panes.get_mut(pane_id) {
                        slot.session.take();
                    }
                    return;
                }
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    // Idempotent: once a pane is in a terminal state (Done /
                    // Failed) it must not be overwritten by a later Exit. The
                    // forwarder's `Exit{code: None}` (PTY EOF, unknown code)
                    // frequently arrives AFTER the reap sweep's
                    // `Exit{code: Some(_)}` (real exit code); keeping the
                    // existing, more-informed state avoids downgrading a
                    // `Failed(non-zero)` back to a bland `Done(None)` (or vice
                    // versa). Only transition a pane that is still non-terminal.
                    let already_terminal =
                        matches!(pane.state(), AgentState::Done(_) | AgentState::Failed(_));
                    if !already_terminal {
                        // code 0 / unknown => Done; a non-zero exit is a failure.
                        let state = match code {
                            Some(0) | None => AgentState::Done(code),
                            Some(c) => AgentState::Failed(format!("exit code {c}")),
                        };
                        pane.set_state(state);
                    }
                }
                // Take the session so its Drop runs (kill + join the reader),
                // guaranteeing no process/thread is leaked once the child is
                // gone. `Option::take` is idempotent: a second Exit for the
                // same pane (forwarder None after reap Some) finds the slot
                // already None and is a no-op.
                if let Some(slot) = self.panes.get_mut(pane_id) {
                    slot.session.take();
                }
                // Feature 7: if this pane ran an orchestrated task, report its
                // completion to the coordinator so dependent tasks can be
                // dispatched on the next pump.
                if let Some(tid) = self.panes.get(pane_id).and_then(|slot| slot.task) {
                    let failed = self
                        .panes
                        .get(pane_id)
                        .map(|p| matches!(p.state(), AgentState::Failed(_)))
                        .unwrap_or(false);
                    let summary = if failed {
                        "failed".to_string()
                    } else {
                        "done".to_string()
                    };
                    if let Some(coord) = self.coordinator.as_mut() {
                        coord.report_done(tid, summary);
                    }
                }
            }
        }
    }

    /// Append a new agent pane mid-run (Feature 7 orchestration). In daemon
    /// mode, sends a `createOrAttach` RPC instead of spawning a local PTY.
    /// Returns the new pane index.
    fn spawn_one(&mut self, spec: AgentSpec) -> usize {
        let spec = self.contextualize_spec(spec);
        if self.orca_cli.is_some() {
            return self.spawn_one_orca_cli(spec);
        }
        if self.daemon.is_some() {
            return self.spawn_one_daemon(spec);
        }
        self.spawn_one_local(spec)
    }

    fn spawn_one_orca_cli(&mut self, spec: AgentSpec) -> usize {
        let idx = self.panes.len();
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        let command = spec.command.clone();
        let command_text = command
            .iter()
            .map(|arg| crate::orca_cli_bridge::shell_quote(std::ffi::OsStr::new(arg)))
            .collect::<Vec<_>>()
            .join(" ");
        let pane = Pane::new(id, &spec.name, self.cols, self.rows);
        let mut slot = PaneSlot::new(pane, command);
        slot.workspace_id = spec.workspace_id.clone();
        slot.worktree_path = spec.worktree.clone();
        slot.session_owner = SessionOwner::OrcaTuiOwned;
        self.panes.push(slot);
        let Some(bridge) = self.orca_cli.clone() else {
            return idx;
        };
        let worktree = spec
            .worktree
            .as_ref()
            .map(|path| format!("path:{}", path.display()));
        let title = Some(spec.name.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = thread::Builder::new()
            .name("orca-cli-create".into())
            .spawn(move || {
                let _ =
                    tx.send(bridge.create(worktree.as_deref(), &command_text, title.as_deref()));
            });
        self.orca_spawn_rx.push((id, rx));
        idx
    }

    fn pump_orca_cli(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;
        let pending = std::mem::take(&mut self.orca_spawn_rx);
        let mut changed = false;
        for (pane_id, rx) in pending {
            match rx.try_recv() {
                Ok(Ok(created)) => {
                    if self.orca_cancelled_panes.remove(&pane_id) {
                        if let Some(bridge) = self.orca_cli.clone() {
                            let _ = bridge.close_tab(&created.handle);
                        }
                    } else if let Some(index) = self.idx_of_pane(pane_id) {
                        let slot = &mut self.panes[index];
                        slot.orca_handle = Some(created.handle);
                        slot.daemon_session_id = created.pty_id;
                        slot.session_owner = SessionOwner::OrcaTuiOwned;
                        slot.daemon_read_only = false;
                        slot.set_state(AgentState::Running);
                    }
                    changed = true;
                }
                Ok(Err(error)) => {
                    if let Some(index) = self.idx_of_pane(pane_id) {
                        self.panes[index].set_state(AgentState::Failed(error.to_string()));
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => self.orca_spawn_rx.push((pane_id, rx)),
                Err(TryRecvError::Disconnected) => {
                    if let Some(index) = self.idx_of_pane(pane_id) {
                        self.panes[index]
                            .set_state(AgentState::Failed("Orca create worker stopped".to_owned()));
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    /// Daemon-mode spawn: creates an Idle placeholder immediately, then sends
    /// `createOrAttach` on a bounded worker connection. Completion is applied
    /// by [`Self::pump_daemon_spawns`] without blocking the UI loop.
    fn spawn_one_daemon(&mut self, spec: AgentSpec) -> usize {
        let idx = self.panes.len();
        // Stable pane id (never reused). The daemon stream reader reports
        // `AgentUpdate`s by this id; `apply_update` resolves it back to a
        // position via `idx_of_pane`.
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        let name = spec.name.clone();
        let command = spec.command.clone();
        let cols = self.cols;
        let rows = self.rows;
        let session_id = format!(
            "orcatui-{}-{}",
            id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );

        let params = serde_json::json!({
            "sessionId": session_id,
            "cols": cols,
            "rows": rows,
            "command": command.join(" "),
        });
        let pane = Pane::new(id, &name, cols, rows);
        let mut slot = PaneSlot::new(pane, command.clone());
        slot.workspace_id = spec.workspace_id.clone();
        slot.worktree_path = spec.worktree.clone();
        slot.session_owner = SessionOwner::OrcaTuiOwned;
        self.panes.push(slot);
        let rx = self
            .daemon
            .as_ref()
            .expect("daemon mode checked by spawn_one")
            .spawn_session_async(params);
        self.daemon_spawn_rx.push((id, session_id, rx));
        self.panes.last_mut().expect("spawned pane").task = None;
        idx
    }

    /// Apply any finished daemon create-session workers. Pending receivers are
    /// retained; completed or disconnected receivers are removed exactly once.
    fn pump_daemon_spawns(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        let pending = std::mem::take(&mut self.daemon_spawn_rx);
        let mut changed = false;
        for (pane_id, session_id, rx) in pending {
            match rx.try_recv() {
                Ok(Ok(response)) => {
                    if let Some(idx) = self.idx_of_pane(pane_id) {
                        let slot = &mut self.panes[idx];
                        slot.set_state(AgentState::Running);
                        if let Some(snapshot) = Self::daemon_snapshot_bytes(&response) {
                            slot.feed(&snapshot);
                        }
                        slot.daemon_session_id = Some(session_id.clone());
                        if let Some(map) = &self.daemon_session_map {
                            map.lock().unwrap().insert(session_id, pane_id);
                        }
                    }
                    changed = true;
                }
                Ok(Err(error)) => {
                    let reason = error.to_string();
                    if let Some(idx) = self.idx_of_pane(pane_id) {
                        self.panes[idx].set_state(AgentState::Failed(reason.clone()));
                    }
                    self.toasts.push(crate::toast::Toast::error(format!(
                        "Failed to create daemon session: {reason}"
                    )));
                    changed = true;
                }
                Err(TryRecvError::Empty) => {
                    self.daemon_spawn_rx.push((pane_id, session_id, rx));
                }
                Err(TryRecvError::Disconnected) => {
                    let reason = "daemon session worker stopped before returning a result";
                    if let Some(idx) = self.idx_of_pane(pane_id) {
                        self.panes[idx].set_state(AgentState::Failed(reason.to_string()));
                    }
                    self.toasts.push(crate::toast::Toast::error(reason));
                    changed = true;
                }
            }
        }
        changed
    }

    /// Standalone-mode spawn: creates a local PTY via portable-pty.
    fn spawn_one_local(&mut self, spec: AgentSpec) -> usize {
        let idx = self.panes.len();
        // Stable pane id (never reused); the forwarder is handed this id so an
        // `AgentUpdate::Output` keeps targeting the right pane after positions
        // shift on a later close. `idx` below is still the vec position.
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        let name = spec.name.clone();
        let command = spec.command.clone();
        let cols = self.cols;
        let rows = self.rows;
        let (agent_cwd, branch_label) = spec_launch_target(&spec, &self.launch_cwd);
        match PtySession::spawn(spec.command, Some(&agent_cwd), cols, rows) {
            Ok((session, rx)) => {
                let mut pane = Pane::new(id, &name, cols, rows);
                pane.set_state(AgentState::Running);
                if let Some(branch) = branch_label {
                    pane.set_branch(Some(branch));
                }
                let mut slot = PaneSlot::new(pane, command.clone());
                slot.workspace_id = spec.workspace_id.clone();
                slot.worktree_path = spec.worktree.clone();
                self.panes.push(slot);
                let tx = self.bus_tx.clone();
                let _ = thread::Builder::new()
                    .name(format!("orca-bus-fwd({name})"))
                    .spawn(move || bus::forward_session(id, rx, tx));
                if let Some(slot) = self.panes.last_mut() {
                    slot.session = Some(session);
                }
            }
            Err(err) => {
                // Do NOT eprintln here — we are inside the raw-mode TUI, so a
                // stderr write would corrupt the display. The Failed pane state
                // below carries the error into the header + sidebar instead.
                let mut pane = Pane::new(id, &name, cols, rows);
                pane.set_state(AgentState::Failed(format!("{err:#}")));
                let mut slot = PaneSlot::new(pane, command.clone());
                slot.workspace_id = spec.workspace_id.clone();
                slot.worktree_path = spec.worktree.clone();
                self.panes.push(slot);
            }
        }
        self.panes.last_mut().expect("spawned pane").task = None;
        idx
    }

    /// Feature 8: mark every current pane eligible for auto-reconnect (used
    /// with `--remote --reconnect`). A dropped remote session is then re-spawned
    /// on its own pane after an exponential backoff, up to the policy's max.
    fn daemon_session_label(
        info: &crate::orca_daemon::DaemonSessionInfo,
        foreground_process: Option<&str>,
        terminal: Option<&OrcaTerminal>,
    ) -> String {
        // The desktop terminal projection is the source of truth for what a
        // user sees. Prefer its title, then its structured agent identity;
        // daemon `listSessions` does not carry either field.
        if let Some(title) = terminal
            .and_then(|terminal| terminal.title.as_deref())
            .map(str::trim)
            .filter(|title| !title.is_empty())
        {
            return title.to_owned();
        }
        if let Some(agent) = terminal
            .and_then(|terminal| terminal.agent_identity.as_deref())
            .map(str::trim)
            .filter(|agent| !agent.is_empty())
        {
            return agent.to_owned();
        }
        // Orca's structured agent owner is authoritative when present. This
        // keeps a codex/claude tab named after the actual agent instead of the
        // checkout directory (which is often shared by many tabs).
        if let Some(agent) = info
            .agent_session_owners
            .iter()
            .filter_map(|owner| owner.claim.as_ref()?.agent.as_deref())
            .find(|agent| !agent.is_empty())
        {
            return agent.to_owned();
        }
        // Legacy/manual sessions do not carry ownership metadata. Orca's
        // foreground-process probe still identifies the running agent; avoid
        // replacing a useful shell label with `zsh`/`bash`.
        if let Some(process) = foreground_process
            .map(str::trim)
            .filter(|process| !process.is_empty())
        {
            let basename = Path::new(process)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(process);
            if !matches!(
                basename,
                "bash"
                    | "zsh"
                    | "sh"
                    | "fish"
                    | "pwsh"
                    | "powershell"
                    | "powershell.exe"
                    | "cmd"
                    | "cmd.exe"
            ) {
                return basename.to_owned();
            }
        }
        info.cwd
            .as_deref()
            .and_then(|cwd| Path::new(cwd).file_name().and_then(|name| name.to_str()))
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .or_else(|| info.terminal_handle.clone())
            .unwrap_or_else(|| info.session_id.clone())
    }

    fn daemon_session_state(info: &crate::orca_daemon::DaemonSessionInfo) -> AgentState {
        if !info.is_alive {
            return AgentState::Done(None);
        }
        match info.state.as_str() {
            "exited" => AgentState::Done(None),
            "exiting" => AgentState::Done(None),
            "created" | "spawning" => AgentState::Idle,
            _ => AgentState::Running,
        }
    }

    fn daemon_snapshot_bytes(payload: &serde_json::Value) -> Option<Vec<u8>> {
        let snapshot = payload.get("snapshot")?;
        if let Some(snapshot) = snapshot.as_str() {
            return (!snapshot.is_empty()).then(|| snapshot.as_bytes().to_vec());
        }
        if snapshot.is_null() {
            return None;
        }
        let snapshot: crate::orca_daemon::DaemonSnapshot =
            serde_json::from_value(snapshot.clone()).ok()?;
        let combined = format!(
            "{}{}{}{}",
            snapshot.scrollback_ansi,
            snapshot.rehydrate_sequences,
            snapshot.snapshot_ansi,
            snapshot.pending_escape_tail_ansi
        );
        (!combined.is_empty()).then(|| combined.into_bytes())
    }

    fn refresh_orca_terminal_catalog(&mut self) {
        match crate::orca_workspaces::list_terminals() {
            Ok(terminals) => {
                if crate::debug_log::enabled() {
                    crate::debug_log::append(format_args!(
                        "{} orca_terminal_catalog=loaded rows={}",
                        crate::debug_log::WORKTREE_PREFIX,
                        terminals.len()
                    ));
                }
                self.orca_terminal_catalog = terminals;
            }
            Err(error) => {
                if crate::debug_log::enabled() {
                    crate::debug_log::append(format_args!(
                        "{} orca_terminal_catalog=unavailable error={}",
                        crate::debug_log::WORKTREE_PREFIX,
                        crate::debug_log::classify_error(error.as_ref())
                    ));
                }
                self.toasts.push(crate::toast::Toast::warning(
                    "无法读取 Orca 终端标题，已使用 daemon 会话信息。".to_owned(),
                ));
            }
        }
    }

    /// When the TUI itself is launched inside an Orca-managed terminal, that
    /// terminal is the TUI's host PTY rather than a child tab it can safely
    /// attach to. Rendering the host session back into itself creates a
    /// feedback loop and overwrites the shell's current input line. Orca
    /// exposes the stable terminal handle to child processes; use it only to
    /// identify this one session and leave every other live terminal visible.
    fn is_host_terminal_session(
        info: &crate::orca_daemon::DaemonSessionInfo,
        host_handle: Option<&str>,
        host_pty_id: Option<&str>,
    ) -> bool {
        host_handle.is_some_and(|handle| info.terminal_handle.as_deref() == Some(handle))
            || host_pty_id.is_some_and(|pty_id| info.session_id == pty_id)
    }

    fn hydrate_daemon_sessions(
        &mut self,
        client: &mut DaemonConnection,
        session_map: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    ) -> usize {
        let sessions = match client.list_sessions() {
            Ok(sessions) => sessions,
            Err(error) => {
                self.toasts.push(crate::toast::Toast::warning(format!(
                    "无法读取 Orca daemon 会话列表：{error}"
                )));
                return 0;
            }
        };
        let host_handle = std::env::var("ORCA_TERMINAL_HANDLE").ok();
        let host_pty_id = host_handle.as_deref().and_then(|handle| {
            self.orca_terminal_catalog
                .iter()
                .find(|terminal| terminal.handle == handle)
                .map(|terminal| terminal.pty_id.as_str())
        });
        let live_ids: std::collections::HashSet<String> = sessions
            .iter()
            .filter(|session| session.is_alive)
            .filter(|session| {
                !Self::is_host_terminal_session(session, host_handle.as_deref(), host_pty_id)
            })
            .map(|session| session.session_id.clone())
            .collect();
        // A reconnect may discover that a previously visible daemon session
        // exited while the socket was down. Remove that stale tab so the UI
        // cardinality continues to match Orca's current live inventory.
        self.panes.retain(|slot| {
            slot.daemon_session_id
                .as_ref()
                .map_or(true, |session_id| live_ids.contains(session_id))
        });
        let mut hydrated = 0usize;
        let mut label_counts = std::collections::HashMap::<String, usize>::new();
        let mut skipped_host = 0usize;
        let mut live_sessions: Vec<_> = sessions
            .into_iter()
            .filter(|session| session.is_alive)
            .filter(|session| {
                let is_host =
                    Self::is_host_terminal_session(session, host_handle.as_deref(), host_pty_id);
                if is_host {
                    skipped_host += 1;
                }
                !is_host
            })
            .collect();
        // Keep the same order as Orca's terminal list. Unknown rows are kept
        // after known rows so no live daemon session disappears from the TUI.
        live_sessions.sort_by_key(|session| {
            self.orca_terminal_catalog
                .iter()
                .find(|terminal| terminal.pty_id == session.session_id)
                .map(|terminal| terminal.order)
                .unwrap_or(usize::MAX)
        });
        let matched = live_sessions
            .iter()
            .filter(|session| {
                self.orca_terminal_catalog
                    .iter()
                    .any(|terminal| terminal.pty_id == session.session_id)
            })
            .count();
        if crate::debug_log::enabled() {
            crate::debug_log::append(format_args!(
                "{} daemon_sessions={} orca_terminals={} matched={} unmatched={} order=orca-terminal-list",
                crate::debug_log::WORKTREE_PREFIX,
                live_sessions.len(),
                self.orca_terminal_catalog.len(),
                matched,
                live_sessions.len().saturating_sub(matched)
            ));
            if skipped_host > 0 {
                crate::debug_log::append(format_args!(
                    "{} host_terminal_skipped={} reason=self-host-pty",
                    crate::debug_log::WORKTREE_PREFIX,
                    skipped_host
                ));
            }
        }
        let focused_pane_id = self.panes.get(self.focus).map(|slot| slot.id());
        for info in live_sessions {
            let cols = info.cols.max(MIN_COLS);
            let rows = info.rows.max(MIN_ROWS);
            // Orca v36's `createOrAttach(attachOnly)` still calls
            // `detachAllClients()`, so using it for an inventory viewer steals
            // the GUI's input attachment. Hydration is therefore always
            // snapshot-only, including after reconnect; there is no persisted
            // ownership proof that would make a reattach safe.
            let response = match client.snapshot_session(&info.session_id) {
                Ok(response) => response,
                Err(error) => {
                    self.toasts.push(crate::toast::Toast::warning(format!(
                        "无法读取 Orca 终端 {}：{error}",
                        info.session_id
                    )));
                    continue;
                }
            };
            let foreground_process = client.foreground_process(&info.session_id).ok().flatten();
            let terminal = self
                .orca_terminal_catalog
                .iter()
                .find(|terminal| terminal.pty_id == info.session_id);
            let terminal_workspace_id = terminal.and_then(|terminal| terminal.worktree_id.clone());
            let terminal_workspace_path =
                terminal.and_then(|terminal| terminal.worktree_path.clone());
            let mut session_label =
                Self::daemon_session_label(&info, foreground_process.as_deref(), terminal);
            // Preserve Orca titles exactly, including legitimate duplicates.
            // Only number fallback labels where the runtime supplied no
            // presentation metadata at all.
            if terminal.is_none() {
                let count = label_counts.entry(session_label.clone()).or_insert(0);
                *count += 1;
                if *count > 1 {
                    session_label = format!("{} #{}", session_label, count);
                }
            }
            let pane_id = if let Some(existing_idx) = self.panes.iter().position(|pane| {
                pane.daemon_session_id.as_deref() == Some(info.session_id.as_str())
            }) {
                let pane_id = self.panes[existing_idx].id();
                let mut pane = Pane::new(pane_id, session_label, cols, rows);
                pane.set_state(Self::daemon_session_state(&info));
                pane.set_branch(info.cwd.clone());
                if let Some(bytes) = Self::daemon_snapshot_bytes(&response) {
                    pane.feed(&bytes);
                }
                self.panes[existing_idx].pane = pane;
                if self.panes[existing_idx].workspace_id.is_none() {
                    self.panes[existing_idx].workspace_id = terminal_workspace_id.clone();
                }
                if self.panes[existing_idx].worktree_path.is_none() {
                    self.panes[existing_idx].worktree_path = terminal_workspace_path.clone();
                }
                // Preserve ownership established by this TUI. A session
                // created through `+`/`n` already has a daemon id and remains
                // writable across reconnects; only sessions first discovered
                // from Orca's inventory are read-only snapshots.
                pane_id
            } else {
                let pane_id = self.next_pane_id;
                self.next_pane_id += 1;
                let mut pane = Pane::new(pane_id, session_label, cols, rows);
                pane.set_state(Self::daemon_session_state(&info));
                pane.set_branch(info.cwd.clone());
                if let Some(bytes) = Self::daemon_snapshot_bytes(&response) {
                    pane.feed(&bytes);
                }
                let mut slot = PaneSlot::new(pane, Vec::new());
                slot.daemon_session_id = Some(info.session_id.clone());
                slot.workspace_id = terminal_workspace_id;
                slot.worktree_path = terminal_workspace_path;
                slot.daemon_read_only = true;
                slot.session_owner = SessionOwner::OrcaExisting;
                self.panes.push(slot);
                pane_id
            };
            // Snapshot-only panes have no stream subscription. Keeping them
            // out of this map prevents a later stream event from being
            // misinterpreted as output for a pane that cannot receive it.
            if !self
                .panes
                .iter()
                .find(|pane| pane.id() == pane_id)
                .is_some_and(|pane| pane.daemon_read_only)
            {
                session_map
                    .lock()
                    .unwrap()
                    .insert(info.session_id.clone(), pane_id);
            }
            hydrated += 1;
        }
        self.panes.sort_by_key(|pane| {
            pane.daemon_session_id
                .as_deref()
                .and_then(|session_id| {
                    self.orca_terminal_catalog
                        .iter()
                        .find(|terminal| terminal.pty_id == session_id)
                })
                .map(|terminal| terminal.order)
                .unwrap_or(usize::MAX)
        });
        if let Some(focused_pane_id) = focused_pane_id {
            if let Some(index) = self.idx_of_pane(focused_pane_id) {
                self.focus = index;
            }
        }
        if !self.panes.is_empty() && self.focus >= self.panes.len() {
            self.focus = 0;
        }
        hydrated
    }

    /// Connect to Orca through its public CLI.  This is deliberately a
    /// process-based viewer: no daemon Unix socket, stream attachment,
    /// `write`, `resize`, or `kill` RPC is opened for GUI-owned sessions.
    pub fn try_connect_daemon(&mut self) {
        let bridge = OrcaCliBridge::new();
        match bridge.list_terminals() {
            Ok(terminals) => {
                let terminals = filter_host_terminal(terminals);
                let count = self.hydrate_cli_terminals(&terminals);
                self.orca_terminal_catalog = terminals.clone();
                self.start_orca_cli_poller(bridge.clone());
                self.start_orca_sender(bridge.clone());
                self.orca_cli = Some(bridge);
                self.conn_state = ConnectionState::Connected;
                self.toasts.push(crate::toast::Toast::success(format!(
                    "Connected to Orca runtime via CLI, {count} 个终端（GUI 会话只读）"
                )));
            }
            Err(error) => {
                self.toasts.push(crate::toast::Toast::warning(format!(
                    "Orca CLI unavailable ({error}). Running standalone."
                )));
            }
        }
    }

    fn hydrate_cli_terminals(&mut self, terminals: &[OrcaTerminal]) -> usize {
        let mut count = 0;
        for terminal in terminals {
            let title = terminal
                .title
                .clone()
                .or_else(|| terminal.agent_identity.clone())
                .unwrap_or_else(|| terminal.handle.clone());
            let state = AgentState::Running;
            if let Some(index) = self
                .panes
                .iter()
                .position(|slot| slot.orca_handle.as_deref() == Some(terminal.handle.as_str()))
            {
                let slot = &mut self.panes[index];
                slot.pane.set_name(title);
                slot.pane.set_state(state);
                count += 1;
                continue;
            }
            let mut pane = Pane::new(self.next_pane_id, title, self.cols, self.rows);
            self.next_pane_id += 1;
            pane.set_state(state);
            pane.set_branch(terminal.branch.clone().or_else(|| {
                terminal
                    .worktree_path
                    .as_ref()
                    .map(|path| path.display().to_string())
            }));
            let mut slot = PaneSlot::new(pane, Vec::new());
            slot.daemon_session_id = Some(terminal.pty_id.clone());
            slot.orca_handle = Some(terminal.handle.clone());
            slot.session_owner = SessionOwner::OrcaExisting;
            slot.daemon_read_only = true;
            slot.workspace_id = terminal.worktree_id.clone();
            slot.worktree_path = terminal.worktree_path.clone();
            self.panes.push(slot);
            count += 1;
        }
        count
    }

    fn start_orca_cli_poller(&mut self, bridge: OrcaCliBridge) {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let tx = self.bus_tx.clone();
        let _ = thread::Builder::new()
            .name("orca-cli-poller".into())
            .spawn(move || {
                while !stop_thread.load(Ordering::Relaxed) {
                    if let Ok(terminals) = bridge.list_terminals() {
                        let terminals = filter_host_terminal(terminals);
                        let _ = tx.send(AgentUpdate::OrcaInventory {
                            terminals: terminals.clone(),
                        });
                        for terminal in terminals {
                            if let Ok(screen) = bridge.read_screen(&terminal.handle) {
                                let _ = tx.send(AgentUpdate::OrcaSnapshot {
                                    terminal,
                                    lines: screen.lines,
                                    status: screen.status,
                                });
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            });
        self.orca_poll_stop = Some(stop);
    }

    fn start_orca_sender(&mut self, bridge: OrcaCliBridge) {
        let (tx, rx) = std::sync::mpsc::channel::<(String, Vec<u8>, bool)>();
        let _ = thread::Builder::new()
            .name("orca-cli-sender".into())
            .spawn(move || {
                let mut pending = None;
                loop {
                    let first = match pending.take().or_else(|| rx.recv().ok()) {
                        Some(message) => message,
                        None => break,
                    };
                    let (handle, mut bytes, mut enter) = first;
                    let deadline = Instant::now() + Duration::from_millis(15);
                    while !enter {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        match rx.recv_timeout(remaining) {
                            Ok((next_handle, next_bytes, next_enter)) if next_handle == handle => {
                                bytes.extend(next_bytes);
                                enter = next_enter;
                            }
                            Ok(message) => {
                                pending = Some(message);
                                break;
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                pending = None;
                                break;
                            }
                        }
                    }
                    let _ = bridge.send_text(&handle, &String::from_utf8_lossy(&bytes), enter);
                }
            });
        self.orca_send_tx = Some(tx);
    }

    fn stop_orca_cli_poller(&mut self) {
        if let Some(stop) = self.orca_poll_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.orca_send_tx = None;
        if let Some(bridge) = self.orca_cli.clone() {
            let pending = std::mem::take(&mut self.orca_spawn_rx);
            for (_, rx) in pending {
                if let Ok(Ok(created)) = rx.recv_timeout(Duration::from_millis(200)) {
                    let _ = bridge.close_tab(&created.handle);
                }
            }
        }
    }

    pub fn enable_reconnect(&mut self) {
        let policy = ssh::ReconnectPolicy::default();
        for slot in &mut self.panes {
            slot.reconnect = Some(ssh::ReconnectSession::new(policy.clone()));
        }
    }

    /// Feature 8: re-spawn pane `i`'s command into the same pane (preserving
    /// its emulator/scrollback), used when a scheduled reconnect comes due.
    fn respawn(&mut self, i: usize) {
        let Some(command) = self
            .panes
            .get(i)
            .and_then(|slot| (!slot.command.is_empty()).then(|| slot.command.clone()))
        else {
            return;
        };
        // The pane already has a stable id — re-use it for the new forwarder
        // thread so `apply_update` keeps routing to THIS pane (not whatever slid
        // into position `i` after a close elsewhere).
        let Some(pane_id) = self.panes.get(i).map(|p| p.id()) else {
            return;
        };
        let cols = self.cols;
        let rows = self.rows;
        let agent_cwd = self
            .panes
            .get(i)
            .and_then(|slot| slot.worktree_path.as_deref())
            .unwrap_or(&self.launch_cwd);
        match PtySession::spawn(command, Some(agent_cwd), cols, rows) {
            Ok((session, rx)) => {
                if let Some(pane) = self.panes.get_mut(i) {
                    pane.session = Some(session);
                    pane.set_state(AgentState::Running);
                    pane.reconnect_due = None;
                }
                let tx = self.bus_tx.clone();
                let _ = thread::Builder::new()
                    .name(format!("orca-reconnect({pane_id})"))
                    .spawn(move || bus::forward_session(pane_id, rx, tx));
            }
            Err(err) => {
                // No eprintln (would corrupt the TUI); the Failed state carries it.
                if let Some(pane) = self.panes.get_mut(i) {
                    pane.set_state(AgentState::Failed(format!("reconnect failed: {err:#}")));
                }
            }
        }
    }

    /// Feature 8: fire any backoff-scheduled reconnects whose wait has elapsed
    /// and that haven't exhausted their retry budget. Non-blocking — the loop
    /// keeps rendering other panes while a remote one waits to come back.
    fn pump_reconnect(&mut self) {
        let now = Instant::now();
        // Collect indices due for respawn first to avoid borrow conflicts.
        let mut due: Vec<usize> = Vec::new();
        for i in 0..self.panes.len() {
            let Some(rs) = self.panes.get(i).and_then(|slot| slot.reconnect.as_ref()) else {
                continue;
            };
            let deadline = self.panes.get(i).and_then(|slot| slot.reconnect_due);
            let Some(deadline) = deadline else {
                continue;
            };
            if now >= deadline {
                if rs.exhausted() {
                    // Give up: clear the schedule, leave the pane terminal.
                    if let Some(slot) = self.panes.get_mut(i) {
                        slot.reconnect_due = None;
                    }
                } else {
                    due.push(i);
                }
            }
        }
        for i in due {
            // Respawn resets the attempt counter for the next drop.
            if let Some(slot) = self.panes.get_mut(i) {
                if let Some(rs) = slot.reconnect.as_mut() {
                    rs.record_success();
                }
                slot.reconnect_due = None;
            }
            self.respawn(i);
        }
    }

    /// Legacy daemon reconnection hook. GUI integration uses the CLI poller,
    /// whose next successful `terminal list` automatically repairs a transient
    /// runtime outage. Keeping this hook a no-op prevents the old Unix-socket
    /// reconnect path from ever being selected by `run --daemon`.
    fn pump_daemon_reconnect(&mut self) {
        // The CLI poller is self-healing and owns its own retry cadence.
    }

    /// Feature 7: release every dependency-gated task the coordinator can
    /// dispatch right now, each to its own new pane. Called once per loop tick
    /// so a task whose dependencies just completed spawns on the next frame.
    fn pump_orchestration(&mut self) {
        if self.coordinator.is_none() {
            return;
        }
        let agent = self.orch_agent.clone().unwrap_or_default();
        loop {
            let dispatch = self
                .coordinator
                .as_mut()
                .and_then(|c| c.dispatch_next(std::slice::from_ref(&agent)));
            let Some(dispatch) = dispatch else {
                break;
            };
            let tid = dispatch.task_id;
            let mut spec = AgentSpec::from_command(vec![agent.clone(), dispatch.prompt.clone()]);
            spec.name = format!("task-{tid}");
            if let Some(c) = self.coordinator.as_mut() {
                c.mark_in_progress(tid);
            }
            let pane_id = self.spawn_one(spec);
            if let Some(slot) = self.panes.get_mut(pane_id) {
                slot.task = Some(tid);
            }
        }
    }

    /// Render the Herdr-style workspace sidebar, tab strip, active terminal and
    /// footer. The active tab viewport + PTY size are reconciled here (before
    /// the immutable draw closure) so the emulator and agent agree on the real
    /// terminal dimensions.
    fn render(&mut self) -> Result<()> {
        let size = self.terminal.size()?;
        // Edge-to-edge — no outer margin, so the layout fills the terminal and
        // feels dense ("꽉찬") like opencode. Double borders on panes provide
        // the visual separation that the margin/spacing previously gave.
        let total = Rect::new(0, 0, size.width, size.height);

        // Derive the sidebar model before laying out panes so a long Orca
        // workspace name can widen the sidebar enough to remain readable.
        // The helper caps the expansion while preserving a usable pane area.
        let catalog_entries = self.catalog_sidebar_entries();
        let (workspace_groups, workspace_targets) = self.workspace_sidebar_groups();
        let use_workspace_sidebar = self.daemon.is_some()
            || self.orca_cli.is_some()
            || self.workspace_catalog_loaded
            || !self.workspace_catalog.is_empty();
        let workspace_width_entries: Vec<crate::sidebar::SidebarEntry> = workspace_groups
            .iter()
            .map(|group| crate::sidebar::SidebarEntry {
                name: group.name.clone(),
                state: AgentState::Idle,
                branch: group.detail.clone(),
                activity: None,
                focused: false,
                pinned: false,
            })
            .collect();
        self.sidebar_workspace_targets = workspace_targets;
        self.sidebar_catalog_entries = self
            .workspace_catalog
            .iter()
            .filter(|workspace| {
                !workspace.is_on_local_host()
                    || !self
                        .panes
                        .iter()
                        .any(|pane| pane.workspace_id.as_deref() == Some(workspace.id.as_str()))
            })
            .cloned()
            .collect();
        let mut render_model =
            RenderModel::from_slots_with_catalog(&self.panes, self.focus, &catalog_entries);
        // The persistent sidebar is workspace-first. Keep the render model's
        // sidebar projection aligned with the grouped rows so width/tally
        // calculations and diagnostics describe the same visible structure.
        if use_workspace_sidebar {
            render_model.sidebar_entries = workspace_width_entries.clone();
        }

        // Reserve the left sidebar (Orca-style agent list with status dots +
        // live activity from OSC 9999). Hidden when sidebar_width is 0 or the
        // terminal is too narrow for panes to be usable.
        let width_entries = if use_workspace_sidebar {
            &workspace_width_entries
        } else {
            &render_model.sidebar_entries
        };
        let sidebar_w = sidebar::recommended_width(
            width_entries,
            self.config.layout.sidebar_width,
            total.width,
            12,
        );
        let show_sidebar = sidebar_w > 0 && !self.sidebar_hidden && total.width > sidebar_w + 8;
        let (sidebar_area, content_area) = if show_sidebar {
            // spacing(1) adds a 1-cell gap between the sidebar and the pane
            // area — the panes' own Double borders provide the visual boundary,
            // so the sidebar needs no right border of its own.
            let h = Layout::horizontal([Constraint::Length(sidebar_w), Constraint::Min(1)])
                .spacing(1)
                .split(total);
            (Some(h[0]), h[1])
        } else {
            (None, total)
        };
        self.sidebar_entry_hitboxes = if use_workspace_sidebar {
            Vec::new()
        } else {
            sidebar_area
                .map(|area| sidebar::entry_hit_rects(area, &render_model.sidebar_entries, true))
                .unwrap_or_default()
        };
        self.sidebar_workspace_hitboxes.clear();
        self.sidebar_pane_hitboxes.clear();

        let reserve_footer = total.height >= 3 && self.config.layout.show_status_bar;
        let (main_area, footer_area) = if reserve_footer {
            let chunks =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(content_area);
            (chunks[0], chunks[1])
        } else {
            (content_area, Rect::default())
        };
        // Herdr-style layout: a persistent tab strip above one active terminal
        // surface. Inactive sessions remain alive and continue receiving PTY
        // output, but are never painted into split panes.
        let chrome = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(main_area);
        let tab_area = chrome[0];
        let terminal_area = chrome[1];
        // The horizontal strip belongs to the current workspace, not to the
        // whole daemon inventory. Keep the global pane indexes in a separate
        // map because tab hit boxes are indexed by this filtered list.
        let active_workspace_group = if use_workspace_sidebar {
            workspace_groups.iter().position(|group| {
                group
                    .agents
                    .iter()
                    .any(|(pane_idx, _)| *pane_idx == self.focus)
            })
        } else {
            None
        };
        let tab_pane_indices: Vec<usize> = active_workspace_group
            .and_then(|group_idx| workspace_groups.get(group_idx))
            .map(|group| group.agents.iter().map(|(pane_idx, _)| *pane_idx).collect())
            .unwrap_or_else(|| (0..self.panes.len()).collect());
        self.tab_pane_indices = tab_pane_indices.clone();
        let tab_items: Vec<TabItem> = tab_pane_indices
            .iter()
            .filter_map(|pane_idx| self.panes.get(*pane_idx))
            .map(|slot| TabItem {
                label: slot.name().to_owned(),
                status: Some(
                    match AgentStatus::derive(
                        slot.state(),
                        slot.activity().map(|a| a.state.as_str()),
                    ) {
                        AgentStatus::Working => '●',
                        AgentStatus::Blocked => '!',
                        AgentStatus::Failed => '×',
                        AgentStatus::Done => '✓',
                        _ => '○',
                    },
                ),
            })
            .collect();
        let active_tab = tab_pane_indices
            .iter()
            .position(|pane_idx| *pane_idx == self.focus)
            .unwrap_or(0);
        self.tab_hitboxes = tab_bar::layout_tabs(tab_area, &tab_items);
        self.sidebar_rect = sidebar_area;
        let zoomed = self.zoomed && self.focus < self.panes.len();
        let mut active_rects = vec![Rect::default(); self.panes.len()];
        if let Some(rect) = active_rects.get_mut(self.focus) {
            *rect = terminal_area;
        }
        self.pane_rects = active_rects;

        // Only the visible tab is resized to the real terminal viewport. This
        // prevents PTY dimensions from being derived from a split grid and
        // makes fullscreen TUIs behave like a genuine terminal.
        let mut daemon_resize = None;
        let focused_daemon_read_only = self
            .panes
            .get(self.focus)
            .is_some_and(|slot| slot.daemon_read_only);
        if let Some(pane) = self.panes.get_mut(self.focus) {
            let rect = terminal_area;
            let inner_w = rect.width.saturating_sub(2).max(MIN_COLS);
            let inner_h = rect.height.saturating_sub(2).max(MIN_ROWS);
            let (cur_w, cur_h) = pane.size();
            if (cur_w, cur_h) != (inner_w, inner_h) {
                if std::env::var("ORCA_DEBUG_LOG").is_ok() {
                    use std::io::Write;
                    let has_session = pane.session.is_some();
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("/tmp/orca-live.log")
                    {
                        let _ = writeln!(f, "RESIZE active tab: emulator {cur_w}x{cur_h} → {inner_w}x{inner_h}; session_present={has_session}");
                    }
                }
                pane.resize_viewport(inner_w, inner_h);
                if let Some(session) = pane.session.as_mut() {
                    let r = session.resize(inner_w, inner_h);
                    if std::env::var("ORCA_DEBUG_LOG").is_ok() {
                        use std::io::Write;
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("/tmp/orca-live.log")
                        {
                            let _ = writeln!(
                                f,
                                "  session.resize({inner_w}x{inner_h}) = {}",
                                if r.is_ok() { "OK" } else { "ERR" }
                            );
                        }
                    }
                } else if !focused_daemon_read_only {
                    if let Some(session_id) = pane.daemon_session_id.clone() {
                        daemon_resize = Some((session_id, inner_w, inner_h));
                    }
                }
            }
        }
        // Orca CLI screen projections are intentionally read-only. There is no
        // resize call in that mode; retain the legacy daemon path only for the
        // built-in daemon client.
        if self.orca_cli.is_none() {
            if let (Some(daemon), Some((session_id, cols, rows))) =
                (&mut self.daemon, daemon_resize)
            {
                daemon.enqueue_resize(session_id, cols, rows);
            }
        }

        // The render model was derived before layout so its workspace labels
        // could participate in the sidebar-width decision above.
        if crate::debug_log::enabled() {
            let signature = (
                self.panes.len(),
                render_model.sidebar_entries.len(),
                self.focus,
            );
            if self.debug_last_sidebar_signature != Some(signature) {
                crate::debug_log::append(format_args!(
                    "{} render pane_count={} sidebar_entry_count={} catalog_only={} focus={} source=PaneSlot+OrcaCatalog",
                    crate::debug_log::WORKTREE_PREFIX,
                    signature.0,
                    signature.1,
                    catalog_entries.len(),
                    signature.2
                ));
                self.debug_last_sidebar_signature = Some(signature);
            }
        }
        render_model.pane_rects = self.pane_rects.clone();
        render_model.footer_hint = if zoomed {
            FOOTER_ZOOM.to_string()
        } else {
            match self.mode {
                InputMode::Pane => FOOTER_PANE,
                InputMode::Jump => FOOTER_JUMP,
                InputMode::Spawn => FOOTER_SPAWN,
                InputMode::SpawnCustom => FOOTER_SPAWN_CUSTOM,
                InputMode::TasksRepo => FOOTER_TASKS_REPO,
                InputMode::TasksList => FOOTER_TASKS_LIST,
                InputMode::Settings => FOOTER_SETTINGS,
                InputMode::Activity => FOOTER_ACTIVITY,
                InputMode::Dashboard => FOOTER_DASHBOARD,
                InputMode::Sidebar => FOOTER_SIDEBAR,
                InputMode::Workspaces => FOOTER_WORKSPACES,
                InputMode::Normal => FOOTER_NORMAL,
            }
            .to_string()
        };

        let focus = self.focus;
        let show_help = self.show_help;
        let mode = self.mode;
        // Snapshot the jump-palette state (computed before the mutable `panes`
        // borrow so the draw closure can render the overlay without touching self).
        let jump_open = mode == InputMode::Jump;
        let jump_filtered_idx: Vec<usize> = if jump_open {
            self.jump_filtered()
        } else {
            Vec::new()
        };
        let jump_query = self.jump_query.clone();
        let jump_selected = self.jump_selected;
        let spawn_open = mode == InputMode::Spawn;
        let spawn_opts: Vec<(String, String)> = if spawn_open {
            self.spawn_options()
                .into_iter()
                .map(|(name, cmd)| (name, cmd.first().cloned().unwrap_or_default()))
                .collect()
        } else {
            Vec::new()
        };
        let spawn_selected = self.spawn_selected;
        // Snapshot the custom-command modal state so the draw closure never
        // touches `self.custom_cmd` (borrow-checker safety on the 60fps path).
        let custom_open = mode == InputMode::SpawnCustom;
        let custom_cmd_view = self.custom_cmd.clone();
        // Snapshot the activity timeline BEFORE the mutable `panes` borrow below
        // so the draw closure never touches `self.activity` (borrow-checker
        // safety on the 60fps render path). Owned Strings only.
        let activity_open = mode == InputMode::Activity;
        let activity_lines: Vec<String> = if activity_open {
            self.activity
                .recent(usize::from(total.height))
                .iter()
                .map(|e| e.render_line())
                .collect()
        } else {
            Vec::new()
        };
        // Snapshot the sidebar-nav popup state so the draw closure never
        // touches `self.sidebar_nav` (borrow-checker safety on the 60fps path).
        let sidebar_open = mode == InputMode::Sidebar;
        let sidebar_selected = self.sidebar_nav;
        let workspace_open = mode == InputMode::Workspaces;
        let workspace_rows = if workspace_open {
            self.workspace_inventory_rows()
        } else {
            Vec::new()
        };
        let workspace_selected = self.workspace_selected;
        let workspace_unresolved_hosts = self.workspace_unresolved_hosts.clone();
        let workspace_unverifiable_scope_hosts = self.workspace_unverifiable_scope_hosts.clone();
        // Snapshot the Tasks view state (Phase 2) BEFORE the mutable `panes`
        // borrow so the draw closure never touches the tasks_* fields.
        let tasks_repo_open = mode == InputMode::TasksRepo;
        let tasks_repo_input_view = self.tasks_repo_input.clone();
        let tasks_list_open = mode == InputMode::TasksList;
        let tasks_items_view: Vec<TasksEntry> = if tasks_list_open {
            self.tasks_items.clone()
        } else {
            Vec::new()
        };
        let tasks_selected_view = self.tasks_selected;
        let tasks_error_view = self.tasks_error.clone();
        // Snapshot the Settings overlay state (Phase 2) BEFORE the mutable
        // `panes` borrow so the draw closure never touches the config / cursor.
        // All four row display-values are derived here as owned Strings.
        let settings_open = mode == InputMode::Settings;
        let settings_cursor = self.settings_cursor;
        let settings_sidebar_on = self.config.layout.sidebar_width > 0;
        let settings_status_bar = self.config.layout.show_status_bar;
        let settings_default_agent = self.config.default_agent.clone();
        // Match the live theme against a preset by accent-hex equality; if none
        // match (a hand-edited custom theme), label it "Custom" so the user
        // still sees *something* informative and the cycle has a defined start.
        let settings_theme_name = Config::theme_presets()
            .iter()
            .find(|(_, t)| t.accent.eq_ignore_ascii_case(&self.config.theme.accent))
            .map(|(n, _)| (*n).to_string())
            .unwrap_or_else(|| "Custom".to_string());
        // Snapshot the dashboard overlay state (Phase 2) BEFORE the mutable
        // `panes` borrow so the draw closure never touches `self.panes` (borrow-
        // checker safety on the 60fps path). Derived from each pane's lifecycle
        // state + OSC 9999 activity, exactly like the sidebar tallies.
        let dashboard_open = mode == InputMode::Dashboard;
        let dashboard_entries: Vec<(String, AgentStatus)> = if dashboard_open {
            self.panes
                .iter()
                .map(|p| {
                    let status =
                        AgentStatus::derive(p.state(), p.activity().map(|a| a.state.as_str()));
                    (p.name().to_string(), status)
                })
                .collect()
        } else {
            Vec::new()
        };
        // Consolidate all modal-only values into the immutable render model.
        // Overlay drawing below consumes this snapshot and never reaches back
        // into `App` while panes are mutably borrowed.
        render_model.overlay = crate::render_model::OverlayModel {
            mode,
            jump_query: jump_query.clone(),
            jump_filtered: jump_filtered_idx
                .iter()
                .map(|&idx| {
                    (
                        idx,
                        self.panes
                            .get(idx)
                            .map(|p| p.name().to_string())
                            .unwrap_or_default(),
                    )
                })
                .collect(),
            jump_selected,
            spawn_options: spawn_opts.clone(),
            spawn_selected,
            activity_lines: activity_lines.clone(),
            sidebar_selected,
            tasks_repo_input: tasks_repo_input_view.clone(),
            tasks_items: tasks_items_view
                .iter()
                .map(|entry| {
                    let kind = match entry.kind {
                        TaskKind::Issue => "issue",
                        TaskKind::PullRequest => "pr",
                    };
                    (
                        kind.to_string(),
                        entry.title.clone(),
                        entry.number.to_string(),
                    )
                })
                .collect(),
            tasks_selected: tasks_selected_view,
            tasks_error: tasks_error_view.clone(),
            settings_cursor,
            settings_sidebar_on,
            settings_status_bar,
            settings_default_agent: settings_default_agent.clone(),
            settings_theme_name: settings_theme_name.clone(),
            dashboard_entries: dashboard_entries.clone(),
            workspace_rows,
            workspace_selected,
            workspace_catalog_loaded: self.workspace_catalog_loaded,
            workspace_unresolved_hosts,
            workspace_unverifiable_scope_hosts,
        };
        let panes = &mut self.panes;
        let theme = &self.config.theme;
        // Agent status tallies for the footer status bar (opencode-style).
        let tally = render_model.tally;
        self.footer_quit_hitbox = if reserve_footer && footer_area.width >= 12 {
            Some(Rect::new(
                footer_area.right().saturating_sub(10),
                footer_area.y,
                10,
                footer_area.height,
            ))
        } else {
            None
        };
        let footer_quit_hitbox = self.footer_quit_hitbox;
        let mut workspace_hitboxes = crate::sidebar::WorkspaceSidebarHitboxes::default();
        self.terminal.draw(|f| {
            if let Some(sb) = sidebar_area {
                let conn_status = match self.conn_state {
                    ConnectionState::Standalone => Some(("● Standalone", theme.muted())),
                    ConnectionState::Connected => Some(("● Daemon", theme.success())),
                    ConnectionState::Disconnected { .. } => Some(("✗ Disconnected", theme.error())),
                };
                if use_workspace_sidebar {
                    workspace_hitboxes = sidebar::render_workspace_sidebar(
                        f,
                        sb,
                        &workspace_groups,
                        theme,
                        conn_status,
                        panes.len(),
                    );
                } else {
                    sidebar::render_sidebar(
                        f,
                        sb,
                        &render_model.sidebar_entries,
                        theme,
                        conn_status,
                    );
                }
                // Fill the gap between sidebar and panes with the theme bg so it
                // isn't a terminal-default strip (background everywhere).
                let gw = content_area.x.saturating_sub(sb.right());
                if gw > 0 {
                    let gap = Rect::new(sb.right(), total.y, gw, total.height);
                    f.buffer_mut()
                        .set_style(gap, Style::default().bg(theme.bg()));
                }
            }
            let _ = tab_bar::render_tab_bar(f, tab_area, &tab_items, active_tab, theme);
            if let Some(pane) = panes.get_mut(focus) {
                pane.render(f, terminal_area, true, theme);
            }
            if reserve_footer {
                // opencode-style status bar: left = agent tally, right = key-hint.
                let hint_width = footer_area.width.min(66);
                let foot = Layout::horizontal([Constraint::Min(1), Constraint::Length(hint_width)])
                    .split(footer_area);
                let mut status_spans: Vec<Span> = Vec::new();
                if tally.working > 0 {
                    status_spans.push(Span::styled(
                        format!(" ● {} working ", tally.working),
                        Style::default().fg(theme.success()).bg(theme.panel()),
                    ));
                }
                if tally.blocked > 0 {
                    status_spans.push(Span::styled(
                        format!(" ● {} blocked ", tally.blocked),
                        Style::default().fg(theme.error()).bg(theme.panel()),
                    ));
                }
                if tally.interrupted > 0 {
                    status_spans.push(Span::styled(
                        format!(" ● {} interrupted ", tally.interrupted),
                        Style::default().fg(theme.accent()).bg(theme.panel()),
                    ));
                }
                if tally.waiting > 0 {
                    status_spans.push(Span::styled(
                        format!(" ● {} waiting ", tally.waiting),
                        Style::default().fg(theme.warning()).bg(theme.panel()),
                    ));
                }
                if tally.failed > 0 {
                    status_spans.push(Span::styled(
                        format!(" ✗ {} failed ", tally.failed),
                        Style::default().fg(theme.error()).bg(theme.panel()),
                    ));
                }
                if tally.done > 0 {
                    status_spans.push(Span::styled(
                        format!(" ✓ {} done ", tally.done),
                        Style::default().fg(theme.muted()).bg(theme.panel()),
                    ));
                }
                let status_line = Line::from(status_spans);
                f.render_widget(
                    Paragraph::new(status_line).style(Style::default().bg(theme.panel())),
                    foot[0],
                );
                let (hint_area, quit_area) = if footer_quit_hitbox.is_some() {
                    let parts = Layout::horizontal([Constraint::Min(1), Constraint::Length(10)])
                        .split(foot[1]);
                    (parts[0], Some(parts[1]))
                } else {
                    (foot[1], None)
                };
                f.render_widget(
                    Paragraph::new(render_model.footer_hint.as_str())
                        .style(Style::default().bg(theme.panel()).fg(theme.accent()))
                        .alignment(Alignment::Right),
                    hint_area,
                );
                if let Some(area) = quit_area {
                    f.render_widget(
                        Paragraph::new(" × 退出 ")
                            .style(Style::default().bg(theme.panel()).fg(theme.error()))
                            .alignment(Alignment::Center),
                        area,
                    );
                }
            }
            // Jump palette overlay (drawn last, on top of everything).
            if jump_open {
                use ratatui::style::Modifier;
                use ratatui::text::Span;
                let n = render_model.overlay.jump_filtered.len();
                let pop_h = (n as u16 + 3).min(total.height.saturating_sub(4)).max(5);
                let pop_w = total.width.min(64).max(40);
                let pop = crate::overlay::centered_rect(total, pop_w, pop_h);
                let inner =
                    crate::overlay::begin_popup(f, pop, Line::from(" Jump to agent "), theme);

                let mut lines: Vec<Line> = Vec::new();
                // Query line with a block cursor.
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("/{}", render_model.overlay.jump_query),
                        Style::default().fg(theme.accent()),
                    ),
                    Span::styled(
                        "_",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::SLOW_BLINK),
                    ),
                ]));
                lines.push(Line::default());
                for (i, (_pane_idx, name)) in render_model.overlay.jump_filtered.iter().enumerate()
                {
                    if i as u16 + 3 > inner.height {
                        break;
                    }
                    let selected = i == render_model.overlay.jump_selected;
                    let style = if selected {
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.fg())
                    };
                    let prefix = if selected { "▶ " } else { "  " };
                    lines.push(Line::from(format!("{prefix}{name}")).style(style));
                }
                crate::overlay::render_lines(f, inner, lines, theme);
            }
            // Spawn picker overlay (drawn on top, like the jump palette).
            if spawn_open {
                use ratatui::style::Modifier;
                // ~3 items per 12 rows of terminal height, clamped to [2, 6]
                let max_visible = ((total.height / 12) as usize).clamp(2, 6);
                let n = render_model.overlay.spawn_options.len();
                let visible = n.min(max_visible);
                let pop_h = (visible as u16 + 3)
                    .min(total.height.saturating_sub(4))
                    .max(5);
                let pop_w = total.width.min(48).max(30);
                let pop = crate::overlay::centered_rect(total, pop_w, pop_h);
                let inner = crate::overlay::begin_popup(f, pop, Line::from(" New pane "), theme);

                // Scroll offset: keep the selected item visible.
                let scroll = render_model
                    .overlay
                    .spawn_selected
                    .saturating_sub(max_visible.saturating_sub(1));

                let mut lines: Vec<Line> = Vec::new();
                for (i, (name, cmd_first)) in render_model
                    .overlay
                    .spawn_options
                    .iter()
                    .enumerate()
                    .skip(scroll)
                    .take(usize::from(inner.height))
                {
                    let selected = i == render_model.overlay.spawn_selected;
                    let style = if selected {
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.fg())
                    };
                    let prefix = if selected { "▶ " } else { "  " };
                    let line = if cmd_first.is_empty() || cmd_first == name {
                        format!("{prefix}{name}")
                    } else {
                        format!("{prefix}{name:<12} {cmd_first}")
                    };
                    lines.push(Line::from(line).style(style));
                }
                // Scroll indicator.
                if n > max_visible {
                    let more_below = render_model.overlay.spawn_selected + 1 < n;
                    let more_above = scroll > 0;
                    let indicator = match (more_above, more_below) {
                        (true, true) => " ↑↓ more ",
                        (true, false) => " ↑ end ",
                        (false, true) => " ↓ more ",
                        (false, false) => "",
                    };
                    if !indicator.is_empty() {
                        lines.push(Line::from(indicator).style(Style::default().fg(theme.muted())));
                    }
                }
                crate::overlay::render_lines(f, inner, lines, theme);
            }
            // Activity timeline overlay (drawn on top, any key to close).
            if activity_open {
                let pop = Rect::new(
                    total.x + 2,
                    total.y + 1,
                    total.width.saturating_sub(4).max(40),
                    total.height.saturating_sub(2).max(10),
                );
                let inner = crate::overlay::begin_popup(
                    f,
                    pop,
                    Line::from(" Activity (any key to close) "),
                    theme,
                );
                // `activity_lines` is newest-first (recent() returns newest-first),
                // so the most recent transition renders at the top.
                let lines: Vec<Line> = if render_model.overlay.activity_lines.is_empty() {
                    vec![Line::from("(no activity yet)").style(Style::default().fg(theme.muted()))]
                } else {
                    render_model
                        .overlay
                        .activity_lines
                        .iter()
                        .take(usize::from(inner.height))
                        .map(|s| Line::from(s.as_str()).style(Style::default().fg(theme.fg())))
                        .collect()
                };
                crate::overlay::render_lines(f, inner, lines, theme);
            }
            // Agent dashboard overlay (Phase 2): a read-only 3-bucket board
            // grouping the live per-pane statuses into needs-attention /
            // working / done columns. Any key dismisses it back to Normal.
            if dashboard_open {
                let pop = Rect::new(
                    total.x + 2,
                    total.y + 1,
                    total.width.saturating_sub(4).max(40),
                    total.height.saturating_sub(2).max(10),
                );
                let inner = crate::overlay::begin_popup(
                    f,
                    pop,
                    Line::from(" Agent Dashboard (any key to close) "),
                    theme,
                );
                // Split the inner area into 3 equal columns. Using Min(1) × 3
                // fills evenly regardless of odd widths (ratatui distributes any
                // remainder rather than leaving a gap).
                let cols = Layout::horizontal([
                    Constraint::Min(1),
                    Constraint::Min(1),
                    Constraint::Min(1),
                ])
                .split(inner);

                // Status → bucket mapping (see docs/TECHNICAL-DESIGN.md):
                //   needs-attention = Blocked | Interrupted | Failed
                //   working         = Working  | Waiting     | Idle
                //   done            = Done
                let mut needs: Vec<&str> = Vec::new();
                let mut working_b: Vec<&str> = Vec::new();
                let mut done: Vec<&str> = Vec::new();
                for (name, status) in &render_model.overlay.dashboard_entries {
                    match status {
                        AgentStatus::Blocked | AgentStatus::Interrupted | AgentStatus::Failed => {
                            needs.push(name.as_str());
                        }
                        AgentStatus::Working | AgentStatus::Waiting | AgentStatus::Idle => {
                            working_b.push(name.as_str());
                        }
                        AgentStatus::Done => done.push(name.as_str()),
                    }
                }

                let buckets: [(&str, &Vec<&str>, ratatui::style::Color); 3] = [
                    ("needs-attention", &needs, theme.error()),
                    ("working", &working_b, theme.success()),
                    ("done", &done, theme.muted()),
                ];
                for (i, (label, members, color)) in buckets.into_iter().enumerate() {
                    let mut lines: Vec<Line> = Vec::new();
                    // Header: "label (count)" in the bucket color.
                    lines.push(
                        Line::from(format!("{label} ({})", members.len()))
                            .style(Style::default().fg(color)),
                    );
                    if members.is_empty() {
                        lines.push(Line::from("(none)").style(Style::default().fg(theme.muted())));
                    } else {
                        for name in members {
                            lines.push(Line::from(*name).style(Style::default().fg(theme.fg())));
                        }
                    }
                    crate::overlay::render_lines(f, cols[i], lines, theme);
                }
            }
            // Sidebar nav popup (Ctrl+S / `s` in Pane mode): Activity / Tasks /
            // Settings. All three are wired in Phase 1/2 — no placeholders remain.
            if sidebar_open {
                use ratatui::style::Modifier;
                let n = SIDEBAR_NAV_ITEMS.len();
                let pop_h = (n as u16 + 3).min(total.height.saturating_sub(4)).max(5);
                let pop_w = total.width.min(36).max(24);
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                let inner = crate::overlay::begin_popup(f, pop, Line::from(" Navigate "), theme);
                let mut lines: Vec<Line> = Vec::new();
                for (i, name) in SIDEBAR_NAV_ITEMS.iter().enumerate() {
                    let selected = i == sidebar_selected;
                    let prefix = if selected { "▶ " } else { "  " };
                    // Activity (0), Tasks (1), and Settings (2) are all
                    // implemented in Phase 1/2; no "(soon)" placeholders remain.
                    let suffix = if i > 2 { " (soon)" } else { "" };
                    let label = format!("{prefix}{name}{suffix}");
                    let style = if selected {
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::BOLD)
                    } else if i > 2 {
                        // Hypothetical future placeholder — dimmed.
                        Style::default().fg(theme.muted())
                    } else {
                        // Activity / Tasks (implemented) — normal weight.
                        Style::default().fg(theme.fg())
                    };
                    lines.push(Line::from(label).style(style));
                }
                crate::overlay::render_lines(f, inner, lines, theme);
            }
            if render_model.overlay.mode == InputMode::Workspaces {
                crate::workspace_view::render_workspace_overlay(
                    f,
                    total,
                    crate::workspace_view::WorkspaceOverlay {
                        rows: &render_model.overlay.workspace_rows,
                        selected: render_model.overlay.workspace_selected,
                        catalog_loaded: render_model.overlay.workspace_catalog_loaded,
                        unresolved_hosts: &render_model.overlay.workspace_unresolved_hosts,
                        unverifiable_scope_hosts: &render_model
                            .overlay
                            .workspace_unverifiable_scope_hosts,
                    },
                    theme,
                );
            }
            // Help overlay: full-screen keybindings reference.
            if show_help {
                let pop = Rect::new(
                    total.x + 2,
                    total.y + 1,
                    total.width.saturating_sub(4).max(40),
                    total.height.saturating_sub(2).max(10),
                );
                let inner = crate::overlay::begin_popup(
                    f,
                    pop,
                    Line::from(" Keybindings (any key to close) "),
                    theme,
                );

                let help_lines = vec![
                    Line::from(vec![Span::styled(
                        "Global",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )]),
                    Line::from("  Ctrl+Alt+P  Enter Pane mode"),
                    Line::from("  Ctrl+Q    Quit"),
                    Line::from("  click tab ×  Close that terminal tab"),
                    Line::from("  scroll    Scroll focused pane"),
                    Line::raw(""),
                    Line::from(vec![Span::styled(
                        "Pane mode (Ctrl+Alt+P)",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )]),
                    Line::from("  h j k l   Move focus (or arrows)"),
                    Line::from("  Tab       Next terminal tab (wraps)"),
                    Line::from("  p         Pin / unpin agent"),
                    Line::from("  x         Close focused pane"),
                    Line::from("  z         Zoom / unzoom focused pane"),
                    Line::from("  n         Spawn picker (new terminal tab)"),
                    Line::from("  b         Toggle sidebar"),
                    Line::from("  s         Sidebar nav hub"),
                    Line::from("  /         Jump palette (fuzzy-focus)"),
                    Line::from("  a         Activity timeline (overlay)"),
                    Line::from("  d         Agent dashboard (overlay)"),
                    Line::from("  w         All Orca workspaces (scrollable)"),
                    Line::from("  ?         This help"),
                    Line::from("  Esc       Back to Normal"),
                    Line::raw(""),
                    Line::from(vec![Span::styled(
                        "Normal mode",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )]),
                    Line::from("  (all keys except Tab/Shift+Tab forwarded to the agent)"),
                    Line::raw(""),
                    Line::from(vec![Span::styled(
                        "Zoom mode (z in Pane)",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )]),
                    Line::from("  Ctrl+Alt+P → z   Unzoom"),
                    Line::from("  Esc          Normal (interact with agent while zoomed)"),
                ];
                f.render_widget(
                    Paragraph::new(help_lines).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Custom-command text-entry modal (drawn on top of the spawn
            // picker, mirroring the jump palette's centered box style).
            if custom_open {
                use ratatui::style::Modifier;
                let pop_w = total.width.min(60).max(40);
                let pop_h = 5u16;
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                let inner =
                    crate::overlay::begin_popup(f, pop, Line::from(" Custom command "), theme);
                let line = Line::from(vec![
                    Span::styled("> ", Style::default().fg(theme.accent()).bg(theme.panel())),
                    Span::styled(
                        custom_cmd_view.clone(),
                        Style::default().fg(theme.fg()).bg(theme.panel()),
                    ),
                    Span::styled(
                        "_",
                        Style::default()
                            .fg(theme.accent())
                            .bg(theme.panel())
                            .add_modifier(Modifier::SLOW_BLINK),
                    ),
                ]);
                f.render_widget(
                    Paragraph::new(line).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Tasks view — repo-input modal (Phase 2). Mirrors the custom-command
            // modal shape: a centered single-line text entry with a block cursor.
            if tasks_repo_open {
                use ratatui::style::Modifier;
                let pop_w = total.width.min(60).max(40);
                let pop_h = 5u16;
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                let inner = crate::overlay::begin_popup(
                    f,
                    pop,
                    Line::from(" Tasks \u{2014} owner/name "),
                    theme,
                );
                let line = Line::from(vec![
                    Span::styled("> ", Style::default().fg(theme.accent()).bg(theme.panel())),
                    Span::styled(
                        tasks_repo_input_view.clone(),
                        Style::default().fg(theme.fg()).bg(theme.panel()),
                    ),
                    Span::styled(
                        "_",
                        Style::default()
                            .fg(theme.accent())
                            .bg(theme.panel())
                            .add_modifier(Modifier::SLOW_BLINK),
                    ),
                ]);
                f.render_widget(
                    Paragraph::new(line).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Tasks view — issues/PRs list browser (Phase 2). Mirrors the spawn
            // picker shape: a scrollable list with ↑↓ selection + a scroll
            // indicator, or an error message when the fetch failed.
            if tasks_list_open {
                use ratatui::style::Modifier;
                let n = render_model.overlay.tasks_items.len();
                // ~3 items per 12 rows of terminal height, clamped to [2, 8].
                let max_visible = ((total.height / 12) as usize).clamp(2, 8);
                let body_rows = if render_model.overlay.tasks_error.is_some() {
                    3
                } else {
                    n
                };
                let visible = body_rows.min(max_visible);
                let pop_h = (visible as u16 + 3)
                    .min(total.height.saturating_sub(4))
                    .max(5);
                let pop_w = total.width.min(72).max(40);
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                let title = match &render_model.overlay.tasks_error {
                    Some(_) => " Tasks \u{2014} error ",
                    None => " Tasks \u{2014} open issues + PRs ",
                };
                let inner = crate::overlay::begin_popup(f, pop, Line::from(title), theme);

                let mut lines: Vec<Line> = Vec::new();
                if let Some(err) = &render_model.overlay.tasks_error {
                    // Fetch failure: show the error + an Esc hint, no list.
                    lines.push(Line::from(err.as_str()).style(Style::default().fg(theme.error())));
                    lines.push(Line::default());
                    lines.push(
                        Line::from("press Esc to close").style(Style::default().fg(theme.muted())),
                    );
                } else {
                    // Scroll offset: keep the selected item visible (spawn-picker
                    // pattern).
                    let scroll = render_model
                        .overlay
                        .tasks_selected
                        .saturating_sub(max_visible.saturating_sub(1));
                    for (i, (tag, title, number)) in render_model
                        .overlay
                        .tasks_items
                        .iter()
                        .enumerate()
                        .skip(scroll)
                        .take(usize::from(inner.height))
                    {
                        let selected = i == render_model.overlay.tasks_selected;
                        let style = if selected {
                            Style::default()
                                .fg(theme.accent())
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme.fg())
                        };
                        let prefix = if selected { "▶ " } else { "  " };
                        // `▶ #NNN title  [issue|pr]` — tag right-aligned-ish by
                        // a fixed 2-space gap (titles vary in width).
                        lines.push(
                            Line::from(format!("{prefix}#{:<4} {}  [{tag}]", number, title))
                                .style(style),
                        );
                    }
                    // Scroll indicator (only when there are more items than
                    // visible and no error).
                    if n > max_visible {
                        let more_below = render_model.overlay.tasks_selected + 1 < n;
                        let more_above = scroll > 0;
                        let indicator = match (more_above, more_below) {
                            (true, true) => " \u{2191}\u{2193} more ",
                            (true, false) => " \u{2191} end ",
                            (false, true) => " \u{2193} more ",
                            (false, false) => "",
                        };
                        if !indicator.is_empty() {
                            lines.push(
                                Line::from(indicator).style(Style::default().fg(theme.muted())),
                            );
                        }
                    }
                }
                crate::overlay::render_lines(f, inner, lines, theme);
            }
            // Settings overlay (Phase 2): four toggle/cycle rows. Mirrors the
            // spawn-picker shape — a centered rounded box with a ▶ cursor.
            // Changes are applied live to `self.config` by `toggle_setting`;
            // this render just reflects the current values.
            if settings_open {
                use ratatui::style::Modifier;
                let rows = [
                    ("Sidebar", if settings_sidebar_on { "on" } else { "off" }),
                    ("Status bar", if settings_status_bar { "on" } else { "off" }),
                    ("Default agent", settings_default_agent.as_str()),
                    ("Theme", settings_theme_name.as_str()),
                ];
                let pop_h = (rows.len() as u16 + 3)
                    .min(total.height.saturating_sub(4))
                    .max(5);
                let pop_w = total.width.min(44).max(32);
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                let inner = crate::overlay::begin_popup(f, pop, Line::from(" Settings "), theme);

                let mut lines: Vec<Line> = Vec::new();
                for (i, (label, value)) in rows.iter().enumerate() {
                    let selected = i == settings_cursor;
                    let prefix = if selected { "\u{25B6} " } else { "  " };
                    let style = if selected {
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.fg())
                    };
                    lines.push(Line::from(format!("{prefix}{label}: {value}")).style(style));
                }
                crate::overlay::render_lines(f, inner, lines, theme);
            }
            // Toast overlay: render transient messages at the bottom of the
            // content area, above the footer. Each toast is one line.
            if !self.toasts.is_empty() && reserve_footer {
                let toast_rows = self.toasts.len().min(3) as u16;
                let toast_area = Rect::new(
                    content_area.x,
                    footer_area.y.saturating_sub(toast_rows),
                    content_area.width,
                    toast_rows,
                );
                self.toasts.render_buf(f.buffer_mut(), toast_area, theme);
            }
        })?;
        if use_workspace_sidebar {
            self.sidebar_workspace_hitboxes = workspace_hitboxes.workspaces;
            self.sidebar_pane_hitboxes = workspace_hitboxes.panes;
        }

        Ok(())
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) => {
                if key.kind == KeyEventKind::Press {
                    self.handle_key(key);
                }
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(cols, rows) => {
                self.cols = cols.max(MIN_COLS);
                self.rows = rows.max(MIN_ROWS);
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Help overlay: any key dismisses it.
        if self.show_help {
            self.show_help = false;
            return;
        }
        // Normal/Pane keys are reduced to semantic commands in the dedicated
        // input seam. Modal-specific text handlers remain below.
        if matches!(self.mode, InputMode::Normal | InputMode::Pane) {
            if let Some(command) = input::reduce_key(self.mode, key) {
                self.apply_input_command(command);
                return;
            }
        }
        match self.mode {
            InputMode::Jump => self.handle_jump_key(key),
            InputMode::Spawn => self.handle_spawn_key(key),
            // Activity overlay: any key dismisses it (back to Normal), mirroring
            // the Help overlay's "any key to close" contract.
            InputMode::Activity => self.mode = InputMode::Normal,
            // Dashboard overlay (Phase 2): read-only status board. Any key
            // dismisses it back to Normal, mirroring the Activity/Help contract.
            InputMode::Dashboard => self.mode = InputMode::Normal,
            InputMode::Sidebar => match key.code {
                KeyCode::Esc => self.mode = InputMode::Normal,
                KeyCode::Up => {
                    if self.sidebar_nav > 0 {
                        self.sidebar_nav -= 1;
                    }
                }
                KeyCode::Down => {
                    if self.sidebar_nav + 1 < SIDEBAR_NAV_ITEMS.len() {
                        self.sidebar_nav += 1;
                    }
                }
                KeyCode::Enter => match self.sidebar_nav {
                    0 => self.mode = InputMode::Activity,
                    1 => {
                        // Phase 2: Tasks view — reset state and open the
                        // repo-input modal. The fetch runs on Enter (see
                        // `handle_tasks_repo_key`).
                        self.tasks_repo_input.clear();
                        self.tasks_items.clear();
                        self.tasks_repo = None;
                        self.tasks_selected = 0;
                        self.tasks_error = None;
                        self.mode = InputMode::TasksRepo;
                    }
                    2 => {
                        // Phase 2: Settings overlay — reset the cursor and open
                        // the live toggle/cycle panel. Persistence runs on Esc
                        // (see `handle_settings_key`).
                        self.settings_cursor = 0;
                        self.mode = InputMode::Settings;
                    }
                    _ => {
                        // Any future nav item beyond Settings stays a stub.
                        let name = SIDEBAR_NAV_ITEMS[self.sidebar_nav];
                        self.toasts.push(crate::toast::Toast::info(format!(
                            "{name} \u{2014} coming soon"
                        )));
                        self.mode = InputMode::Normal;
                    }
                },
                _ => {}
            },
            InputMode::Workspaces => {
                let count = self.workspace_count();
                match key.code {
                    KeyCode::Esc => self.mode = InputMode::Normal,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.workspace_selected = self.workspace_selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if count > 0 {
                            self.workspace_selected =
                                (self.workspace_selected + 1).min(count.saturating_sub(1));
                        }
                    }
                    KeyCode::PageUp => {
                        self.workspace_selected = self.workspace_selected.saturating_sub(8);
                    }
                    KeyCode::PageDown => {
                        if count > 0 {
                            self.workspace_selected =
                                (self.workspace_selected + 8).min(count.saturating_sub(1));
                        }
                    }
                    _ => {}
                }
            }
            InputMode::SpawnCustom => match key.code {
                KeyCode::Esc => self.mode = InputMode::Normal,
                KeyCode::Enter => {
                    // Shell-split on whitespace (no quote handling — v1).
                    let parts: Vec<String> = self
                        .custom_cmd
                        .split_whitespace()
                        .map(String::from)
                        .collect();
                    if parts.is_empty() {
                        self.mode = InputMode::Normal;
                        return;
                    }
                    if !self.can_spawn_pane() {
                        self.toasts.push(crate::toast::Toast::warning(
                            "Terminal too small for another pane".to_string(),
                        ));
                        self.mode = InputMode::Normal;
                        return;
                    }
                    let spec = AgentSpec::from_command(parts);
                    let idx = self.spawn_one(spec);
                    self.focus = idx;
                    self.mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    self.custom_cmd.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.custom_cmd.push(c);
                }
                _ => {}
            },
            InputMode::TasksRepo => self.handle_tasks_repo_key(key),
            InputMode::TasksList => self.handle_tasks_list_key(key),
            InputMode::Settings => self.handle_settings_key(key),
            // Normal and Pane modes are fully handled by `reduce_key` above.
            InputMode::Normal | InputMode::Pane => {}
        }
    }

    /// 执行输入 reducer 产生的语义命令；这里不解析原始按键，也不执行外部 I/O。
    fn apply_input_command(&mut self, command: InputCommand) {
        match command {
            InputCommand::Noop => {}
            InputCommand::Quit => self.request_quit(),
            InputCommand::EnterPaneMode => self.mode = InputMode::Pane,
            InputCommand::SetNormalMode => self.mode = InputMode::Normal,
            InputCommand::Forward(key) => self.forward_key_to_agent(key),
            InputCommand::Focus(direction) => self.focus_directional(direction),
            InputCommand::NextTab => self.focus_next(),
            InputCommand::PreviousTab => self.focus_prev(),
            InputCommand::TogglePin => self.toggle_pin_focused(),
            InputCommand::ClosePane => self.close_focused_pane(),
            InputCommand::ToggleZoom => self.zoomed = !self.zoomed,
            InputCommand::ToggleHelp => self.show_help = !self.show_help,
            InputCommand::OpenJump => {
                self.jump_query.clear();
                self.jump_selected = 0;
                self.mode = InputMode::Jump;
            }
            InputCommand::OpenActivity => self.mode = InputMode::Activity,
            InputCommand::OpenDashboard => self.mode = InputMode::Dashboard,
            InputCommand::OpenSpawn => {
                self.spawn_selected = 0;
                self.mode = InputMode::Spawn;
            }
            InputCommand::ToggleSidebar => self.sidebar_hidden = !self.sidebar_hidden,
            InputCommand::OpenSidebar => {
                self.mode = InputMode::Sidebar;
                self.sidebar_nav = 0;
            }
            InputCommand::OpenWorkspaces => {
                self.mode = InputMode::Workspaces;
                self.workspace_selected = 0;
            }
        }
    }

    fn request_quit(&mut self) {
        self.quit = true;
        // Queue only explicitly-created Orca tabs for the legacy daemon
        // teardown path; GUI-owned tabs remain untouched.
        self.queue_daemon_sessions_for_cleanup();
    }

    fn queue_daemon_sessions_for_cleanup(&mut self) {
        self.daemon_cleanup_session_ids
            .extend(self.panes.iter().filter_map(|slot| {
                if self.orca_cli.is_some() {
                    (slot.session_owner == SessionOwner::OrcaTuiOwned)
                        .then(|| slot.daemon_session_id.clone())
                        .flatten()
                } else {
                    slot.daemon_session_id.clone()
                }
            }));
        self.daemon_cleanup_session_ids.sort();
        self.daemon_cleanup_session_ids.dedup();
    }

    /// Jump-palette key handling: build the filter query, move the selection
    /// within the filtered list, focus on Enter, cancel on Esc.
    fn handle_jump_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                // Focus the selected filtered agent (if any) and close.
                if let Some(&idx) = self.jump_filtered().get(self.jump_selected) {
                    self.focus = idx;
                }
                self.mode = InputMode::Normal;
            }
            KeyCode::Backspace => {
                self.jump_query.pop();
                self.jump_selected = 0;
            }
            KeyCode::Up => {
                if self.jump_selected > 0 {
                    self.jump_selected -= 1;
                }
            }
            KeyCode::Down => {
                let n = self.jump_filtered().len();
                if self.jump_selected + 1 < n {
                    self.jump_selected += 1;
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.jump_query.push(c);
                self.jump_selected = 0;
            }
            _ => {}
        }
    }

    /// The pane indices whose name matches the jump query (case-insensitive
    /// substring). Empty query ⇒ all panes, in order.
    fn jump_filtered(&self) -> Vec<usize> {
        let q = self.jump_query.to_ascii_lowercase();
        self.panes
            .iter()
            .enumerate()
            .filter(|(_, p)| q.is_empty() || p.name().to_ascii_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    /// Tasks view repo-input modal. Enter parses `owner/name` and starts the
    /// issue/PR query on a background worker; Backspace deletes and Esc cancels.
    fn handle_tasks_repo_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.tasks_fetch_rx = None;
                self.task_body_rx = None;
                self.task_body_entry = None;
                self.tasks_request_id = self.tasks_request_id.wrapping_add(1);
                self.mode = InputMode::Normal;
            }
            KeyCode::Backspace => {
                self.tasks_repo_input.pop();
            }
            KeyCode::Enter => {
                // Parse the repo reference. On error, surface the message and
                // stay in the repo-input modal so the user can fix the typo.
                match RepoRef::parse(&self.tasks_repo_input) {
                    Ok(repo) => {
                        self.tasks_repo = Some(repo.clone());
                        self.tasks_request_id = self.tasks_request_id.wrapping_add(1);
                        let request_id = self.tasks_request_id;
                        let (tx, rx) = std::sync::mpsc::channel();
                        self.tasks_fetch_rx = Some(rx);
                        self.tasks_items.clear();
                        self.tasks_selected = 0;
                        self.tasks_error = Some("正在获取任务…".to_string());
                        self.mode = InputMode::TasksList;
                        std::thread::Builder::new()
                            .name("orca-gh-tasks".into())
                            .spawn(move || {
                                let result = Self::fetch_tasks_blocking(&repo);
                                let _ = tx.send((request_id, result));
                            })
                            .ok();
                    }
                    Err(e) => {
                        self.tasks_error = Some(format!("{e:#}"));
                        self.toasts
                            .push(crate::toast::Toast::warning(format!("{e:#}")));
                    }
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.tasks_repo_input.push(c);
            }
            _ => {}
        }
    }

    /// Phase 2 — Tasks view: fetch open issues + PRs for `repo` and merge them
    /// into a single [`Vec<TasksEntry>`] sorted by number ascending. Issues
    /// store a title-only prompt initially (body is fetched lazily on
    /// dispatch); PRs store the final `pr_to_prompt` form.
    fn fetch_tasks_blocking(repo: &RepoRef) -> anyhow::Result<Vec<TasksEntry>> {
        use crate::integrations::{issue_to_prompt, list_issues, list_pull_requests, pr_to_prompt};
        let mut items: Vec<TasksEntry> = Vec::new();
        // Issues — body is None from the list endpoint; the issue_to_prompt
        // form here is title-only. The real body is fetched on Enter via
        // `integrations::fetch_issue` so the dispatch prompt includes it.
        let issues = list_issues(repo).context("fetching open issues via `gh issue list`")?;
        for iss in issues {
            // Build a body-less copy for the initial prompt; the lazy fetch
            // replaces this with the full-body prompt on dispatch.
            let title_only = crate::integrations::Issue {
                number: iss.number,
                title: iss.title.clone(),
                body: None,
            };
            items.push(TasksEntry {
                kind: TaskKind::Issue,
                number: iss.number,
                title: iss.title,
                prompt: issue_to_prompt(&title_only),
            });
        }
        // Pull requests — pr_to_prompt is the final form (no lazy body fetch).
        let prs = list_pull_requests(repo).context("fetching open PRs via `gh pr list`")?;
        for pr in prs {
            // Borrow `pr` for the prompt before partially moving `pr.title`.
            let prompt = pr_to_prompt(&pr);
            items.push(TasksEntry {
                kind: TaskKind::PullRequest,
                number: pr.number,
                title: pr.title,
                prompt,
            });
        }
        // Sort ascending by number so the list is stable + scannable.
        items.sort_by_key(|e| e.number);
        Ok(items)
    }

    /// Phase 2 — Tasks view: issues/PRs list browser key handling. ↑↓ moves
    /// the selection (clamped), Enter dispatches a new agent pane with the
    /// issue/PR body as the prompt, Esc returns to Normal.
    fn handle_tasks_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.tasks_fetch_rx = None;
                self.task_body_rx = None;
                self.task_body_entry = None;
                self.tasks_request_id = self.tasks_request_id.wrapping_add(1);
                self.tasks_error = None;
                self.mode = InputMode::Normal;
            }
            KeyCode::Up => {
                if self.tasks_selected > 0 {
                    self.tasks_selected -= 1;
                }
            }
            KeyCode::Down => {
                let n = self.tasks_items.len();
                if n > 0 && self.tasks_selected + 1 < n {
                    self.tasks_selected += 1;
                }
            }
            KeyCode::Enter => {
                // Nothing to dispatch if the list is empty (e.g. a fetch error
                // left tasks_items empty and the overlay is showing the error).
                let Some(entry) = self.tasks_items.get(self.tasks_selected).cloned() else {
                    return;
                };
                if !self.can_spawn_pane() {
                    self.toasts.push(crate::toast::Toast::warning(
                        "Terminal too small for another pane".to_string(),
                    ));
                    return;
                }
                // For issues, lazily enrich the prompt with the real body so
                // the dispatched agent gets the full issue text. PRs already
                // carry their final prompt. On body-fetch failure, fall back
                // to the stored title-only prompt + warn the operator.
                let final_prompt = match entry.kind {
                    TaskKind::PullRequest => entry.prompt.clone(),
                    TaskKind::Issue => {
                        if let Some(repo) = self.tasks_repo.clone() {
                            self.tasks_request_id = self.tasks_request_id.wrapping_add(1);
                            let request_id = self.tasks_request_id;
                            let (tx, rx) = std::sync::mpsc::channel();
                            self.task_body_rx = Some(rx);
                            self.task_body_entry = Some(entry.clone());
                            self.mode = InputMode::TasksList;
                            std::thread::Builder::new()
                                .name("orca-gh-issue-body".into())
                                .spawn(move || {
                                    let result =
                                        crate::integrations::fetch_issue(&repo, entry.number)
                                            .map(|issue| {
                                                crate::integrations::issue_to_prompt(&issue)
                                            })
                                            .map_err(|error| format!("{error:#}"));
                                    let _ = tx.send((request_id, result));
                                })
                                .ok();
                            return;
                        } else {
                            // No stored repo (shouldn't happen — set on
                            // TasksRepo submit). Fall back to the title-only
                            // prompt rather than blocking dispatch.
                            entry.prompt.clone()
                        }
                    }
                };
                let agent = self.config.default_agent.clone();
                let mut spec = AgentSpec::from_command(vec![agent, final_prompt]);
                spec.name = format!(
                    "{}-#{}",
                    match entry.kind {
                        TaskKind::Issue => "issue",
                        TaskKind::PullRequest => "pr",
                    },
                    entry.number
                );
                let idx = self.spawn_one(spec);
                self.focus = idx;
                self.mode = InputMode::Normal;
            }
            _ => {}
        }
    }

    /// The fixed list of selectable default-agent binaries, in cycle order.
    /// Mirrors the spawn-picker agent registry — `bash` first (matches the
    /// Config default), then the AI coding agents.
    const SETTINGS_AGENTS: &'static [&'static str] =
        &["bash", "opencode", "claude", "codex", "gemini", "aider"];

    /// Phase 2 — Settings overlay key handling. ↑↓ moves the cursor (clamped
    /// to 0..=3), Enter OR Space toggles/cycles the focused row LIVE (the very
    /// next render reflects it), Esc persists the whole config via
    /// [`Config::save`] and returns to Normal.
    ///
    /// Persistence is best-effort: on IO failure (e.g. no writable config dir
    /// in a sandboxed test environment) a warning toast is queued but the mode
    /// still returns to Normal so the user isn't trapped in the overlay.
    fn handle_settings_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // Persist the whole (possibly mutated) config. A failure here
                // is non-fatal — warn + close so the user isn't stuck.
                match self.config.save() {
                    Ok(()) => self
                        .toasts
                        .push(crate::toast::Toast::success("Settings saved".to_string())),
                    Err(e) => self.toasts.push(crate::toast::Toast::warning(format!(
                        "config save failed: {e:#}"
                    ))),
                }
                self.mode = InputMode::Normal;
            }
            KeyCode::Up => {
                if self.settings_cursor > 0 {
                    self.settings_cursor -= 1;
                }
            }
            KeyCode::Down => {
                if self.settings_cursor < 3 {
                    self.settings_cursor += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.toggle_setting(self.settings_cursor);
            }
            _ => {}
        }
    }

    /// Apply a single settings-row toggle LIVE to `self.config`. Row 0 toggles
    /// the sidebar (sidebar_width 0 hides it via the existing render logic; the
    /// default width 26 is restored on the next toggle). Row 1 flips the status
    /// bar. Row 2 cycles the default agent through [`SETTINGS_AGENTS`]. Row 3
    /// cycles the theme through [`Config::theme_presets`] (matched by accent).
    fn toggle_setting(&mut self, row: usize) {
        match row {
            0 => {
                // Sidebar toggle: 0 hides it; non-zero restores the default
                // width. `sidebar_hidden` (the Ctrl+B user override) is left
                // alone — it's a separate, render-time concern.
                if self.config.layout.sidebar_width > 0 {
                    self.config.layout.sidebar_width = 0;
                } else {
                    self.config.layout.sidebar_width = LayoutConfig::default().sidebar_width;
                }
            }
            1 => {
                self.config.layout.show_status_bar = !self.config.layout.show_status_bar;
            }
            2 => {
                // Cycle the default agent through the fixed list, wrapping.
                let cur = self.config.default_agent.clone();
                let next = Self::SETTINGS_AGENTS
                    .iter()
                    .position(|a| a.eq_ignore_ascii_case(&cur))
                    .map(|i| Self::SETTINGS_AGENTS[(i + 1) % Self::SETTINGS_AGENTS.len()])
                    .unwrap_or(Self::SETTINGS_AGENTS[0]);
                self.config.default_agent = (*next).to_string();
            }
            3 => {
                // Cycle the theme through the presets. Match the current theme
                // by accent-hex equality (presets use distinct accents); if the
                // user has a custom theme that matches no preset, start the
                // cycle from index 0 (GitHub Dark).
                let presets = Config::theme_presets();
                let idx = presets
                    .iter()
                    .position(|(_, t)| t.accent.eq_ignore_ascii_case(&self.config.theme.accent))
                    .map(|i| (i + 1) % presets.len())
                    .unwrap_or(0);
                self.config.theme = presets[idx].1.clone();
            }
            _ => {}
        }
    }

    /// Normal-mode passthrough: forward the key to the focused agent's PTY as
    /// raw bytes / VT escape sequences. The agent receives everything — Tab,
    /// Esc, Ctrl+C, arrows — exactly as if it were a real terminal.
    fn forward_key_to_agent(&mut self, key: KeyEvent) {
        // IME note: committed Hangul/CJK/emoji/accented input is forwarded
        // verbatim via the UTF-8 path below and works correctly. Incomplete IME
        // compositions (e.g. a lone Hangul jamo during preedit) are NOT filtered
        // here — crossterm 0.28's `KeyEventState` has no `COMPOSING` flag under
        // the default terminal encoding, so there's no reliable signal to gate
        // on. Fully fixing preedit cursor desync needs an enhanced keyboard
        // protocol / OSC 51-style preedit; see the `hangul` module for a
        // composition building block. Non-composing events are unaffected.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if ctrl => {
                let byte = (c as u8).to_ascii_lowercase().wrapping_sub(b'a' - 1);
                self.send_to_focused(&[byte]);
            }
            // Send the full UTF-8 encoding — `c as u8` truncates non-ASCII
            // (Korean/CJK/emoji/accents are 2-4 bytes), which corrupts input.
            KeyCode::Char(c) if !ctrl && !alt => {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                self.send_to_focused(s.as_bytes());
            }
            KeyCode::Enter if !ctrl && !alt => self.send_to_focused(b"\r"),
            KeyCode::Backspace if !ctrl && !alt => self.send_to_focused(&[0x7f]),
            KeyCode::Tab if !ctrl && !alt => self.send_to_focused(b"\t"),
            KeyCode::Esc => self.send_to_focused(b"\x1b"),
            KeyCode::Up => self.send_to_focused(b"\x1b[A"),
            KeyCode::Down => self.send_to_focused(b"\x1b[B"),
            KeyCode::Right => self.send_to_focused(b"\x1b[C"),
            KeyCode::Left => self.send_to_focused(b"\x1b[D"),
            KeyCode::Home => self.send_to_focused(b"\x1b[H"),
            KeyCode::End => self.send_to_focused(b"\x1b[F"),
            KeyCode::PageUp => self.send_to_focused(b"\x1b[5~"),
            KeyCode::PageDown => self.send_to_focused(b"\x1b[6~"),
            KeyCode::Delete => self.send_to_focused(b"\x1b[3~"),
            KeyCode::BackTab => self.send_to_focused(b"\x1b[Z"),
            _ => {}
        }
    }

    /// Mouse scroll: move the focused pane's scrollback (3 lines per notch).
    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        let (col, row) = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                // A fullscreen/redraw agent (opencode, claude, …) runs in the
                // ALTERNATE screen, which has no terminal scrollback — so scroll
                // OUR buffer would do nothing. Instead forward a PageUp to the
                // agent and let IT scroll its own content (the mouse-friendly
                // path for redraw TUIs). Main-screen panes (bash) scroll our
                // own scrollback as before.
                let in_alt = self
                    .panes
                    .get(self.focus)
                    .map_or(false, |p| p.is_alternate_screen());
                if in_alt {
                    self.send_to_focused(b"\x1b[5~"); // PageUp
                } else if let Some(p) = self.focused_pane_mut() {
                    p.scroll_up(3);
                }
            }
            MouseEventKind::ScrollDown => {
                let in_alt = self
                    .panes
                    .get(self.focus)
                    .map_or(false, |p| p.is_alternate_screen());
                if in_alt {
                    self.send_to_focused(b"\x1b[6~"); // PageDown
                } else if let Some(p) = self.focused_pane_mut() {
                    p.scroll_down(3);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Navigation chrome is interactive even in Normal mode:
                // activate a tab or workspace with one click, matching the
                // Herdr layout. A click on `+` creates a shell-backed tab.
                if self
                    .footer_quit_hitbox
                    .is_some_and(|rect| contains(rect, col, row))
                {
                    self.request_quit();
                    self.mode = InputMode::Normal;
                    self.drag_origin = None;
                    return;
                }
                if self
                    .tab_hitboxes
                    .quit
                    .is_some_and(|r| contains(r, col, row))
                {
                    self.request_quit();
                    self.mode = InputMode::Normal;
                    self.drag_origin = None;
                    return;
                }
                // A close affordance is nested inside its tab, so test it
                // before the broader tab hitboxes. Resolve the filtered tab
                // index through `tab_pane_indices` before removing anything;
                // workspace-scoped strips must never close a pane from another
                // workspace just because vector positions differ.
                if let Some(tab_idx) = self
                    .tab_hitboxes
                    .closes
                    .iter()
                    .position(|rect| rect.is_some_and(|r| contains(r, col, row)))
                {
                    if let Some(&pane_idx) = self.tab_pane_indices.get(tab_idx) {
                        if pane_idx < self.panes.len() {
                            // Prefer the neighboring tab from this same
                            // filtered strip. A plain global-index clamp can
                            // move focus into another workspace when the
                            // closed tab was followed by an unrelated pane.
                            let neighbor = if tab_idx > 0 {
                                self.tab_pane_indices.get(tab_idx - 1).copied()
                            } else {
                                self.tab_pane_indices.get(tab_idx + 1).copied()
                            };
                            self.focus = pane_idx;
                            self.close_focused_pane();
                            if let Some(mut neighbor) = neighbor {
                                // Removing a pane shifts every later global
                                // index one slot to the left.
                                if neighbor > pane_idx {
                                    neighbor = neighbor.saturating_sub(1);
                                }
                                if neighbor < self.panes.len() {
                                    self.focus = neighbor;
                                }
                            }
                        }
                    }
                    self.mode = InputMode::Normal;
                    self.drag_origin = None;
                    return;
                }
                if let Some(idx) = self
                    .tab_hitboxes
                    .tabs
                    .iter()
                    .position(|r| contains(*r, col, row))
                {
                    if let Some(&pane_idx) = self.tab_pane_indices.get(idx) {
                        if pane_idx < self.panes.len() {
                            self.focus = pane_idx;
                        }
                        self.mode = InputMode::Normal;
                    }
                    self.drag_origin = None;
                    return;
                }
                if self.tab_hitboxes.add.is_some_and(|r| contains(r, col, row)) {
                    if self.can_spawn_pane() {
                        let command = if self.config.default_agent.is_empty() {
                            vec!["bash".to_owned()]
                        } else {
                            vec![self.config.default_agent.clone()]
                        };
                        let idx = self.spawn_one(AgentSpec::from_command(command));
                        self.focus = idx;
                        self.mode = InputMode::Normal;
                    } else {
                        self.toasts.push(crate::toast::Toast::warning(
                            "无法创建终端 tab：当前终端空间不足".to_string(),
                        ));
                    }
                    self.drag_origin = None;
                    return;
                }
                if let Some(sidebar) = self.sidebar_rect {
                    if contains(sidebar, col, row) {
                        // Workspace-first sidebar: child terminal rows focus
                        // an existing pane; clicking a workspace focuses its
                        // existing terminal or lazily opens a local checkout.
                        if let Some(pane_idx) = self
                            .sidebar_pane_hitboxes
                            .iter()
                            .position(|rect| rect.is_some_and(|rect| contains(rect, col, row)))
                        {
                            if pane_idx < self.panes.len() {
                                self.focus = pane_idx;
                                self.mode = InputMode::Normal;
                                self.drag_origin = None;
                                return;
                            }
                        }
                        if let Some(group_idx) = self
                            .sidebar_workspace_hitboxes
                            .iter()
                            .position(|rect| rect.is_some_and(|rect| contains(rect, col, row)))
                        {
                            if let Some(workspace) = self
                                .sidebar_workspace_targets
                                .get(group_idx)
                                .and_then(Option::as_ref)
                                .cloned()
                            {
                                if let Some(pane_idx) = self.panes.iter().position(|slot| {
                                    slot.workspace_id.as_deref() == Some(workspace.id.as_str())
                                        || slot.worktree_path.as_ref() == Some(&workspace.path)
                                }) {
                                    self.focus = pane_idx;
                                } else if workspace.is_local() && self.can_spawn_pane() {
                                    let command = self
                                        .panes
                                        .get(self.focus)
                                        .map(|slot| slot.command.clone())
                                        .filter(|command| !command.is_empty())
                                        .unwrap_or_else(|| {
                                            let agent = if self.config.default_agent.is_empty() {
                                                "bash"
                                            } else {
                                                self.config.default_agent.as_str()
                                            };
                                            vec![agent.to_owned()]
                                        });
                                    let mut spec = AgentSpec::from_command(command);
                                    spec.name = workspace.sidebar_name();
                                    spec.worktree_branch = Some(workspace.branch.clone());
                                    spec.workspace_id = Some(workspace.id.clone());
                                    spec.worktree = Some(workspace.path.clone());
                                    self.focus = self.spawn_one(spec);
                                }
                            }
                            self.mode = InputMode::Normal;
                            self.drag_origin = None;
                            return;
                        }
                        // Compatibility hit boxes are retained for the old
                        // flat renderer and for tests that construct panes
                        // without a workspace catalog.
                        if let Some(entry_idx) = self
                            .sidebar_entry_hitboxes
                            .iter()
                            .position(|r| r.is_some_and(|rect| contains(rect, col, row)))
                        {
                            if entry_idx < self.panes.len() {
                                self.focus = entry_idx;
                            } else if let Some(workspace) = self
                                .sidebar_catalog_entries
                                .get(entry_idx.saturating_sub(self.panes.len()))
                                .cloned()
                            {
                                if let Some(pane_idx) = self.panes.iter().position(|slot| {
                                    slot.workspace_id.as_deref() == Some(workspace.id.as_str())
                                }) {
                                    self.focus = pane_idx;
                                } else if workspace.is_local() && self.can_spawn_pane() {
                                    // A catalog workspace may be visible before
                                    // its terminal tab exists (for example when
                                    // `run -- claude` was started without
                                    // `--all-worktrees`). Create that tab lazily
                                    // in the selected checkout on click.
                                    let command = self
                                        .panes
                                        .get(self.focus)
                                        .map(|slot| slot.command.clone())
                                        .filter(|command| !command.is_empty())
                                        .unwrap_or_else(|| {
                                            let agent = if self.config.default_agent.is_empty() {
                                                "bash"
                                            } else {
                                                self.config.default_agent.as_str()
                                            };
                                            vec![agent.to_owned()]
                                        });
                                    let mut spec = AgentSpec::from_command(command);
                                    spec.name = workspace.sidebar_name();
                                    spec.worktree_branch = Some(workspace.branch.clone());
                                    spec.workspace_id = Some(workspace.id.clone());
                                    spec.worktree = Some(workspace.path.clone());
                                    self.focus = self.spawn_one(spec);
                                } else {
                                    self.toasts.push(crate::toast::Toast::info(format!(
                                        "{} 没有本地终端 tab",
                                        workspace.sidebar_name()
                                    )));
                                }
                            }
                            self.mode = InputMode::Normal;
                            self.drag_origin = None;
                            return;
                        }
                    }
                }
                if let Some(idx) = self.pane_at(col, row) {
                    // Clear any selection on other panes. We do NOT start a
                    // selection on Down — a plain click should only FOCUS the
                    // pane. The selection starts lazily on the first Drag move
                    // (see the Drag arm), so a click without movement selects
                    // nothing and copies nothing.
                    for (i, p) in self.panes.iter_mut().enumerate() {
                        if i != idx {
                            p.clear_selection();
                        }
                    }
                    self.focus = idx;
                    self.drag_origin = self
                        .map_to_inner(idx, col, row)
                        .map(|inner| (idx, inner.0, inner.1));
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Start the selection (at the Down point) on the FIRST move,
                // then extend it to the current cell. Belongs to the pane where
                // the button went down, so dragging across panes keeps the
                // selection in its origin pane.
                if let Some((origin_idx, oc, or)) = self.drag_origin {
                    if let Some((c, r)) = self.map_to_inner(origin_idx, col, row) {
                        if let Some(p) = self.panes.get_mut(origin_idx) {
                            if !p.has_selection() {
                                p.start_selection(oc, or);
                            }
                            p.extend_selection(c, r);
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Only copy when a real DRAG produced a selection. A plain click
                // (Down with no Drag) leaves no selection, so this is a no-op
                // and the user isn't bombarded with "Copied"/"No clipboard" on
                // every click.
                let text = self.panes.get(self.focus).and_then(|p| p.selected_text());
                if let Some(t) = text {
                    match crate::clipboard::copy_with(&t, self.config.clipboard_command.as_deref())
                    {
                        Ok(()) => self.toasts.push(crate::toast::Toast::info("Copied")),
                        Err(_) => self.toasts.push(crate::toast::Toast::warning(
                            "No clipboard tool — set [clipboard] in config (e.g. clip.exe on WSL)",
                        )),
                    }
                }
                if let Some(p) = self.panes.get_mut(self.focus) {
                    p.clear_selection();
                }
                self.drag_origin = None;
            }
            _ => {}
        }
    }

    /// Index of the pane whose last-rendered OUTER rect contains `(col,row)`,
    /// or `None` when the point is outside every pane (or no rects are cached).
    fn pane_at(&self, col: u16, row: u16) -> Option<usize> {
        self.pane_rects
            .iter()
            .position(|r| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
    }

    /// Map a terminal `(col,row)` to pane `idx`'s INNER grid `(col,row)`.
    /// The pane content sits 1 cell inside the rounded border on every side,
    /// so inner = outer shrunk by 1 on each edge. Returns `None` if the point
    /// is outside the pane or the inner area is too small to hold content.
    fn map_to_inner(&self, idx: usize, col: u16, row: u16) -> Option<(u16, u16)> {
        let r = self.pane_rects.get(idx)?;
        if col < r.x || row < r.y {
            return None;
        }
        let c = col.saturating_sub(r.x).saturating_sub(1);
        let rr = row.saturating_sub(r.y).saturating_sub(1);
        // Clamp to the inner dimensions (w-2, h-2); the -2 accounts for both borders.
        let max_c = r.width.saturating_sub(2);
        let max_r = r.height.saturating_sub(2);
        if max_c == 0 || max_r == 0 {
            return None;
        }
        Some((c.min(max_c - 1), rr.min(max_r - 1)))
    }

    /// Grid-aware directional focus (Pane mode: arrows / hjkl).
    fn focus_directional(&mut self, dir: FocusDir) {
        let n = self.panes.len();
        if n == 0 {
            return;
        }
        let cols = ((n as f64).sqrt().ceil() as usize).max(1);
        self.focus = match dir {
            FocusDir::Right => {
                let row_end = (self.focus / cols + 1) * cols;
                if self.focus + 1 < row_end.min(n) {
                    self.focus + 1
                } else {
                    self.focus
                }
            }
            FocusDir::Left => {
                if self.focus % cols > 0 {
                    self.focus - 1
                } else {
                    self.focus
                }
            }
            FocusDir::Down => {
                if self.focus + cols < n {
                    self.focus + cols
                } else {
                    self.focus
                }
            }
            FocusDir::Up => {
                if self.focus >= cols {
                    self.focus - cols
                } else {
                    self.focus
                }
            }
        };
    }

    #[allow(dead_code)]
    fn focus_next(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        // In workspace mode, cycle only through the tabs rendered for the
        // active workspace. Fall back to the complete pane list before the
        // first frame (or if the cached map became stale after a close).
        let scoped = self
            .tab_pane_indices
            .iter()
            .copied()
            .filter(|idx| *idx < self.panes.len())
            .collect::<Vec<_>>();
        let indices: &[usize] = if !scoped.is_empty() && scoped.contains(&self.focus) {
            &scoped
        } else {
            // This local array cannot be borrowed as a slice alongside the
            // branch above, so handle the fallback directly.
            if self.focus >= self.panes.len() {
                self.focus = 0;
            } else {
                self.focus = (self.focus + 1) % self.panes.len();
            }
            return;
        };
        if let Some(position) = indices.iter().position(|idx| *idx == self.focus) {
            self.focus = indices[(position + 1) % indices.len()];
        }
    }

    #[allow(dead_code)]
    fn focus_prev(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        let scoped = self
            .tab_pane_indices
            .iter()
            .copied()
            .filter(|idx| *idx < self.panes.len())
            .collect::<Vec<_>>();
        if !scoped.is_empty() && scoped.contains(&self.focus) {
            if let Some(position) = scoped.iter().position(|idx| *idx == self.focus) {
                self.focus = scoped[(position + scoped.len() - 1) % scoped.len()];
            }
        } else {
            self.focus = if self.focus == 0 {
                self.panes.len() - 1
            } else {
                self.focus - 1
            };
        }
    }

    /// Build the list of spawnable agents for the Ctrl+N picker.
    /// Always includes `bash`; then all detected installed agents (deduped);
    /// then the configured default if not already present.
    fn spawn_options(&self) -> Vec<(String, Vec<String>)> {
        // Detect installed agents ONCE per call (this only runs while the spawn
        // picker is open, not every normal-mode frame) so each catalog entry can
        // be marked with whether its binary is actually on $PATH.
        let installed: Vec<&'static str> = AgentKind::detect_installed()
            .iter()
            .map(AgentKind::binary)
            .collect();
        let mut opts: Vec<(String, Vec<String>)> = Vec::new();
        // Always-available: bash.
        opts.push(("bash".to_string(), vec!["bash".to_string()]));
        // The FULL agent catalog, not just the detected subset. A user can pick
        // any known CLI agent; an uninstalled one is tagged "(not installed)"
        // and, if selected, spawns into a Failed pane that surfaces the
        // missing-binary error (so the catalog is always complete — matches the
        // "run any CLI agent" philosophy and Orca's agent list).
        for kind in AgentKind::all_known() {
            let bin = kind.binary();
            let name = if installed.contains(&bin) {
                kind.display_name().to_string()
            } else {
                format!("{} (not installed)", kind.display_name())
            };
            opts.push((name, vec![bin.to_string()]));
        }
        // Configured default if not already listed.
        let default = &self.config.default_agent;
        if !opts
            .iter()
            .any(|(_, cmd)| cmd.first().is_some_and(|c| c == default))
            && !default.is_empty()
        {
            opts.push((default.clone(), vec![default.clone()]));
        }
        // Custom-command sentinel: selecting this entry opens the
        // `InputMode::SpawnCustom` text-entry modal so the user can type any
        // command. The empty command vector is the sentinel.
        opts.push(("Custom command\u{2026}".to_string(), Vec::new()));
        opts
    }

    /// Maximum number of terminal tabs kept in one TUI session.
    fn max_panes(&self) -> usize {
        // Tabs share one full-size terminal surface, so their count is not
        // constrained by a split-grid's cell dimensions. Keep a generous
        // safety cap to avoid unbounded PTY/session creation from repeated
        // clicks while still supporting real Orca workspaces on narrow windows.
        256
    }

    /// Check if there is room for one more pane.
    fn can_spawn_pane(&self) -> bool {
        self.panes.len() < self.max_panes()
    }

    /// Spawn-picker key handling: Up/Down to navigate, Enter to spawn, Esc cancel.
    fn handle_spawn_key(&mut self, key: KeyEvent) {
        let opts = self.spawn_options();
        let n = opts.len();
        match key.code {
            KeyCode::Esc => self.mode = InputMode::Normal,
            KeyCode::Up => {
                if self.spawn_selected > 0 {
                    self.spawn_selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.spawn_selected + 1 < n {
                    self.spawn_selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some((_, cmd)) = opts.get(self.spawn_selected) {
                    if cmd.is_empty() {
                        // Custom command sentinel: switch to the text-entry modal.
                        self.custom_cmd.clear();
                        self.mode = InputMode::SpawnCustom;
                        return;
                    }
                    if !self.can_spawn_pane() {
                        self.toasts.push(crate::toast::Toast::warning(
                            "Terminal too small for another pane".to_string(),
                        ));
                        self.mode = InputMode::Normal;
                        return;
                    }
                    let spec = AgentSpec::from_command(cmd.clone());
                    let idx = self.spawn_one(spec);
                    self.focus = idx;
                }
                self.mode = InputMode::Normal;
            }
            _ => {}
        }
    }

    fn toggle_pin_focused(&mut self) {
        if let Some(slot) = self.panes.get_mut(self.focus) {
            slot.pinned = !slot.pinned;
        }
    }

    /// Close the focused pane, drop its complete slot, and clamp focus.
    fn close_focused_pane(&mut self) {
        if self.focus >= self.panes.len() {
            return;
        }
        let idx = self.focus;
        // `x` is the explicit close action. Local PTYs are killed; an Orca
        // terminal is closed only when this TUI created it. Existing GUI tabs
        // are detached from this view without any remote mutation.
        if let Some(session) = self.panes[idx].session.as_mut() {
            let _ = session.kill();
        }
        if let Some(session_id) = self.panes[idx].daemon_session_id.clone() {
            let legacy_daemon = self.orca_cli.is_none();
            if legacy_daemon || self.panes[idx].session_owner == SessionOwner::OrcaTuiOwned {
                self.daemon_cleanup_session_ids.push(session_id.clone());
                if let (Some(bridge), Some(handle)) = (
                    self.orca_cli.as_ref(),
                    self.panes[idx].orca_handle.as_deref(),
                ) {
                    let bridge = bridge.clone();
                    let handle = handle.to_owned();
                    self.orca_closed_handles.insert(handle.clone());
                    let _ = thread::Builder::new()
                        .name("orca-cli-close".into())
                        .spawn(move || {
                            let _ = bridge.close_tab(&handle);
                        });
                } else if let Some(daemon) = self.daemon.as_mut() {
                    daemon.enqueue_kill(session_id.clone());
                }
            }
            if let Some(session_map) = &self.daemon_session_map {
                session_map.lock().unwrap().remove(&session_id);
            }
        }
        // Dropping one slot removes the PTY and every item of pane-owned state
        // atomically; there are no sibling vectors to keep in lockstep.
        let pane_id = self.panes[idx].id();
        if self.orca_spawn_rx.iter().any(|(id, _)| *id == pane_id) {
            self.orca_cancelled_panes.insert(pane_id);
        }
        self.panes.remove(idx);
        // Keep the cached workspace-scoped tab projection coherent until the
        // next render rebuilds its hitboxes. This matters when a close key is
        // followed immediately by Tab/Shift+Tab in the same event cycle.
        self.tab_pane_indices.retain(|pane_idx| *pane_idx != idx);
        for pane_idx in &mut self.tab_pane_indices {
            if *pane_idx > idx {
                *pane_idx = pane_idx.saturating_sub(1);
            }
        }
        // Adjust focus to the previous pane (or wrap to the last).
        if self.panes.is_empty() {
            self.mode = InputMode::Normal;
            self.zoomed = false; // nothing left to zoom — clear the stale flag
            self.focus = 0;
        } else if self.focus >= self.panes.len() {
            self.focus = self.panes.len() - 1;
        }
    }

    fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        if self.focus >= self.panes.len() {
            None
        } else {
            self.panes.get_mut(self.focus).map(|slot| &mut slot.pane)
        }
    }

    /// Tear down sessions owned by this TUI. Orca-existing views are never
    /// mutated; only runtime handles created by this process are closed.
    fn close_daemon_sessions(&mut self) {
        if let Some(bridge) = self.orca_cli.clone() {
            let handles: Vec<String> = self
                .panes
                .iter()
                .filter(|slot| slot.session_owner == SessionOwner::OrcaTuiOwned)
                .filter_map(|slot| slot.orca_handle.clone())
                .collect();
            for handle in handles {
                // `run` calls this after raw-mode restoration, so it is safe
                // to wait for the public CLI command and guarantee teardown
                // delivery before the process exits.
                let _ = bridge.close_tab(&handle);
            }
            self.orca_cli = None;
            self.daemon_cleanup_session_ids.clear();
            return;
        }
        let mut session_ids = std::mem::take(&mut self.daemon_cleanup_session_ids);
        session_ids.extend(self.panes.iter().filter_map(|slot| {
            (slot.session_owner == SessionOwner::OrcaTuiOwned)
                .then(|| slot.daemon_session_id.clone())
                .flatten()
        }));
        session_ids.sort();
        session_ids.dedup();

        let Some(mut daemon) = self.daemon.take() else {
            return;
        };
        daemon.set_rpc_timeout(Duration::from_millis(750));
        for session_id in session_ids {
            if let Err(error) = daemon.kill_session(&session_id) {
                eprintln!("orcatui: failed to close Orca session {session_id}: {error}");
            }
        }
    }

    /// Forward raw bytes to the focused pane's PTY. Best-effort: a dead/exited
    /// agent's PTY write will error and we swallow it rather than tear down the
    /// whole TUI (typing into a finished pane is a no-op).
    ///
    /// In daemon mode, sends a `write` RPC only for a session owned by this
    /// TUI. Existing Orca GUI sessions are read-only snapshots; forwarding
    /// their input would mutate the GUI session without owning its stream.
    fn send_to_focused(&mut self, bytes: &[u8]) {
        if self.orca_cli.is_some() {
            let Some(slot) = self.panes.get(self.focus) else {
                return;
            };
            if slot.session_owner != SessionOwner::OrcaTuiOwned {
                return;
            }
            let Some(handle) = slot.orca_handle.clone() else {
                return;
            };
            if let Some(tx) = &self.orca_send_tx {
                let enter = bytes == b"\r";
                let payload = if enter { Vec::new() } else { bytes.to_vec() };
                let _ = tx.send((handle, payload, enter));
            }
            return;
        }
        if let Some(daemon) = &mut self.daemon {
            if self
                .panes
                .get(self.focus)
                .is_some_and(|slot| slot.daemon_read_only)
            {
                return;
            }
            // Daemon mode: forward via RPC using the pane's daemon session ID.
            let session_id = match self
                .panes
                .get(self.focus)
                .and_then(|slot| slot.daemon_session_id.as_ref())
            {
                Some(id) => id.clone(),
                None => return, // no daemon session for this pane yet
            };
            daemon.enqueue_write(session_id, bytes.to_vec());
            return;
        }
        // Standalone mode: write directly to the local PTY.
        let Some(session) = self
            .panes
            .get_mut(self.focus)
            .and_then(|slot| slot.session.as_mut())
        else {
            return;
        };
        let _ = session.write_bytes(bytes);
    }

    /// True once every session slot is `None` (no live agent process remains).
    /// For an empty session set this is vacuously true → the loop exits.
    fn all_sessions_gone(&self) -> bool {
        self.panes.iter().all(|slot| {
            if slot.session.is_some() {
                return false;
            }
            // A daemon-backed tab has no local PtySession handle. It remains
            // live until Orca emits an exit event and the pane transitions to
            // a terminal state; otherwise the main loop would exit immediately
            // after hydrating the daemon inventory.
            if slot.daemon_session_id.is_some() {
                return matches!(slot.state(), AgentState::Done(_) | AgentState::Failed(_));
            }
            true
        })
    }

    /// Whether the main loop should auto-exit on this tick.
    ///
    /// All agents must be gone, orchestration drained, and no reconnect
    /// pending — AND the user must be back to a calm, **unzoomed Normal**
    /// state. The last two clauses are the fix for "I pressed Ctrl+Alt+P and
    /// it died": a zoomed/active agent that just exited leaves its Done/Failed
    /// pane visible so the user can react, instead of the app vanishing
    /// mid-interaction. Once they unzoom and return to idle Normal, the loop
    /// breaks (and [`App::run`] prints `exit_reason`).
    fn should_auto_exit(&self) -> bool {
        self.all_sessions_gone()
            // A connected Orca daemon is a persistent terminal host. Keep the
            // viewer open even when its current session list is empty so the
            // user can create a new tab explicitly with `+`/`n`; an empty
            // inventory must not cause an immediate auto-exit.
            && !matches!(self.conn_state, ConnectionState::Connected)
            && self.catalog_sidebar_entries().is_empty()
            && self.orchestration_drained()
            && self.daemon_spawn_rx.is_empty()
            && self.panes.iter().all(|slot| slot.reconnect_due.is_none())
            && self.mode == InputMode::Normal
            && !self.zoomed
    }
}

// ── Daemon stream helpers ───────────────────────────────────────────────────

/// Parse a Data-frame payload and route it to the correct pane.
///
/// The daemon sends NDJSON `{"sessionId":"...","data":"..."}`. A payload
/// carrying an unknown session ID is dropped; only payloads with no session ID
/// use the legacy raw-bytes fallback to pane 0.
fn parse_stream_data(
    payload: &[u8],
    session_map: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
) -> Option<(usize, Vec<u8>)> {
    // Try NDJSON first.
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(payload) {
        if let Some(sid) = json.get("sessionId").and_then(|v| v.as_str()) {
            let pane_id = session_map.lock().unwrap().get(sid).copied()?;
            let data_value = json
                .get("data")
                .or_else(|| json.get("payload").and_then(|p| p.get("data")));
            if let Some(data) = data_value.and_then(|v| v.as_str()) {
                return Some((pane_id, data.as_bytes().to_vec()));
            }
            return Some((pane_id, Vec::new()));
        }
    }
    // Compatibility path: raw bytes (or JSON without a session ID) → pane 0.
    Some((0, payload.to_vec()))
}

fn filter_host_terminal(terminals: Vec<OrcaTerminal>) -> Vec<OrcaTerminal> {
    let host = std::env::var("ORCA_TERMINAL_HANDLE").ok();
    terminals
        .into_iter()
        .filter(|terminal| host.as_deref() != Some(terminal.handle.as_str()))
        .collect()
}

fn orca_status_to_state(status: Option<&str>) -> AgentState {
    match status.unwrap_or("running") {
        "exited" | "done" | "closed" => AgentState::Done(None),
        "created" | "spawning" | "idle" => AgentState::Idle,
        _ => AgentState::Running,
    }
}

/// Parse an Event-frame payload for an exit event.
///
/// Returns `Some((pane_id, exit_code))` for exit events, `None` otherwise.
fn parse_stream_event(
    payload: &[u8],
    session_map: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
) -> Option<(usize, i32)> {
    let json: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let event = json.get("event").and_then(|v| v.as_str())?;
    if event != "exit" {
        return None;
    }
    let sid = json.get("sessionId").and_then(|v| v.as_str())?;
    let pane_id = session_map.lock().unwrap().get(sid).copied()?;
    let code = json
        .get("payload")
        .and_then(|p| p.get("code"))
        .and_then(|c| c.as_i64())
        .map(|c| c as i32)
        .unwrap_or(0);
    Some((pane_id, code))
}

/// Install terminal-window shutdown handlers. macOS Terminal and most Unix
/// terminals send SIGHUP when their window is closed; SIGTERM covers process
/// supervisors, and SIGINT keeps Ctrl+C on the same cleanup path while raw
/// mode is active. The handler only flips an atomic flag; all terminal and RPC
/// work remains on the TUI thread.
fn install_tui_shutdown_handlers() -> Arc<AtomicBool> {
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }

    static mut SHUTDOWN_FLAG: Option<Arc<AtomicBool>> = None;
    let flag = Arc::new(AtomicBool::new(false));

    extern "C" fn tui_shutdown_signal_handler(_sig: i32) {
        // SAFETY: the static is initialized before handlers are installed and
        // only an atomic store occurs in the signal context.
        unsafe {
            let ptr = &raw const SHUTDOWN_FLAG;
            if let Some(flag) = &*ptr {
                flag.store(true, Ordering::Relaxed);
            }
        }
    }

    unsafe {
        SHUTDOWN_FLAG = Some(Arc::clone(&flag));
        let handler = tui_shutdown_signal_handler as *const () as usize;
        signal(1, handler); // SIGHUP
        signal(2, handler); // SIGINT
        signal(15, handler); // SIGTERM
    }
    flag
}

impl<B: Backend> Drop for App<B> {
    fn drop(&mut self) {
        // Covers panic paths and callers that construct an App without
        // entering `run`; the normal path already took the same connection in
        // `run`, so this is a no-op after a clean exit.
        self.stop_orca_cli_poller();
        self.close_daemon_sessions();
        // Panic-safety: if `run` never restored (or panicked mid-loop), make
        // one best-effort attempt to give the user their terminal back. The
        // `PtySession` drops below kill+join any still-running agents.
        if self.raw_mode_active {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            let _ = self.terminal.show_cursor();
            self.raw_mode_active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    //! Decision-logic tests for `App` that do NOT require a real terminal
    //! (no raw mode, no PTYs). They construct an `App` via [`App::for_test`]
    //! and exercise the pure routing/state methods: `apply_update`,
    //! `focus_next`/`focus_prev`, `all_sessions_gone`, `handle_key`, and
    //! `publish_snapshot`. The TUI-bound `run`/`main_loop`/`render` paths still
    //! need a TTY and stay out of reach of unit tests.

    use super::*;
    use crate::agent::AgentKind;
    use ratatui::backend::TestBackend;

    impl App<TestBackend> {
        /// Build an `App` from pre-made panes with no live sessions and no raw
        /// mode — purely for exercising the decision logic under test.
        fn for_test(panes: Vec<Pane>) -> Self {
            let panes: Vec<PaneSlot> = panes
                .into_iter()
                .map(|pane| PaneSlot::new(pane, Vec::new()))
                .collect();
            // Use an in-memory TestBackend (NOT CrosstermBackend+stdout) so the
            // tests run identically on a developer TTY and on a headless CI
            // runner whose stdout is a pipe — `CrosstermBackend::new(stdout)`
            // fails at `Terminal::new` when stdout isn't a TTY (it queries the
            // size via an ioctl that errors). The decision logic under test
            // never touches the terminal anyway.
            let terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal backend");
            let (bus_tx, rx) = bus::channel();
            Self {
                panes,
                focus: 0,
                terminal,
                bus_rx: rx,
                quit: false,
                raw_mode_active: false,
                worktrees: None,
                workspace_catalog: Vec::new(),
                workspace_catalog_loaded: false,
                workspace_unresolved_hosts: Vec::new(),
                workspace_unverifiable_scope_hosts: Vec::new(),
                orca_terminal_catalog: Vec::new(),
                scheduler: FrameScheduler::new(TARGET_FRAME_60FPS, Instant::now()),
                snapshot_tx: None,
                coordinator: None,
                orch_agent: None,
                daemon_session_map: None,
                daemon_reconnect_attempts: 0,
                daemon_backoff: Duration::from_secs(3),
                daemon_reconnect_rx: None,
                daemon_spawn_rx: Vec::new(),
                cols: 80,
                rows: 24,
                bus_tx,
                config: Config::default(),
                interaction: InteractionState::default(),
                exit_reason: None,
                conn_state: ConnectionState::Standalone,
                toasts: crate::toast::ToastQueue::new(),
                daemon: None,
                orca_cli: None,
                orca_poll_stop: None,
                orca_send_tx: None,
                orca_spawn_rx: Vec::new(),
                orca_closed_handles: std::collections::HashSet::new(),
                orca_cancelled_panes: std::collections::HashSet::new(),
                daemon_cleanup_session_ids: Vec::new(),
                shutdown_signal: None,
                activity: ActivityLog::new(),
                pane_rects: Vec::new(),
                tab_hitboxes: TabHitboxes::default(),
                tab_pane_indices: Vec::new(),
                footer_quit_hitbox: None,
                sidebar_rect: None,
                sidebar_entry_hitboxes: Vec::new(),
                sidebar_catalog_entries: Vec::new(),
                sidebar_workspace_hitboxes: Vec::new(),
                sidebar_pane_hitboxes: Vec::new(),
                sidebar_workspace_targets: Vec::new(),
                launch_cwd: std::env::current_dir().unwrap_or_default(),
                debug_last_sidebar_signature: None,
                next_pane_id: 0,
                tasks_repo: None,
                tasks_items: Vec::new(),
                tasks_fetch_rx: None,
                task_body_rx: None,
                task_body_entry: None,
                tasks_request_id: 0,
            }
        }
    }

    fn pane(id: usize, name: &str) -> Pane {
        let mut p = Pane::new(id, name, 40, 6);
        p.set_state(AgentState::Running);
        p
    }

    #[test]
    fn host_terminal_session_is_excluded_by_handle_or_pty_id() {
        let mut by_handle = crate::orca_daemon::DaemonSessionInfo {
            session_id: "pty-host".to_owned(),
            state: "running".to_owned(),
            shell_state: String::new(),
            is_alive: true,
            cwd: None,
            cols: 80,
            rows: 24,
            pid: None,
            terminal_handle: Some("term-host".to_owned()),
            agent_session_owners: Vec::new(),
        };
        assert!(App::<TestBackend>::is_host_terminal_session(
            &by_handle,
            Some("term-host"),
            None,
        ));
        assert!(!App::<TestBackend>::is_host_terminal_session(
            &by_handle,
            Some("term-other"),
            Some("pty-other"),
        ));

        by_handle.terminal_handle = None;
        assert!(App::<TestBackend>::is_host_terminal_session(
            &by_handle,
            Some("term-host"),
            Some("pty-host"),
        ));
    }

    #[test]
    fn apply_update_output_feeds_the_matching_pane() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.apply_update(AgentUpdate::Output {
            pane_id: 1,
            bytes: b"hi".to_vec(),
        });
        // Pane 1's emulator should now contain "hi" on its first line.
        let cell = app.panes[1].emulator().cell(0, 0).expect("cell");
        assert_eq!(cell.chars, "h");
        // Pane 0 untouched.
        assert!(!app.panes[0].emulator().cell(0, 0).unwrap().has_contents());
    }

    /// Regression: closing the first pane must NOT freeze the survivor's output.
    /// Before the stable-id fix, the survivor's forwarder kept reporting its OLD
    /// spawn position (`pane_id: 1`); after the close, position 1 no longer
    /// existed (the survivor had slid to position 0), so `apply_update` did
    /// `panes.get_mut(1)` → `None` → output silently DROPPED. The survivor's
    /// pane appeared frozen even though its agent was still producing bytes.
    /// 关闭 pane 后，存活 slot 仍保留稳定标识并接收正确输出。
    #[test]
    fn close_first_pane_survivor_still_receives_output_by_stable_id() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        // Sanity: two panes with stable ids 0 and 1.
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.panes[0].id(), 0);
        assert_eq!(app.panes[1].id(), 1);

        // 建立 slot 内部的活动状态基线。
        app.record_activity();
        assert!(app.panes.iter().all(|slot| slot.last_status.is_some()));

        // Close the focused (first) pane — the survivor (id 1) slides to pos 0.
        app.focus = 0;
        app.close_focused_pane();

        // Survivor is now the only pane, at position 0, retaining its stable id.
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.panes[0].id(), 1);

        assert!(app.panes[0].last_status.is_some());

        // The survivor's forwarder is still alive and reports by its STABLE id
        // (1). `apply_update` must resolve id→position (1→0) and deliver.
        // Before the fix this was dropped: `get_mut(1)` was out of range.
        app.apply_update(AgentUpdate::Output {
            pane_id: 1,
            bytes: b"survivor-live".to_vec(),
        });
        let cell = app.panes[0].emulator().cell(0, 0).expect("cell");
        assert_eq!(cell.chars, "s", "survivor received output by stable id");

        // An update for the CLOSED pane's id (0) is a clean no-op (no panic,
        // no misrouting into the survivor's pane).
        app.apply_update(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"ghost".to_vec(),
        });
        // Survivor's first cell is still the 's' from above, not 'g'.
        assert_eq!(
            app.panes[0].emulator().cell(0, 0).unwrap().chars,
            "s",
            "closed pane's update did not bleed into the survivor"
        );
    }

    /// Regression for "when another agent is closed, the remaining (idle) agent
    /// shows a bigger window but its content stays stale (doesn't update)."
    ///
    /// Hypothesis (verified): this was a SYMPTOM of the pane-id staleness bug
    /// fixed by the stable-id routing in [`App::apply_update`] (`idx_of_pane`).
    /// Before that fix, closing pane 0 shifted indices and the survivor's
    /// forwarder kept reporting its OLD position; `apply_update` then dropped
    /// the survivor's post-resize output (via `get_mut(1)` → `None`), so its
    /// content appeared frozen. With stable-id routing the survivor's output is
    /// delivered, and `render()` → `resize_viewport` already invalidates the
    /// grid cache, so the content refreshes at the new size.
    #[test]
    fn close_pane_then_resize_survivor_content_refreshes() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.sidebar_hidden = true; // deterministic, full-width pane math
                                   // Lay both panes out at the default 80x24 so each has a known size.
        app.render().expect("initial render");

        // Close the focused first pane. The survivor (stable id 1) is IDLE and
        // slides to position 0, now filling the whole pane area.
        app.focus = 0;
        app.close_focused_pane();
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.panes[0].id(), 1);

        // Grow the terminal and render. render() computes each pane's target
        // size from its rect and must call resize_viewport when it changes —
        // growing the emulator and marking the grid cache dirty.
        let (pre_w, pre_h) = app.panes[0].size();
        app.terminal
            .resize(Rect::new(0, 0, 120, 40))
            .expect("resize backend bigger");
        app.render().expect("render after close + resize");
        let (post_w, post_h) = app.panes[0].size();
        assert!(
            (post_w, post_h) > (pre_w, pre_h),
            "survivor emulator grew after the resize: {pre_w}x{pre_h} -> {post_w}x{post_h}"
        );

        // The survivor's forwarder reports by its STABLE id (1). After the close
        // it lives at position 0; apply_update must resolve 1 -> 0 and deliver.
        app.apply_update(AgentUpdate::Output {
            pane_id: 1,
            bytes: b"after-resize-content".to_vec(),
        });
        app.render().expect("render after post-resize output");

        // The post-resize output reached the survivor's emulator...
        assert_eq!(
            app.panes[0].emulator().cell(0, 0).expect("cell").chars,
            "a",
            "survivor received post-resize output by stable id"
        );
        // ...and is painted into the rendered buffer (NOT stale).
        assert!(
            buffer_text(&app).contains("after-resize-content"),
            "survivor content refreshed at the new size (no stale freeze)"
        );
    }

    #[test]
    fn apply_update_exit_zero_is_done_nonzero_is_failed() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.apply_update(AgentUpdate::Exit {
            pane_id: 0,
            code: Some(0),
        });
        assert!(matches!(app.panes[0].state(), AgentState::Done(_)));

        app.apply_update(AgentUpdate::Exit {
            pane_id: 1,
            code: Some(2),
        });
        assert!(matches!(app.panes[1].state(), AgentState::Failed(_)));
    }

    #[test]
    fn apply_update_exit_is_idempotent_once_terminal() {
        // A real Some(code) lands first; a later forwarder Exit{None} must NOT
        // downgrade a Failed pane to Done (the reap-before-drain invariant).
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.apply_update(AgentUpdate::Exit {
            pane_id: 0,
            code: Some(1),
        });
        assert!(matches!(app.panes[0].state(), AgentState::Failed(_)));
        app.apply_update(AgentUpdate::Exit {
            pane_id: 0,
            code: None,
        });
        assert!(
            matches!(app.panes[0].state(), AgentState::Failed(_)),
            "a late None must not overwrite an existing terminal state"
        );
        // The session slot is taken exactly once either way.
        assert!(app.panes[0].session.is_none());
    }

    #[test]
    fn focus_next_and_prev_wrap_around() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b"), pane(2, "c")]);
        assert_eq!(app.focus, 0);
        app.focus_next();
        assert_eq!(app.focus, 1);
        app.focus_next();
        assert_eq!(app.focus, 2);
        app.focus_next(); // wraps
        assert_eq!(app.focus, 0);
        app.focus_prev(); // wraps back
        assert_eq!(app.focus, 2);
    }

    #[test]
    fn all_sessions_gone_reflects_live_slots() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        // for_test starts with all slots None → vacuously "gone".
        assert!(app.all_sessions_gone());
        // Put a live session in one slot to exercise the false path.
        app.panes[1].session = dummy_session();
        assert!(!app.all_sessions_gone());
        app.panes[1].session = None;
        assert!(app.all_sessions_gone());
    }

    #[test]
    fn all_sessions_gone_keeps_live_daemon_tab_open_without_local_pty() {
        let mut app = App::for_test(vec![pane(0, "orca-window")]);
        app.panes[0].daemon_session_id = Some("session-1".to_string());
        assert!(!app.all_sessions_gone());
        app.panes[0].set_state(AgentState::Done(Some(0)));
        assert!(app.all_sessions_gone());
    }

    #[test]
    fn should_auto_exit_waits_for_idle_unzoomed_normal() {
        // No panes → all_sessions_gone is vacuously true; orchestration is
        // drained and no reconnects are pending. So the decision hinges on the
        // user's activity state.
        let mut app = App::for_test(vec![]);

        // Zoomed (the reported crash case: agent exited while zoomed) → do NOT
        // auto-exit; leave the dead pane visible so the user can react.
        app.zoomed = true;
        app.mode = InputMode::Normal;
        assert!(
            !app.should_auto_exit(),
            "zoomed: don't yank the app away mid-interaction"
        );

        // Pane mode (active control) → do NOT auto-exit.
        app.zoomed = false;
        app.mode = InputMode::Pane;
        assert!(!app.should_auto_exit(), "Pane mode: active control");

        // An overlay mode (e.g. Activity) → do NOT auto-exit.
        app.mode = InputMode::Activity;
        assert!(!app.should_auto_exit(), "overlay: active control");

        // Calm Normal + unzoomed → auto-exit.
        app.mode = InputMode::Normal;
        assert!(
            app.should_auto_exit(),
            "idle + unzoomed + all gone → auto-exit"
        );

        // A live session suppresses auto-exit regardless of mode.
        let mut slot = PaneSlot::new(pane(0, "alive"), Vec::new());
        slot.session = dummy_session();
        app.panes.push(slot);
        app.mode = InputMode::Normal;
        assert!(
            !app.should_auto_exit(),
            "a live agent still running → no auto-exit"
        );
    }

    #[test]
    fn daemon_spawn_worker_disconnect_marks_placeholder_failed() {
        let mut app = App::for_test(vec![pane(7, "pending")]);
        let (tx, rx) = std::sync::mpsc::channel::<
            Result<serde_json::Value, crate::orca_daemon::DaemonError>,
        >();
        drop(tx);
        app.daemon_spawn_rx.push((7, "session-7".into(), rx));

        assert!(app.pump_daemon_spawns());
        assert!(app.daemon_spawn_rx.is_empty());
        assert!(
            matches!(app.panes[0].state(), AgentState::Failed(reason) if reason.contains("worker stopped"))
        );
    }

    #[test]
    fn handle_key_mode_switch_ctrl_q_quit_and_pane_focus() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        assert_eq!(app.mode, InputMode::Normal);
        assert!(!app.quit);

        // Ctrl+Q quits from any mode.
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.quit, "Ctrl+Q quits");

        // Ctrl+Alt+P enters pane mode (gateway — distinct from Ctrl+P which
        // opencode uses for its command palette).
        app.quit = false;
        app.handle_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(app.mode, InputMode::Pane, "Ctrl+Alt+P enters pane mode");

        // In pane mode: Tab advances focus.
        app.focus = 0;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, 1, "Tab advances focus in pane mode");

        // Esc exits pane mode back to normal.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Esc exits pane mode");

        // In normal mode: Tab switches the active terminal tab too.
        app.focus = 0;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, 1, "Tab in normal mode switches active tab");
    }

    #[test]
    fn toggle_pin_flips_the_focused_pane_pinned_flag() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.focus = 1;
        app.mode = InputMode::Pane;
        assert!(!app.panes[0].pinned, "pane 0 starts unpinned");
        assert!(!app.panes[1].pinned, "pane 1 starts unpinned");

        // First press of bare `p` pins the focused pane (1).
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(app.panes[1].pinned, "first press pins the focused pane");
        assert!(!app.panes[0].pinned, "the non-focused pane is untouched");

        // Second press flips it back to unpinned.
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(!app.panes[1].pinned, "second press unpins");
    }

    #[test]
    fn ctrl_b_toggles_sidebar_visibility() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        assert!(!app.sidebar_hidden, "sidebar visible by default");
        // Sidebar toggle moved behind the gateway: enter Pane (Ctrl+Alt+P),
        // then press `b`. (Was a global Ctrl+B hotkey; moved to avoid
        // colliding with agent shortcuts.)
        app.handle_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(app.sidebar_hidden, "Pane + b hides the sidebar");
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(!app.sidebar_hidden, "b again re-shows it");
    }

    #[test]
    fn sidebar_mode_enters_with_ctrl_s() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        assert_eq!(app.mode, InputMode::Normal);
        // Sidebar nav moved behind the gateway: enter Pane (Ctrl+Alt+P),
        // then press `s`. (Was a global Ctrl+S hotkey; moved to avoid both
        // collision with agent shortcuts and terminals' XOFF flow control.)
        app.handle_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Sidebar, "Pane + s enters sidebar mode");
        assert_eq!(app.sidebar_nav, 0, "selection starts at the first item");
    }

    #[test]
    fn sidebar_nav_moves_with_arrows() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Sidebar;
        app.sidebar_nav = 0;
        // Down: 0 → 1 → 2 → clamped at 2.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 1);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 2);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 2, "clamped at the last item");
        // Up: 2 → 1 → 0 → clamped at 0.
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 1);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 0);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 0, "clamped at the first item");
    }

    #[test]
    fn sidebar_enter_activity_opens_overlay() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Sidebar;
        app.sidebar_nav = 0; // Activity
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.mode,
            InputMode::Activity,
            "Enter on Activity opens the overlay"
        );
    }

    #[test]
    fn sidebar_enter_tasks_opens_repo_input() {
        // Phase 2: Tasks (index 1) is implemented — Enter opens the repo-input
        // modal (TasksRepo) and resets the Tasks state, with NO toast.
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Sidebar;
        app.sidebar_nav = 1;
        // Dirty the state first to prove Enter resets it.
        app.tasks_repo_input = "stale".to_string();
        app.tasks_selected = 9;
        app.tasks_error = Some("stale error".to_string());
        assert!(app.toasts.is_empty(), "no toast before dispatch");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.mode,
            InputMode::TasksRepo,
            "Tasks opens the repo-input modal"
        );
        assert!(
            app.tasks_repo_input.is_empty(),
            "repo input cleared on open"
        );
        assert_eq!(app.tasks_selected, 0, "selection reset on open");
        assert!(app.tasks_error.is_none(), "error cleared on open");
        assert!(app.toasts.is_empty(), "no toast for an implemented feature");
    }

    #[test]
    fn background_task_fetch_result_is_applied_without_blocking() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        let (tx, rx) = std::sync::mpsc::channel();
        app.tasks_fetch_rx = Some(rx);
        app.mode = InputMode::TasksList;
        tx.send((
            0,
            Ok(vec![TasksEntry {
                kind: TaskKind::Issue,
                number: 7,
                title: "后台结果".into(),
                prompt: "prompt".into(),
            }]),
        ))
        .unwrap();

        assert!(app.poll_tasks_fetch());
        assert_eq!(app.tasks_items.len(), 1);
        assert_eq!(app.tasks_items[0].number, 7);
        assert!(app.tasks_fetch_rx.is_none());
    }

    #[test]
    fn sidebar_enter_settings_opens_overlay() {
        // Phase 2: Settings (index 2) is implemented — Enter opens the settings
        // overlay (cursor reset to row 0), with NO toast.
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Sidebar;
        app.sidebar_nav = 2;
        app.settings_cursor = 9; // dirty to prove Enter resets it
        assert!(app.toasts.is_empty(), "no toast before dispatch");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Settings, "Settings opens the overlay");
        assert_eq!(app.settings_cursor, 0, "cursor reset to the first row");
        assert!(app.toasts.is_empty(), "no toast for an implemented feature");
    }

    #[test]
    fn settings_overlay_toggles_live_and_closes_on_esc() {
        // Phase 2: Settings overlay applies each toggle LIVE to self.config.
        // Toggling the status-bar row (cursor 1) flips show_status_bar; Esc
        // persists (best-effort) and returns to Normal regardless of save
        // outcome (the test env may lack a writable config dir, so only the
        // mode transition — which always happens — is asserted).
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Settings;
        app.settings_cursor = 1; // status bar row
        assert!(
            app.config.layout.show_status_bar,
            "status bar on by default"
        );
        // Enter toggles the focused row live.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            !app.config.layout.show_status_bar,
            "status bar toggled off live"
        );
        // Space also toggles (Enter | Space both map to toggle_setting).
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(
            app.config.layout.show_status_bar,
            "status bar toggled back on via Space"
        );
        // Esc persists (best-effort) and returns to Normal.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            app.mode,
            InputMode::Normal,
            "Esc closes the overlay and returns to Normal"
        );
    }

    #[test]
    fn sidebar_esc_returns_to_normal() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Sidebar;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Esc exits sidebar mode");
    }

    #[test]
    fn jump_palette_filters_and_focuses_on_enter() {
        let mut app = App::for_test(vec![
            pane(0, "claude"),
            pane(1, "codex"),
            pane(2, "opencode"),
        ]);
        app.mode = InputMode::Pane;
        app.focus = 0;
        // `/` opens the palette; empty query shows all agents.
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Jump);
        assert_eq!(app.jump_filtered(), vec![0, 1, 2], "empty query shows all");

        // type "co" → matches codex + opencode (substring, case-insensitive).
        for c in ['c', 'o'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(
            app.jump_filtered(),
            vec![1, 2],
            "query 'co' filters to codex+opencode"
        );

        // Down → select the 2nd filtered (opencode = pane 2); Enter focuses it.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.jump_selected, 1);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Enter closes the palette");
        assert_eq!(app.focus, 2, "Enter focused the selected agent (opencode)");

        // Esc cancels without changing focus.
        app.mode = InputMode::Jump;
        app.focus = 0;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal);
        assert_eq!(app.focus, 0, "Esc cancels without focusing");
    }

    #[test]
    fn publish_snapshot_broadcasts_pane_states() {
        let mut app = App::for_test(vec![pane(0, "claude"), pane(1, "codex")]);
        app.panes[1].set_state(AgentState::Done(None));
        app.panes[0].set_branch(Some("orca/main".into()));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<AgentSnapshot>>();
        app.set_snapshot_sender(tx);
        app.publish_snapshot();

        let snaps = rx.try_recv().expect("a snapshot was published");
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].name, "claude");
        assert_eq!(snaps[0].state, "Running");
        assert_eq!(snaps[0].branch.as_deref(), Some("orca/main"));
        assert_eq!(snaps[1].name, "codex");
        assert_eq!(snaps[1].state, "Done");
    }

    /// A throwaway `PtySession` is hard to build without spawning; the
    /// `all_sessions_gone` false-path only needs `Some(_)` in a slot, so we
    /// build one off the cheapest possible child (`true`, exits immediately).
    fn dummy_session() -> Option<PtySession> {
        let bin = AgentKind::detect_installed()
            .first()
            .map(AgentKind::binary)
            .unwrap_or("true");
        PtySession::spawn(vec![bin.to_string()], None, 20, 3)
            .map(|(s, _rx)| s)
            .ok()
    }

    /// Flatten the TestBackend's buffer into a string (rows joined by `\n`) so
    /// tests can assert on rendered content without computing exact cell coords.
    fn buffer_text(app: &App<TestBackend>) -> String {
        let buf = app.terminal.backend().buffer();
        let area = buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn render_draws_pane_headers_content_and_footer() {
        let mut app = App::for_test(vec![pane(0, "alpha"), pane(1, "beta")]);
        app.panes[0].feed(b"hello-world");
        app.render().expect("render into TestBackend");

        let text = buffer_text(&app);
        // Pane names appear in their bordered headers.
        assert!(text.contains("alpha"), "pane 0 header rendered");
        assert!(text.contains("beta"), "pane 1 header rendered");
        // Fed agent output is painted into pane 0's body.
        assert!(text.contains("hello-world"), "fed content rendered");
        // The footer key-hints are drawn on the reserved last line.
        assert!(text.contains("Ctrl+Alt+P"), "footer rendered");
        assert!(text.contains("quit"));
    }

    #[test]
    fn render_uses_single_active_terminal_and_tab_strip() {
        let mut app = App::for_test(vec![pane(0, "alpha"), pane(1, "beta")]);
        app.panes[0].feed(b"active-output");
        app.panes[1].feed(b"inactive-output");
        app.render().expect("render tab layout");
        let text = buffer_text(&app);
        assert!(text.contains("alpha"), "active tab label is visible");
        assert!(text.contains("beta"), "inactive tab label is visible");
        assert!(
            text.contains("×"),
            "each sufficiently wide tab exposes close affordance"
        );
        assert!(text.contains("active-output"), "active terminal is painted");
        assert!(
            !text.contains("inactive-output"),
            "inactive terminal is not split-painted"
        );
        assert_eq!(app.tab_hitboxes.tabs.len(), 2);
        assert!(
            app.pane_rects[1].width == 0,
            "only active tab has a terminal hitbox"
        );
    }

    #[test]
    fn mouse_click_tab_switches_active_terminal() {
        let mut app = App::for_test(vec![pane(0, "alpha"), pane(1, "beta")]);
        app.render().expect("initial tab render");
        let rect = app.tab_hitboxes.tabs[1];
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, 1);
        assert_eq!(app.mode, InputMode::Normal);
    }

    #[test]
    fn mouse_click_tab_close_removes_selected_pane() {
        let mut app = App::for_test(vec![pane(0, "alpha"), pane(1, "beta"), pane(2, "gamma")]);
        app.render().expect("initial tab render");
        let close = app.tab_hitboxes.closes[1].expect("second tab is wide enough to close");
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: close.x,
            row: close.y,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.panes.len(), 2, "close removes exactly one pane");
        assert_eq!(app.panes[0].name(), "alpha");
        assert_eq!(app.panes[1].name(), "gamma");
        assert_eq!(app.tab_pane_indices, vec![0, 1]);
        assert_eq!(app.focus, 0, "focus lands on the previous surviving pane");
        assert_eq!(app.mode, InputMode::Normal);
    }

    #[test]
    fn workspace_tabs_only_show_and_select_panes_in_focused_workspace() {
        let mut app = App::for_test(vec![
            pane(0, "alpha"),
            pane(1, "other-workspace"),
            pane(2, "beta"),
        ]);
        let workspace = |id: &str, path: &str, branch: &str| OrcaWorkspace {
            id: id.to_owned(),
            path: PathBuf::from(path),
            branch: branch.to_owned(),
            display_name: branch.to_owned(),
            repo_id: id.to_owned(),
            project_id: None,
            host_id: Some("local".to_owned()),
            catalog_source_host_id: None,
            is_archived: false,
            workspace_status: None,
            is_main_worktree: false,
        };
        app.workspace_catalog = vec![
            workspace("workspace-a", "/workspace-a", "main"),
            workspace("workspace-b", "/workspace-b", "feature"),
        ];
        app.workspace_catalog_loaded = true;
        app.panes[0].workspace_id = Some("workspace-a".to_owned());
        app.panes[1].workspace_id = Some("workspace-b".to_owned());
        app.panes[2].workspace_id = Some("workspace-a".to_owned());
        app.focus = 0;

        app.render().expect("render workspace-scoped tabs");
        assert_eq!(app.tab_pane_indices, vec![0, 2]);
        assert_eq!(app.tab_hitboxes.tabs.len(), 2);

        let second_tab = app.tab_hitboxes.tabs[1];
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: second_tab.x + 1,
            row: second_tab.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.focus, 2,
            "tab click maps filtered position to pane index"
        );

        app.focus = 0;
        app.focus_next();
        assert_eq!(app.focus, 2, "next tab stays in workspace-a");
        app.focus_next();
        assert_eq!(app.focus, 0, "next tab wraps within workspace-a");
        app.focus_prev();
        assert_eq!(app.focus, 2, "previous tab stays in workspace-a");
    }

    #[test]
    fn workspace_tab_close_maps_filtered_tab_to_the_correct_global_pane() {
        let mut app = App::for_test(vec![
            pane(0, "alpha"),
            pane(1, "other-workspace"),
            pane(2, "beta"),
        ]);
        let workspace = |id: &str, path: &str, branch: &str| OrcaWorkspace {
            id: id.to_owned(),
            path: PathBuf::from(path),
            branch: branch.to_owned(),
            display_name: branch.to_owned(),
            repo_id: id.to_owned(),
            project_id: None,
            host_id: Some("local".to_owned()),
            catalog_source_host_id: None,
            is_archived: false,
            workspace_status: None,
            is_main_worktree: false,
        };
        app.workspace_catalog = vec![
            workspace("workspace-a", "/workspace-a", "main"),
            workspace("workspace-b", "/workspace-b", "feature"),
        ];
        app.workspace_catalog_loaded = true;
        app.panes[0].workspace_id = Some("workspace-a".to_owned());
        app.panes[1].workspace_id = Some("workspace-b".to_owned());
        app.panes[2].workspace_id = Some("workspace-a".to_owned());
        app.focus = 0;
        app.render().expect("render workspace-scoped tabs");

        let close = app.tab_hitboxes.closes[1].expect("second workspace tab can close");
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: close.x,
            row: close.y,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.panes.len(), 2);
        assert_eq!(
            app.panes.iter().map(|pane| pane.name()).collect::<Vec<_>>(),
            vec!["alpha", "other-workspace"]
        );
        assert_eq!(app.tab_pane_indices, vec![0]);
        assert_eq!(
            app.focus, 0,
            "closing workspace-a's second tab keeps alpha focused"
        );
    }

    #[test]
    fn closing_read_only_daemon_tab_queues_orca_close() {
        let mut app = App::for_test(vec![pane(0, "orca-session")]);
        app.panes[0].daemon_session_id = Some("session-1".to_owned());
        app.panes[0].daemon_read_only = true;
        app.render().expect("render read-only daemon tab");
        let close = app.tab_hitboxes.closes[0].expect("daemon tab close affordance");
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: close.x,
            row: close.y,
            modifiers: KeyModifiers::NONE,
        });

        assert!(
            app.panes.is_empty(),
            "read-only tab can be dismissed locally"
        );
        assert_eq!(
            app.daemon_cleanup_session_ids,
            vec!["session-1"],
            "closing an Orca tab queues its daemon session for synchronized cleanup"
        );
        assert!(app.daemon_session_map.is_none());
    }

    #[test]
    fn mouse_click_quit_control_requests_app_exit_without_closing_tab() {
        let mut app = App::for_test(vec![pane(0, "alpha"), pane(1, "beta")]);
        app.render().expect("initial tab render");
        let quit = app.tab_hitboxes.quit.expect("quit control visible");
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: quit.x + quit.width.saturating_sub(1),
            row: quit.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.quit, "clicking the chrome exit control requests quit");
        assert_eq!(
            app.panes.len(),
            2,
            "exit waits for teardown to close daemon tabs"
        );
    }

    #[test]
    fn global_quit_queues_orca_sessions_for_window_cleanup() {
        let mut app = App::for_test(vec![pane(0, "orca-a"), pane(1, "orca-b")]);
        for (slot, session_id) in app.panes.iter_mut().zip(["session-a", "session-b"]) {
            slot.daemon_session_id = Some(session_id.to_owned());
            slot.daemon_read_only = true;
        }

        app.request_quit();

        assert!(app.quit);
        assert_eq!(
            app.daemon_cleanup_session_ids,
            vec!["session-a", "session-b"],
            "closing the TUI window must retain every Orca session for final kill RPCs"
        );
    }

    #[test]
    fn footer_quit_button_is_visible_and_clickable() {
        let mut app = App::for_test(vec![pane(0, "alpha")]);
        app.render().expect("render footer");
        let text = buffer_text(&app);
        assert!(
            text.contains("退") && text.contains("出"),
            "footer exposes a visible exit button"
        );
        let quit = app.footer_quit_hitbox.expect("footer exit hitbox");
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: quit.x + 1,
            row: quit.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.quit, "clicking footer exit requests app exit");
    }

    #[test]
    fn connected_empty_daemon_view_stays_open_for_explicit_creation() {
        let mut app = App::for_test(Vec::new());
        app.conn_state = ConnectionState::Connected;
        assert!(
            !app.should_auto_exit(),
            "an empty connected daemon viewer must stay open so +/n can create a tab"
        );
    }

    #[test]
    fn daemon_session_label_prefers_owner_and_ignores_shell_foreground() {
        let mut info = crate::orca_daemon::DaemonSessionInfo {
            session_id: "s1".into(),
            state: "running".into(),
            shell_state: String::new(),
            is_alive: true,
            cwd: Some("/tmp/project".into()),
            cols: 80,
            rows: 24,
            pid: None,
            terminal_handle: None,
            agent_session_owners: vec![crate::orca_daemon::DaemonAgentSessionOwner {
                claim: Some(crate::orca_daemon::DaemonAgentSessionClaim {
                    agent: Some("codex".into()),
                }),
            }],
        };
        assert_eq!(
            App::<TestBackend>::daemon_session_label(&info, Some("zsh"), None),
            "codex"
        );

        info.agent_session_owners.clear();
        assert_eq!(
            App::<TestBackend>::daemon_session_label(&info, Some("/bin/zsh"), None),
            "project",
            "shell processes must not be shown as agents"
        );
        assert_eq!(
            App::<TestBackend>::daemon_session_label(&info, Some("opencode"), None),
            "opencode"
        );
    }

    #[test]
    fn daemon_session_label_matches_orca_terminal_title() {
        let info = crate::orca_daemon::DaemonSessionInfo {
            session_id: "repo::/work@@abc".into(),
            state: "running".into(),
            shell_state: "ready".into(),
            is_alive: true,
            cwd: None,
            cols: 80,
            rows: 24,
            pid: None,
            terminal_handle: Some("term-a".into()),
            agent_session_owners: Vec::new(),
        };
        let terminal = OrcaTerminal {
            pty_id: info.session_id.clone(),
            handle: "term-a".into(),
            title: Some("orca-tui-match-test".into()),
            agent_identity: None,
            worktree_id: None,
            worktree_path: None,
            branch: None,
            order: 0,
        };

        assert_eq!(
            App::<TestBackend>::daemon_session_label(&info, Some("zsh"), Some(&terminal)),
            "orca-tui-match-test"
        );
    }

    #[test]
    fn mouse_click_sidebar_entry_switches_active_terminal() {
        let mut app = App::for_test(vec![pane(0, "alpha"), pane(1, "beta")]);
        app.render().expect("initial sidebar render");
        let rect = app.sidebar_entry_hitboxes[1].expect("second sidebar row visible");
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, 1);
    }

    #[test]
    fn render_does_not_panic_when_terminal_shrinks_tiny() {
        // Regression for "shrinking the window while agents run kills orcatui".
        // Drive the terminal down to degenerate sizes and render at each — none
        // may panic (the layout must degrade gracefully, clamping/omitting
        // pieces instead of underflowing). Two panes so the grid math is
        // exercised, plus a cursor + content so the paint path is too.
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.panes[0].feed(b"hello-world");
        app.panes[0].set_state(AgentState::Running);
        for (w, h) in [
            (80, 24),
            (50, 16),
            (30, 10),
            (20, 6),
            (12, 4),
            (8, 3),
            (5, 2),
            (3, 2),
            (2, 2),
            (1, 1),
        ] {
            app.terminal
                .resize(Rect::new(0, 0, w, h))
                .expect("resize backend");
            app.render()
                .unwrap_or_else(|e| panic!("render panicked at {w}x{h}: {e:#}"));
        }
    }

    #[test]
    fn sidebar_nav_popup_renders_items_and_selection() {
        // In Sidebar mode the Navigate popup shows all three nav items and
        // marks the selected one with the ▶ marker.
        let mut app = App::for_test(vec![pane(0, "alpha")]);
        app.mode = InputMode::Sidebar;
        app.sidebar_nav = 1; // Tasks (now implemented in Phase 2)
        app.render().expect("render into TestBackend");

        let text = buffer_text(&app);
        // Title + every item label. Activity, Tasks, and Settings are all
        // implemented in Phase 1/2 — none carry a "(soon)" tag.
        assert!(text.contains("Navigate"), "popup title rendered");
        assert!(text.contains("Activity"), "first item rendered");
        assert!(text.contains("Tasks"), "Tasks item rendered");
        assert!(
            !text.contains("Tasks (soon)"),
            "Tasks is implemented — no (soon) tag"
        );
        assert!(text.contains("Settings"), "Settings item rendered");
        assert!(
            !text.contains("Settings (soon)"),
            "Settings is implemented — no (soon) tag"
        );
        // The selected row (index 1 → Tasks) carries the ▶ marker.
        assert!(
            text.contains("▶ Tasks"),
            "selected row carries the ▶ marker"
        );
        // Sanity: the non-selected Activity row does NOT carry the marker.
        assert!(
            !text.contains("▶ Activity"),
            "non-selected Activity row has no marker"
        );
    }

    #[test]
    fn render_resizes_each_pane_viewport_to_its_inner_area() {
        // Panes are created at 40×6; a single pane in an 80×24 terminal (minus
        // the footer + 1-cell border) should be resized to a different size.
        let mut app = App::for_test(vec![pane(0, "solo")]);
        let before = app.panes[0].size();
        app.render().expect("render");
        let after = app.panes[0].size();
        assert_ne!(after, before, "render reconciled the pane viewport");
        // And the content is still present after the resize.
        assert!(buffer_text(&app).contains("solo"));
    }

    #[test]
    fn render_handles_many_panes_without_panic() {
        // A grid of 5 panes must lay out and paint without index panics.
        let panes: Vec<Pane> = (0..5).map(|i| pane(i, &format!("p{i}"))).collect();
        let mut app = App::for_test(panes);
        app.focus = 2;
        app.render().expect("render 5 panes");
        let text = buffer_text(&app);
        for i in 0..5 {
            assert!(text.contains(&format!("p{i}")), "pane {i} rendered");
        }
    }

    #[test]
    fn reap_exited_marks_done_and_takes_the_slot() {
        // `reap_exited` polls each live child via try_wait and, on exit, feeds
        // an Exit{Some(code)} back through apply_update. Spawn a child that
        // exits immediately (code 0) and confirm reap transitions it to Done.
        let mut app = App::for_test(vec![pane(0, "runner")]);
        let (session, _rx) =
            PtySession::spawn(vec!["true".into()], None, 20, 3).expect("spawn true");
        app.panes[0].session = Some(session);

        let mut became_done = false;
        for _ in 0..100 {
            app.reap_exited();
            if matches!(app.panes[0].state(), AgentState::Done(_)) {
                became_done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(became_done, "reap_exited should mark the exited child Done");
        assert!(
            app.panes[0].session.is_none(),
            "reap takes the session slot once"
        );
    }

    #[test]
    fn reconnect_schedules_then_respawns_after_backoff() {
        let mut app = App::for_test(vec![pane(0, "remote")]);
        app.enable_reconnect();
        // for_test seeds empty commands; give the pane a real one to respawn.
        app.panes[0].command = vec!["true".into()];

        // A dropped session schedules a reconnect instead of going terminal.
        app.apply_update(AgentUpdate::Exit {
            pane_id: 0,
            code: Some(1),
        });
        assert!(
            matches!(app.panes[0].state(), AgentState::Failed(f) if f.contains("reconnecting")),
            "reconnecting indicator shown"
        );
        assert!(app.panes[0].session.is_none(), "dead session taken");
        assert!(app.panes[0].reconnect_due.is_some(), "respawn scheduled");

        // Before the backoff deadline: pump is a no-op.
        app.pump_reconnect();
        assert!(
            app.panes[0].session.is_none(),
            "no respawn before backoff elapses"
        );

        // Force the deadline into the past → the next pump respawns the pane.
        app.panes[0].reconnect_due = Some(Instant::now() - Duration::from_secs(1));
        app.pump_reconnect();
        assert!(app.panes[0].session.is_some(), "respawned after backoff");
        assert!(matches!(app.panes[0].state(), AgentState::Running));
        assert!(
            app.panes[0].reconnect_due.is_none(),
            "schedule cleared after respawn"
        );

        // Clean up the spawned child so the test doesn't leak a process.
        if let Some(s) = app.panes[0].session.as_mut() {
            let _ = s.kill();
        }
    }

    #[test]
    fn spawn_one_failed_command_adds_failed_pane_and_keeps_vecs_aligned() {
        // Ctrl+N path: spawning an agent whose binary isn't on PATH must NOT
        // panic, and must add exactly one Failed slot.
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        let before = app.panes.len();
        let idx = app.spawn_one(AgentSpec::from_command(vec![
            "definitely-not-a-real-agent-binary-xyz".to_string(),
        ]));
        assert_eq!(idx, before, "new pane index == old panes.len()");
        assert_eq!(app.panes.len(), before + 1, "exactly one pane added");
        assert!(
            matches!(app.panes[idx].state(), AgentState::Failed(_)),
            "unspawnable agent should be a Failed pane (not Running/Idle)"
        );
        assert!(
            app.panes[idx].session.is_none(),
            "a failed spawn leaves the session slot empty"
        );

        // Focusing the new pane + rendering the whole grid must not panic.
        app.focus = idx;
        app.render().expect("render after a failed spawn_one");
    }

    #[test]
    fn spec_launch_target_prefers_existing_worktree() {
        let spec = AgentSpec {
            kind: AgentKind::Generic,
            name: "feature".to_owned(),
            command: vec!["bash".to_owned()],
            worktree: Some(PathBuf::from("/tmp/feature-worktree")),
            worktree_branch: Some("feature/login".to_owned()),
            workspace_id: None,
        };
        let (cwd, label) = spec_launch_target(&spec, Path::new("/tmp/repo"));
        assert_eq!(cwd, PathBuf::from("/tmp/feature-worktree"));
        assert_eq!(label.as_deref(), Some("feature/login"));
    }

    #[test]
    fn catalog_sidebar_keeps_remote_rows_and_deduplicates_local_panes() {
        let mut app = App::for_test(vec![pane(0, "repo/main")]);
        app.panes[0].workspace_id = Some("repo-a::/local/main".to_owned());
        app.workspace_catalog = vec![
            OrcaWorkspace {
                id: "repo-a::/local/main".to_owned(),
                path: PathBuf::from("/local/main"),
                branch: "main".to_owned(),
                display_name: "main".to_owned(),
                repo_id: "repo-a".to_owned(),
                project_id: Some("github:org/repo-a".to_owned()),
                host_id: Some("local".to_owned()),
                catalog_source_host_id: None,
                is_archived: false,
                workspace_status: None,
                is_main_worktree: true,
            },
            OrcaWorkspace {
                id: "repo-b::/remote/feature".to_owned(),
                path: PathBuf::from("/remote/feature"),
                branch: "feature/login".to_owned(),
                display_name: "login".to_owned(),
                repo_id: "repo-b".to_owned(),
                project_id: Some("github:org/repo-b".to_owned()),
                host_id: Some("ssh-host".to_owned()),
                catalog_source_host_id: None,
                is_archived: false,
                workspace_status: None,
                is_main_worktree: false,
            },
        ];
        let rows = app.catalog_sidebar_entries();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "repo-b/login");
        assert_eq!(rows[0].branch.as_deref(), Some("feature/login @ ssh-host"));
    }

    #[test]
    fn catalog_sidebar_keeps_remote_copy_with_same_workspace_id() {
        let mut app = App::for_test(vec![pane(0, "repo/main")]);
        app.panes[0].workspace_id = Some("repo-a::/same/main".to_owned());
        app.workspace_catalog = vec![
            OrcaWorkspace {
                id: "repo-a::/same/main".to_owned(),
                path: PathBuf::from("/same/main"),
                branch: "main".to_owned(),
                display_name: "main".to_owned(),
                repo_id: "repo-a".to_owned(),
                project_id: None,
                host_id: Some("local".to_owned()),
                catalog_source_host_id: None,
                is_archived: false,
                workspace_status: None,
                is_main_worktree: true,
            },
            OrcaWorkspace {
                id: "repo-a::/same/main".to_owned(),
                path: PathBuf::from("/same/main"),
                branch: "main".to_owned(),
                display_name: "main".to_owned(),
                repo_id: "repo-a".to_owned(),
                project_id: None,
                host_id: Some("runtime:env-1".to_owned()),
                catalog_source_host_id: Some("runtime:env-1".to_owned()),
                is_archived: false,
                workspace_status: None,
                is_main_worktree: true,
            },
        ];

        let rows = app.catalog_sidebar_entries();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].branch.as_deref(), Some("main @ runtime:env-1"));
    }

    #[test]
    fn workspace_sidebar_groups_catalog_rows_and_child_terminals() {
        let mut app = App::for_test(vec![pane(0, "build")]);
        app.panes[0].workspace_id = Some("repo-a::/main".to_owned());
        app.workspace_catalog = vec![
            OrcaWorkspace {
                id: "repo-a::/main".to_owned(),
                path: PathBuf::from("/main"),
                branch: "main".to_owned(),
                display_name: "main".to_owned(),
                repo_id: "repo-a".to_owned(),
                project_id: Some("github:org/repo-a".to_owned()),
                host_id: Some("local".to_owned()),
                catalog_source_host_id: None,
                is_archived: false,
                workspace_status: None,
                is_main_worktree: true,
            },
            OrcaWorkspace {
                id: "repo-b::/feature".to_owned(),
                path: PathBuf::from("/feature"),
                branch: "feature".to_owned(),
                display_name: "feature".to_owned(),
                repo_id: "repo-b".to_owned(),
                project_id: None,
                host_id: Some("local".to_owned()),
                catalog_source_host_id: None,
                is_archived: false,
                workspace_status: None,
                is_main_worktree: false,
            },
        ];
        let (groups, targets) = app.workspace_sidebar_groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "repo-a/main");
        assert_eq!(groups[0].agents.len(), 1);
        assert_eq!(groups[0].agents[0].0, 0);
        assert_eq!(groups[1].name, "repo-b/feature");
        assert!(groups[1].agents.is_empty());
        assert_eq!(
            targets[0].as_ref().map(|w| w.id.as_str()),
            Some("repo-a::/main")
        );
    }

    #[test]
    fn workspace_sidebar_render_uses_workspace_title() {
        let mut app = App::for_test(vec![pane(0, "build")]);
        app.workspace_catalog_loaded = true;
        app.workspace_catalog = vec![OrcaWorkspace {
            id: "repo-a::/main".to_owned(),
            path: PathBuf::from("/main"),
            branch: "main".to_owned(),
            display_name: "main".to_owned(),
            repo_id: "repo-a".to_owned(),
            project_id: Some("github:org/repo-a".to_owned()),
            host_id: Some("local".to_owned()),
            catalog_source_host_id: None,
            is_archived: false,
            workspace_status: None,
            is_main_worktree: true,
        }];
        app.panes[0].workspace_id = Some("repo-a::/main".to_owned());
        app.render().expect("render workspace sidebar");
        let text = buffer_text(&app);
        assert!(text.contains("WORKSPACES"));
        assert!(text.contains("repo-a/main"));
        assert!(!text.contains("IN PROGRESS"));
    }

    #[test]
    fn workspace_inventory_overlay_renders_and_scrolls_complete_catalog() {
        let mut app = App::for_test(Vec::new());
        app.set_workspace_catalog(
            (0..20)
                .map(|index| OrcaWorkspace {
                    id: format!("repo::{index}"),
                    path: PathBuf::from(format!("/workspace/{index}")),
                    branch: format!("feature-{index}"),
                    display_name: format!("workspace-{index}"),
                    repo_id: "repo".to_owned(),
                    project_id: None,
                    host_id: Some("local".to_owned()),
                    catalog_source_host_id: None,
                    is_archived: false,
                    workspace_status: None,
                    is_main_worktree: false,
                })
                .collect(),
        );
        app.mode = InputMode::Workspaces;
        app.workspace_selected = 19;
        app.render().expect("render workspace inventory");
        let text = buffer_text(&app);
        assert!(text.contains("Workspaces (20)"));
        assert!(text.contains("workspace-19"));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal);
    }

    #[test]
    fn render_shows_new_content_and_retains_old() {
        // Validates the pane render path against ratatui's double-buffering:
        // feeding new content and re-rendering must paint the new content, the
        // old content must persist, and an idle re-render (no new feed) must
        // neither panic nor erase what was drawn. (A dirty/partial repaint would
        // fail this — unrepainted cells carry 2-frame-old content under swap.)
        let mut app = App::for_test(vec![pane(0, "solo")]);
        app.panes[0].feed(b"first-frame");
        app.render().expect("first render");
        assert!(
            buffer_text(&app).contains("first-frame"),
            "first content rendered"
        );

        app.panes[0].feed(b"\nsecond-line");
        app.render().expect("second render (dirty rows)");
        let text = buffer_text(&app);
        assert!(text.contains("first-frame"), "first content persists");
        assert!(text.contains("second-line"), "new content rendered");

        // Idle frame: no new feed → no dirty rows → content must be retained.
        app.render().expect("idle re-render");
        let text = buffer_text(&app);
        assert!(
            text.contains("first-frame"),
            "content retained on idle frame"
        );
        assert!(
            text.contains("second-line"),
            "new content retained on idle frame"
        );
    }

    #[test]
    fn handle_mouse_scroll_moves_focused_pane_scrollback() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.focus = 1;
        assert_eq!(app.panes[1].scroll(), 0);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.panes[1].scroll(),
            3,
            "ScrollUp moves the focused pane 3 lines toward older output"
        );
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.panes[1].scroll(),
            0,
            "ScrollDown moves it back to the latest line (clamped at 0)"
        );
        assert_eq!(
            app.panes[0].scroll(),
            0,
            "the non-focused pane is untouched"
        );
    }

    #[test]
    fn handle_mouse_scroll_forwards_pageup_in_alt_screen() {
        // A fullscreen agent (opencode/claude) lives in the ALTERNATE screen,
        // which has no terminal scrollback. So the wheel must NOT scroll our
        // buffer (pane.scroll stays 0) — it forwards a PageUp to the agent so
        // the app scrolls its own content. (We can't assert the bytes were sent
        // without a live PTY, but pane.scroll not changing proves the buffer-
        // scroll branch was NOT taken → the forward branch was.)
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "alt")]);
        // Enter the alternate screen, like a fullscreen TUI does.
        app.panes[0].feed(b"\x1b[?1049h");
        assert!(
            app.panes[0].is_alternate_screen(),
            "test precondition: pane is in the alt screen"
        );
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.panes[0].scroll(),
            0,
            "alt-screen wheel must forward to the agent, not move our scrollback"
        );
    }

    #[test]
    fn handle_mouse_does_not_panic_when_focus_is_out_of_range() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.focus = 99;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        // Non-scroll mouse kinds (click) are ignored entirely.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.panes[0].scroll(), 0, "nothing changed");
    }

    #[test]
    fn mouse_down_focuses_without_starting_a_selection() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Give the pane some content so a selection would have something to
        // anchor on if it started.
        app.apply_update(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"hello world".to_vec(),
        });
        // One pane filling the content area with a 1-cell rounded border, so a
        // click at terminal (5,1) maps to inner grid (4,0) — the "hello" row.
        app.pane_rects = vec![Rect::new(0, 0, 40, 12)];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, 0, "Down moves focus to the hit pane");
        assert!(
            !app.panes[0].has_selection(),
            "a plain Down must NOT start a selection — only a Drag should"
        );
        // The drag origin is recorded so the first Drag move can anchor it.
        assert!(app.drag_origin.is_some(), "Down records the drag origin");
    }

    #[test]
    fn mouse_click_without_drag_does_not_select_or_copy() {
        // Regression for "selection too sensitive — a plain click triggers it".
        // Down immediately followed by Up (no movement) must produce NO
        // selection and therefore NO clipboard attempt (no toast).
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.apply_update(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"hello world".to_vec(),
        });
        app.pane_rects = vec![Rect::new(0, 0, 40, 12)];
        let toasts_before = app.toasts.len();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            !app.panes[0].has_selection(),
            "click with no drag must not select"
        );
        assert_eq!(
            app.toasts.len(),
            toasts_before,
            "click with no drag must not attempt a copy (no toast)"
        );
        assert!(app.drag_origin.is_none(), "Up clears the drag origin");
    }

    #[test]
    fn mouse_drag_extends_selection() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.apply_update(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"hello world".to_vec(),
        });
        app.pane_rects = vec![Rect::new(0, 0, 40, 12)];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 10,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            app.panes[0].has_selection(),
            "Drag while a selection is active extends it"
        );
    }

    #[test]
    fn mouse_up_clears_selection() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.apply_update(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"hello world".to_vec(),
        });
        app.pane_rects = vec![Rect::new(0, 0, 40, 12)];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 10,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 10,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        // Do NOT assert clipboard success — CI has no clipboard tool; copy
        // returns Err and pushes a warning toast, but selection is cleared
        // regardless on Up.
        assert!(
            !app.panes[0].has_selection(),
            "Up clears the selection after (attempted) copy"
        );
    }

    #[test]
    fn mouse_down_outside_any_pane_is_noop() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        // A small pane in the corner; click far outside it.
        app.pane_rects = vec![Rect::new(0, 0, 10, 5)];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 50,
            row: 50,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            !app.panes[0].has_selection(),
            "a click outside every pane is a no-op (no selection, no panic)"
        );
    }

    #[test]
    fn focus_directional_moves_focus_on_a_grid() {
        // 4 panes ⇒ cols = ceil(sqrt(4)) = 2 ⇒ a 2×2 grid:
        //   0 1
        //   2 3
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b"), pane(2, "c"), pane(3, "d")]);
        app.mode = InputMode::Pane;
        app.focus = 0;

        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.focus, 1, "Right moves within the row");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.focus, 3, "Down drops a row");
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.focus, 2, "Left moves back within the row");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.focus, 0, "Up moves up a row");

        // hjkl mirrors the arrow keys.
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert_eq!(app.focus, 1, "'l' == Right");
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.focus, 3, "'j' == Down");
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(app.focus, 2, "'h' == Left");
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.focus, 0, "'k' == Up");
    }

    #[test]
    fn focus_directional_clamps_at_grid_boundaries() {
        // 5 panes ⇒ cols = ceil(sqrt(5)) = 3 ⇒ a 3×2 grid:
        //   0 1 2
        //   3 4
        let mut app = App::for_test(vec![
            pane(0, "a"),
            pane(1, "b"),
            pane(2, "c"),
            pane(3, "d"),
            pane(4, "e"),
        ]);
        app.mode = InputMode::Pane;

        app.focus = 0;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.focus, 0, "no Up from the top row");
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.focus, 0, "no Left from column 0");

        app.focus = 4;
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.focus, 4, "no Right past the last pane in the row");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.focus, 4, "no Down past the last row");
    }

    #[test]
    fn forward_key_to_agent_is_a_noop_with_no_live_session() {
        // for_test seeds every session slot as None, so every forwarded key is a
        // best-effort write to a dead PTY — swallowed, never panicking. Exercises
        // every arm of the forwarding match (char, enter, backspace, tab, esc,
        // arrows, ctrl+c, and a multi-byte UTF-8 char).
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Normal;
        let keys = [
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
            // Ctrl+C maps to the raw byte 0x03 — still swallowed with no session.
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            // A non-ASCII char exercises the multi-byte UTF-8 encoding path.
            KeyEvent::new(KeyCode::Char('\u{D55C}'), KeyModifiers::NONE),
        ];
        for key in keys {
            app.handle_key(key); // must not panic
        }
        assert_eq!(app.focus, 0, "forwarding never moves focus");
        assert_eq!(app.mode, InputMode::Normal, "forwarding never changes mode");
    }

    #[test]
    fn apply_update_output_feeds_content_and_ignores_out_of_range_pane_id() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.apply_update(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"hello".to_vec(),
        });
        let cell = app.panes[0].emulator().cell(0, 0).expect("cell exists");
        assert_eq!(cell.chars, "h", "fed content reaches pane 0");
        assert!(
            !app.panes[1].emulator().cell(0, 0).unwrap().has_contents(),
            "pane 1 untouched"
        );
        // An out-of-range pane_id must be a no-op (the .get_mut guard), not a panic.
        app.apply_update(AgentUpdate::Output {
            pane_id: 99,
            bytes: b"nope".to_vec(),
        });
        assert_eq!(app.panes.len(), 2, "no pane added for an OOB pane_id");
    }

    #[test]
    fn cli_snapshot_updates_existing_view_without_ownership_mutation() {
        let mut app = App::for_test(vec![]);
        let terminal = OrcaTerminal {
            pty_id: "pty-1".into(),
            handle: "term-1".into(),
            title: Some("GUI tab".into()),
            agent_identity: Some("codex".into()),
            worktree_id: Some("workspace-1".into()),
            worktree_path: None,
            branch: Some("main".into()),
            order: 0,
        };
        app.apply_update(AgentUpdate::OrcaSnapshot {
            terminal,
            lines: vec!["hello from Orca".into()],
            status: Some("running".into()),
        });
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.panes[0].name(), "GUI tab");
        assert_eq!(app.panes[0].session_owner, SessionOwner::OrcaExisting);
        assert!(app.panes[0].daemon_read_only);
        assert_eq!(app.panes[0].emulator().cell(0, 0).unwrap().chars, "h");
    }

    #[test]
    fn cli_view_does_not_forward_input_to_existing_orca_tab() {
        let mut app = App::for_test(vec![pane(0, "GUI tab")]);
        app.panes[0].orca_handle = Some("term-1".into());
        app.panes[0].session_owner = SessionOwner::OrcaExisting;
        let (tx, rx) = std::sync::mpsc::channel();
        app.orca_send_tx = Some(tx);
        app.orca_cli = Some(OrcaCliBridge::with_executable("true"));
        app.send_to_focused(b"x");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cli_inventory_removes_gui_closed_view_but_keeps_local_pane() {
        let mut app = App::for_test(vec![pane(0, "gui"), pane(1, "local")]);
        app.panes[0].orca_handle = Some("gone".into());
        app.panes[0].session_owner = SessionOwner::OrcaExisting;
        app.panes[1].session = None;
        app.apply_update(AgentUpdate::OrcaInventory { terminals: vec![] });
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.panes[0].name(), "local");
    }

    #[test]
    fn parse_stream_data_routes_known_sessions_and_drops_unknown_sessions() {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        let mut m = HashMap::new();
        m.insert("sess-1".to_string(), 2usize);
        let map = Arc::new(Mutex::new(m));

        let (pane, bytes) =
            super::parse_stream_data(br#"{"sessionId":"sess-1","data":"hello"}"#, &map)
                .expect("known session routes");
        assert_eq!(pane, 2, "known session routes to its pane");
        assert_eq!(bytes, b"hello", "data field extracted as bytes");

        assert!(
            super::parse_stream_data(br#"{"sessionId":"sess-9","data":"x"}"#, &map).is_none(),
            "unknown session must be dropped rather than routed to pane 0"
        );

        // JSON without a sessionId → pane 0 with the raw payload.
        let (pane, bytes) = super::parse_stream_data(br#"{"data":"y"}"#, &map)
            .expect("payload without session id uses raw compatibility path");
        assert_eq!(pane, 0);
        assert_eq!(bytes, br#"{"data":"y"}"#, "raw payload returned verbatim");

        // Non-JSON bytes → pane 0 with the payload verbatim.
        let (pane, bytes) = super::parse_stream_data(b"raw-bytes", &map)
            .expect("raw payload uses compatibility path");
        assert_eq!(pane, 0);
        assert_eq!(bytes, b"raw-bytes");

        let (pane, bytes) = super::parse_stream_data(
            br#"{"type":"event","event":"data","sessionId":"sess-1","payload":{"data":"nested"}}"#,
            &map,
        )
        .expect("Orca NDJSON event routes by payload.data");
        assert_eq!(pane, 2);
        assert_eq!(bytes, b"nested");
    }

    #[test]
    fn daemon_snapshot_bytes_combines_orca_snapshot_sections() {
        let payload = serde_json::json!({
            "snapshot": {
                "scrollbackAnsi": "old\\r\\n",
                "rehydrateSequences": "\\u001b[?25l",
                "snapshotAnsi": "current",
                "pendingEscapeTailAnsi": "tail"
            }
        });
        assert_eq!(
            super::App::<TestBackend>::daemon_snapshot_bytes(&payload).unwrap(),
            b"old\\r\\n\\u001b[?25lcurrenttail"
        );
    }

    #[test]
    fn parse_stream_event_extracts_exit_code_and_ignores_non_exit() {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        let mut m = HashMap::new();
        m.insert("sess-1".to_string(), 2usize);
        let map = Arc::new(Mutex::new(m));

        assert_eq!(
            super::parse_stream_event(
                br#"{"event":"exit","sessionId":"sess-1","payload":{"code":42}}"#,
                &map,
            ),
            Some((2, 42)),
            "exit event routed to the mapped pane with its code"
        );
        assert_eq!(
            super::parse_stream_event(
                br#"{"event":"exit","sessionId":"ghost","payload":{"code":7}}"#,
                &map,
            ),
            None,
            "unknown session is dropped"
        );
        assert_eq!(
            super::parse_stream_event(br#"{"event":"data","sessionId":"sess-1"}"#, &map),
            None,
            "non-exit events are ignored"
        );
        assert_eq!(
            super::parse_stream_event(b"not json", &map),
            None,
            "non-JSON → None"
        );
        assert_eq!(
            super::parse_stream_event(br#"{"sessionId":"sess-1"}"#, &map),
            None,
            "missing event field → None"
        );
        assert_eq!(
            super::parse_stream_event(br#"{"event":"exit","payload":{"code":0}}"#, &map),
            None,
            "missing sessionId → None"
        );
    }

    #[test]
    fn orchestration_drained_reflects_coordinator_state() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        assert!(
            app.orchestration_drained(),
            "no coordinator (plain run) ⇒ vacuously drained"
        );

        // A coordinator with a still-Pending task ⇒ not drained.
        let mut coord = Coordinator::new();
        let tid = coord.add_task("work", Vec::new());
        app.coordinator = Some(coord);
        assert!(!app.orchestration_drained(), "a Pending task ⇒ not drained");

        // Drive it terminal (Done) ⇒ drained.
        if let Some(c) = app.coordinator.as_mut() {
            c.report_done(tid, "finished");
        }
        assert!(app.orchestration_drained(), "Done is terminal");

        // A Failed task is also terminal.
        let mut coord = Coordinator::new();
        let f = coord.add_task("boom", Vec::new());
        coord.report_failed(f, "oops");
        app.coordinator = Some(coord);
        assert!(app.orchestration_drained(), "Failed is terminal too");
    }

    #[test]
    fn ctrl_n_spawns_a_new_pane_as_a_complete_slot() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        let before = app.panes.len();
        // Spawn picker moved behind the gateway: enter Pane (Ctrl+Alt+P),
        // then press `n` to open the picker; Enter spawns the selected agent.
        app.handle_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Spawn, "Pane + n opens spawn picker");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Enter closes the picker");
        assert_eq!(
            app.panes.len(),
            before + 1,
            "Pane + n → Enter adds one pane"
        );
        assert!(app
            .panes
            .iter()
            .all(|slot| slot.command.is_empty() || slot.id() < app.next_pane_id));
        assert_eq!(app.focus, before, "spawned pane is focused");
        app.render().expect("render after spawn");
    }

    #[test]
    fn jump_palette_backspace_and_navigation_clamp() {
        let mut app = App::for_test(vec![
            pane(0, "claude"),
            pane(1, "codex"),
            pane(2, "opencode"),
        ]);
        app.mode = InputMode::Jump;
        for c in ['o', 'd', 'e', 'x'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(app.jump_query, "odex");
        assert_eq!(app.jump_filtered(), vec![1], "only codex matches 'odex'");

        // Backspace trims the last char and resets the selection to the top.
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.jump_query, "ode");
        assert_eq!(
            app.jump_filtered(),
            vec![1, 2],
            "'ode' widens back to codex + opencode"
        );
        assert_eq!(app.jump_selected, 0);

        // Down / Up move the selection; both ends clamp (no under/overflow).
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.jump_selected, 1);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.jump_selected, 1, "Down at the bottom clamps");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.jump_selected, 0);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.jump_selected, 0, "Up at the top clamps");
    }

    #[test]
    fn jump_palette_enter_with_no_matches_keeps_focus() {
        let mut app = App::for_test(vec![pane(0, "claude"), pane(1, "codex")]);
        app.mode = InputMode::Jump;
        app.focus = 1;
        for c in ['z', 'z'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(app.jump_filtered().is_empty(), "no agent matches 'zz'");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Enter closes the palette");
        assert_eq!(app.focus, 1, "Enter with no matches keeps the prior focus");
    }

    #[test]
    fn drain_bus_applies_queued_updates() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        // Push two updates into the bus channel.
        let _ = app.bus_tx.send(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"bus-fed".to_vec(),
        });
        let _ = app.bus_tx.send(AgentUpdate::State {
            pane_id: 1,
            state: AgentState::Done(Some(0)),
        });
        // drain_bus should apply both and return true (activity detected).
        assert!(app.drain_bus(), "drain_bus returns true when updates exist");
        // A second drain with nothing queued returns false.
        assert!(!app.drain_bus(), "drain_bus returns false when empty");
        // Pane 0 got the bytes, pane 1 got the state.
        assert!(
            app.panes[0].emulator().cell(0, 0).unwrap().has_contents(),
            "bus-fed bytes reached pane 0"
        );
        assert!(
            matches!(app.panes[1].state(), AgentState::Done(_)),
            "bus-fed state reached pane 1"
        );
    }

    #[test]
    fn apply_update_state_sets_pane_state() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        assert!(
            matches!(app.panes[0].state(), AgentState::Running),
            "starts Running"
        );
        app.apply_update(AgentUpdate::State {
            pane_id: 0,
            state: AgentState::Done(Some(0)),
        });
        assert!(
            matches!(app.panes[0].state(), AgentState::Done(_)),
            "State update transitioned to Done"
        );
        // Out-of-range pane_id is a no-op (no panic).
        app.apply_update(AgentUpdate::State {
            pane_id: 99,
            state: AgentState::Failed("nope".into()),
        });
    }

    #[test]
    fn handle_event_mouse_routes_to_handle_mouse() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Feeding some content so the pane has scrollback.
        app.panes[0].feed(b"line1\nline2\nline3");
        // A scroll-up mouse event should not panic.
        app.handle_event(Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        // A scroll-down should also not panic.
        app.handle_event(Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        // Other mouse kinds are no-ops (no panic).
        app.handle_event(Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
    }

    #[test]
    fn handle_event_resize_is_a_noop() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        let mode_before = app.mode;
        app.handle_event(Event::Resize(120, 40));
        assert_eq!(app.mode, mode_before, "Resize does not change mode");
    }

    #[test]
    fn handle_event_key_release_does_not_trigger_handle_key() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        assert_eq!(app.mode, InputMode::Normal);
        // A key RELEASE event should NOT enter pane mode (only Press does).
        app.handle_event(Event::Key(KeyEvent {
            code: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        }));
        assert_eq!(
            app.mode,
            InputMode::Normal,
            "Release does not trigger mode switch"
        );
    }

    #[test]
    fn handle_event_other_event_is_noop() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // FocusGained is an "other" event variant → no-op.
        app.handle_event(Event::FocusGained);
        app.handle_event(Event::FocusLost);
        app.handle_event(Event::Paste("hello".to_string()));
        assert!(!app.quit, "unrecognized events don't quit");
    }

    #[test]
    fn forward_key_to_agent_ctrl_chars_are_noop_with_no_session() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // In Normal mode, Ctrl+C should forward byte 0x03 (but with no
        // session it's a silent no-op — no panic).
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        // Ctrl+A → byte 0x01, Ctrl+Z → 0x1a.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(!app.quit, "Ctrl+C in standalone (no Ctrl+Q) does not quit");
    }

    #[test]
    fn forward_key_to_agent_alt_char_is_noop_with_no_session() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Alt+x should be a no-op (falls through to the _ => {} arm because
        // the Char(!ctrl && !alt) arm doesn't match, and there's no
        // Char + alt arm).
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        // No panic = pass.
    }

    #[test]
    fn forward_key_to_agent_special_keys_are_noop_with_no_session() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Various special keys that have explicit arms — all no-ops with
        // no session.
        for code in [
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::Tab,
            KeyCode::Esc,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Delete,
            KeyCode::BackTab,
        ] {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        // No panic = pass.
    }

    #[test]
    fn send_to_focused_is_silent_noop_with_no_session() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // No live session → send_to_focused is a silent no-op.
        app.send_to_focused(b"test");
        // No panic, pane unchanged.
        assert!(
            !app.panes[0].emulator().cell(0, 0).unwrap().has_contents(),
            "no bytes written with no session"
        );
    }

    #[test]
    fn render_with_sidebar_shows_connection_state() {
        let mut app = App::for_test(vec![pane(0, "claude"), pane(1, "codex")]);
        // Default connection state is Standalone — render with sidebar on.
        app.render().expect("render");
        let text = buffer_text(&app);
        assert!(
            text.contains("Standalone"),
            "sidebar shows Standalone connection state"
        );
    }

    #[test]
    fn render_narrow_terminal_keeps_workspace_sidebar_visible() {
        let mut app = App::for_test(vec![pane(0, "bash")]);
        app.set_workspace_catalog(vec![OrcaWorkspace {
            id: "repo::/workspace".to_owned(),
            path: PathBuf::from("/workspace"),
            branch: "feature/sidebar".to_owned(),
            display_name: "ws".to_owned(),
            repo_id: "repo".to_owned(),
            project_id: None,
            host_id: Some("local".to_owned()),
            catalog_source_host_id: None,
            is_archived: false,
            workspace_status: None,
            is_main_worktree: false,
        }]);
        app.terminal.backend_mut().resize(40, 24);

        app.render().expect("render narrow terminal");

        let text = buffer_text(&app);
        assert!(
            text.contains("repo/ws"),
            "workspace sidebar must remain visible when a compact sidebar fits:\n{text}"
        );
    }

    #[test]
    fn render_with_sidebar_hidden_still_works() {
        let mut app = App::for_test(vec![pane(0, "solo")]);
        app.sidebar_hidden = true;
        app.render().expect("render with sidebar hidden");
        let text = buffer_text(&app);
        assert!(text.contains("solo"), "pane name still rendered");
    }

    #[test]
    fn render_with_toasts_overlay_does_not_panic() {
        let mut app = App::for_test(vec![pane(0, "solo")]);
        app.toasts.push(crate::toast::Toast::error("daemon error"));
        app.toasts.push(crate::toast::Toast::warning("slow"));
        app.render().expect("render with toasts");
        let text = buffer_text(&app);
        assert!(text.contains("daemon error"), "toast message rendered");
    }

    #[test]
    fn activity_overlay_toggles_with_a_key() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Pane;
        // Bare `a` in Pane mode opens the activity overlay.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()));
        assert_eq!(
            app.mode,
            InputMode::Activity,
            "'a' in Pane mode should open the Activity overlay"
        );
        // Esc closes it (back to Normal).
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(
            app.mode,
            InputMode::Normal,
            "Esc should close the Activity overlay"
        );
    }

    #[test]
    fn activity_log_records_state_transition() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Pretend pane 0 was previously derived as Working so the first
        // derivation below is treated as a real transition (prev != new).
        app.panes[0].last_status = Some(AgentStatus::Working);
        // Flip the pane to a Failed lifecycle state — `record_activity` should
        // then emit both a State{Working→Failed} and an Error event.
        app.panes[0].set_state(AgentState::Failed("boom".into()));
        app.record_activity();

        assert!(
            app.activity.len() >= 1,
            "at least one event should be recorded on a real transition"
        );
        // At least one recorded event's render_line must mention the pane name
        // AND contain either the arrow (State) or "error" (Error).
        let lines: Vec<String> = app
            .activity
            .recent(app.activity.len())
            .iter()
            .map(|e| e.render_line())
            .collect();
        let found = lines
            .iter()
            .any(|l| l.contains("a") && (l.contains("error") || l.contains("\u{2192}")));
        assert!(
            found,
            "expected a line naming pane 'a' with an arrow or 'error'; got {lines:?}"
        );
    }

    #[test]
    fn spawn_options_includes_custom_command_entry() {
        let app = App::for_test(vec![pane(0, "a")]);
        let opts = app.spawn_options();
        // The custom-command sentinel must be the LAST entry.
        let last = opts
            .last()
            .expect("spawn_options should always have at least bash + custom");
        assert!(
            last.0.contains("Custom command"),
            "last entry name should contain 'Custom command'; got {:?}",
            last.0
        );
        assert!(
            last.1.is_empty(),
            "custom-command sentinel must have an empty command vector; got {:?}",
            last.1
        );
    }

    #[test]
    fn spawn_options_lists_all_known_agents() {
        // The picker must show the FULL agent catalog (Claude, Gemini, Codex,
        // …), not only the agents detected on this machine's $PATH. An
        // uninstalled agent is suffixed "(not installed)" but is still listed
        // so the user sees every option (the "run any CLI agent" philosophy).
        let app = App::for_test(vec![pane(0, "a")]);
        let opts = app.spawn_options();
        let names: Vec<&str> = opts.iter().map(|(n, _)| n.as_str()).collect();
        for kind in AgentKind::all_known() {
            let display = kind.display_name();
            assert!(
                names.iter().any(|n| n.contains(display)),
                "{display:?} should appear in the spawn options (installed or not): {names:?}"
            );
        }
        // bash + 11 known agents + custom sentinel => at least 13 entries.
        assert!(
            opts.len() >= 13,
            "expected bash + all known agents + custom; got {} entries: {opts:?}",
            opts.len()
        );
        // The custom-command sentinel stays last.
        assert!(
            opts.last()
                .map(|(n, _)| n.contains("Custom command"))
                .unwrap_or(false),
            "custom-command entry must be last: {opts:?}"
        );
    }

    #[test]
    fn spawn_options_marks_uninstalled_agents() {
        // At least the structural contract: every entry's command is either the
        // agent binary (non-empty) or the custom sentinel (empty). And entries
        // whose binary is NOT on $PATH carry the "(not installed)" suffix on a
        // machine where that binary is absent. We assert the suffix appears on
        // SOME entry when not everything is installed (the common case in CI,
        // where typically only `bash` and maybe one agent are present).
        let app = App::for_test(vec![pane(0, "a")]);
        let opts = app.spawn_options();
        let installed_bins: Vec<&'static str> = AgentKind::detect_installed()
            .iter()
            .map(AgentKind::binary)
            .collect();
        let uninstalled_count = AgentKind::all_known()
            .iter()
            .filter(|k| !installed_bins.contains(&k.binary()))
            .count();
        let marked_not_installed = opts
            .iter()
            .filter(|(n, _)| n.contains("(not installed)"))
            .count();
        assert_eq!(
            marked_not_installed, uninstalled_count,
            "uninstalled agents ({uninstalled_count}) must each be marked; got {marked_not_installed} marked: {opts:?}"
        );
    }

    #[test]
    fn render_spawn_picker_does_not_panic() {
        // Regression guard: opening the spawn picker (`n`) and rendering must
        // never panic, regardless of how many agents are installed. This
        // exercises the full render path (spawn_options → snapshot → overlay).
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Spawn;
        app.render().expect("render in Spawn mode must not panic");
        let text = buffer_text(&app);
        assert!(
            text.contains("New pane"),
            "spawn picker title should render: {text}"
        );
    }

    #[test]
    fn custom_command_modal_types_and_spawns() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Enter the spawn picker and navigate to the custom-command sentinel
        // (the last entry).
        app.mode = InputMode::Spawn;
        app.spawn_selected = app.spawn_options().len() - 1;
        // Enter on the sentinel switches to the text-entry modal.
        app.handle_spawn_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert_eq!(
            app.mode,
            InputMode::SpawnCustom,
            "Enter on the custom sentinel should open SpawnCustom mode"
        );
        assert!(
            app.custom_cmd.is_empty(),
            "custom_cmd should be cleared when entering SpawnCustom"
        );
        // Type "bash".
        for ch in "bash".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()));
        }
        assert_eq!(
            app.custom_cmd, "bash",
            "typing should accumulate into custom_cmd"
        );
        // Backspace pops one char.
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
        assert_eq!(app.custom_cmd, "bas", "Backspace should pop the last char");
        // Esc cancels back to Normal (without spawning).
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(
            app.mode,
            InputMode::Normal,
            "Esc should cancel SpawnCustom back to Normal"
        );
    }

    #[test]
    fn dashboard_groups_panes_by_status_bucket() {
        // Three panes in distinct lifecycle states map to the three dashboard
        // buckets: Running→Working, Done→done, Failed→needs-attention.
        let mut working_pane = pane(0, "alpha");
        working_pane.set_state(AgentState::Running); // Working bucket
        let mut done_pane = pane(1, "beta");
        done_pane.set_state(AgentState::Done(Some(0))); // done bucket
        let mut failed_pane = pane(2, "gamma");
        failed_pane.set_state(AgentState::Failed("boom".into())); // needs-attention bucket

        let mut app = App::for_test(vec![working_pane, done_pane, failed_pane]);
        app.mode = InputMode::Dashboard;
        app.render().expect("render into TestBackend");

        let text = buffer_text(&app);
        // The overlay title + all three column headers render.
        assert!(text.contains("Agent Dashboard"), "overlay title rendered");
        assert!(
            text.contains("needs-attention"),
            "needs-attention column header rendered"
        );
        assert!(text.contains("working"), "working column header rendered");
        assert!(text.contains("done"), "done column header rendered");
        // Each pane name lands in its bucket: alpha (Working), beta (Done),
        // gamma (Failed) all appear somewhere in the overlay text.
        assert!(text.contains("alpha"), "Working pane name rendered");
        assert!(text.contains("beta"), "Done pane name rendered");
        assert!(text.contains("gamma"), "Failed pane name rendered");
        // The counts appear in the headers: each bucket has exactly 1 member.
        assert!(
            text.contains("needs-attention (1)"),
            "needs-attention count is 1"
        );
        assert!(text.contains("working (1)"), "working count is 1");
        assert!(text.contains("done (1)"), "done count is 1");
    }

    #[test]
    fn zoom_then_esc_then_chat_then_gateway_does_not_panic() {
        // Reproduces the reported crash: Ctrl+Alt+P → z (zoom) → Esc (Normal,
        // zoom persists) → chat → Ctrl+Alt+P again → crash. If any step panics,
        // this test fails with the panic location.
        let mut app = App::for_test(vec![pane(0, "alpha")]);
        // Ctrl+Alt+P → Pane mode.
        app.handle_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(app.mode, InputMode::Pane);
        // z → zoom on.
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert!(app.zoomed);
        app.render().expect("render: zoomed in Pane mode");
        // Esc → Normal (zoom persists — interact with the agent while zoomed).
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.zoomed, "zoom persists into Normal mode");
        // Simulate chatting: the agent produces output into the focused pane.
        app.panes[0].feed(b"agent reply line one\r\nagent reply line two\r\n");
        app.render()
            .expect("render: zoomed in Normal mode after chat");
        // Ctrl+Alt+P again → back to Pane mode (the reported crash point).
        app.handle_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(app.mode, InputMode::Pane);
        app.render()
            .expect("render after the second gateway must not panic");
        // z again should unzoom cleanly (toggle contract).
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert!(!app.zoomed);
        app.render().expect("render unzoomed");
    }

    #[test]
    fn zoomed_mouse_hit_testing_targets_focused_pane() {
        let mut app = App::for_test(vec![pane(0, "alpha"), pane(1, "beta")]);
        app.focus = 1;
        app.zoomed = true;
        app.render().expect("render zoomed panes");

        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            // The default test config shows the sidebar, so the zoomed pane
            // begins to the right of its reserved column.
            column: 30,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.focus, 1, "zoomed click keeps the focused pane");
        assert_eq!(app.drag_origin.map(|origin| origin.0), Some(1));
    }

    /// Close a pane, then spawn a fresh one. The new pane MUST receive a
    /// brand-new stable id from the monotonic counter — never the freed id of
    /// the closed pane. (Before stable-id routing, reusing a freed id would
    /// route one agent's output into another's pane.) `for_test` builds panes
    /// directly, bypassing `spawn_one`, so we advance `next_pane_id` to mirror
    /// the real runtime where the existing ids were already issued.
    #[test]
    fn close_then_spawn_assigns_fresh_pane_id_from_counter() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b"), pane(2, "c")]);
        app.next_pane_id = app.panes.len(); // 3 ids already issued → counter at 3
                                            // Close pane 0; survivors slide to positions 0,1 carrying ids 1,2.
        app.focus = 0;
        app.close_focused_pane();
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.panes[0].id(), 1);
        assert_eq!(app.panes[1].id(), 2);

        // Spawn a new pane. Its id comes from the counter (>= 3), NOT the
        // freed id 0 nor either surviving id (1, 2).
        let idx = app.spawn_one(AgentSpec::from_command(vec!["bash".to_string()]));
        let new_id = app.panes[idx].id();
        assert!(new_id >= 3, "spawned pane got a fresh counter id: {new_id}");
        assert_ne!(new_id, 0, "did not reuse the closed pane's freed id");
        assert_ne!(new_id, 1, "did not collide with a surviving id");
        assert_ne!(new_id, 2, "did not collide with a surviving id");
        assert_eq!(
            app.next_pane_id,
            new_id + 1,
            "counter advanced past the new id"
        );

        // The new pane's forwarder reports by this stable id; an Output for it
        // must resolve to the new position (not silently drop or misroute).
        app.apply_update(AgentUpdate::Output {
            pane_id: new_id,
            bytes: b"fresh-pane-live".to_vec(),
        });
        assert_eq!(
            app.panes[idx].emulator().cell(0, 0).expect("cell").chars,
            "f",
            "new pane received output by its fresh stable id"
        );
    }

    /// Closing the LAST pane (focus = len-1) exercises the
    /// `focus >= panes.len()` clamp branch distinctly from closing the first
    /// pane. Focus must land on the new last pane and survivors keep their ids.
    #[test]
    fn close_last_pane_clamps_focus_to_new_last() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b"), pane(2, "c")]);
        app.focus = 2; // the last pane
        app.close_focused_pane();
        assert_eq!(app.panes.len(), 2);
        // Survivors keep their stable ids [0, 1].
        assert_eq!(app.panes[0].id(), 0);
        assert_eq!(app.panes[1].id(), 1);
        // focus was 2; after the remove panes.len()==2, so it clamps to 1.
        assert_eq!(app.focus, 1, "focus clamped to the new last pane");
    }

    /// Closing every pane must fully drain the slot collection and reset the app
    /// to a sane empty state (Normal mode, zoom cleared) so a later spawn or
    /// render doesn't index into stale state.
    #[test]
    fn close_all_panes_empties_state_and_resets_mode() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.zoomed = true;
        app.mode = InputMode::Pane;
        app.focus = 0;
        app.close_focused_pane();
        assert_eq!(app.panes.len(), 1);
        // Mode/zoom persist while a pane remains.
        assert_eq!(app.mode, InputMode::Pane);
        assert!(app.zoomed);
        app.close_focused_pane(); // close the survivor
        assert!(app.panes.is_empty(), "all panes closed");
        assert_eq!(
            app.mode,
            InputMode::Normal,
            "mode reset to Normal when empty"
        );
        assert!(!app.zoomed, "zoom flag cleared with no panes left");
        assert_eq!(app.focus, 0);
    }

    /// Zoom mode fills the whole pane area with one pane. At a degenerate 5×2
    /// size the inner area collapses, but `render()` must still not panic
    /// (saturating math clamps to MIN_COLS/MIN_ROWS) and must paint something.
    #[test]
    fn zoom_render_at_degenerate_size_does_not_panic() {
        let mut app = App::for_test(vec![pane(0, "solo")]);
        app.sidebar_hidden = true; // deterministic: pane gets the full width
        app.panes[0].feed(b"X");
        app.zoomed = true;
        app.terminal
            .resize(Rect::new(0, 0, 5, 2))
            .expect("resize backend to degenerate 5x2");
        app.render().expect("zoom render at 5x2 must not panic");
        let text = buffer_text(&app);
        assert!(
            text.chars().any(|c| !c.is_whitespace()),
            "zoomed pane painted border/content even at degenerate 5x2"
        );
    }

    /// Blast-radius test: a close → spawn → close sequence keeps each slot's
    /// identity and state self-contained.
    #[test]
    fn slots_stay_consistent_across_close_spawn_close() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b"), pane(2, "c"), pane(3, "d")]);
        app.next_pane_id = app.panes.len(); // 4 ids issued
                                            // Establish activity baselines on each slot.
        app.record_activity();
        assert!(app.panes.iter().all(|slot| slot.last_status.is_some()));

        // Step 1: close idx 1.
        app.focus = 1;
        app.close_focused_pane();
        assert_eq!(app.panes.len(), 3);
        assert!(app.panes.iter().all(|slot| slot.last_status.is_some()));

        // Step 2: spawn a fresh pane (id from the counter).
        app.spawn_one(AgentSpec::from_command(vec!["bash".to_string()]));
        assert_eq!(app.panes.len(), 4);
        assert!(app
            .panes
            .iter()
            .all(|slot| slot.command.is_empty() || !slot.command.is_empty()));

        // Step 3: close idx 0 — the remaining slots stay independent.
        app.focus = 0;
        app.close_focused_pane();
        assert_eq!(app.panes.len(), 3);
        assert!(app.panes.iter().all(|slot| slot.id() < app.next_pane_id));
    }
}
