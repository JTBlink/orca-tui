//! # CLI
//!
//! Argument parsing and subcommand dispatch for the `orca-tui` binary. Kept in
//! the library (not `main.rs`) so the dispatch logic is unit-testable and the
//! binary stays a one-line entry point.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::agent::{AgentKind, AgentSpec};
use crate::app::App;
use crate::integrations::{self, RepoRef};
use crate::orca_workspaces::{self, OrcaWorkspace, OrcaWorkspaceCatalog};
use crate::ssh::SshTarget;
use crate::worktree::WorktreeManager;
use crate::{coordinator, mobile};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the CLI: parse argv and dispatch to the selected subcommand. Returns an
/// error to be surfaced by the binary's `main` (which maps it to a non-zero
/// exit code).
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    try_main(cli)
}

#[derive(Parser, Debug)]
#[command(
    name = "orca-tui",
    version = VERSION,
    about = "Terminal multi-agent coding orchestrator (TUI port of Orca GUI)"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run one or more agents in panes.
    ///
    /// The trailing command list is split into per-agent commands on the
    /// literal `::` separator, and each command gets its own pane, e.g.
    ///
    /// - `orca-tui run -- claude codex opencode` → three side-by-side panes.
    /// - `orca-tui run -- claude :: codex --model x :: opencode` → three panes;
    ///   the middle agent's command is `codex --model x`.
    ///
    /// **Splitting rule:** if any `::` is present the list is split into the
    /// segments between `::` tokens (empty segments from leading/trailing/
    /// doubled `::` are dropped); if no `::` is present each token is its own
    /// agent (backward compatible). A stray `::` therefore changes semantics.
    Run {
        /// Working directory shared by every agent. Defaults to the current
        /// directory.
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,

        /// Give each agent its own isolated git worktree. Requires `cwd` (or
        /// the current directory) to be inside a git repository; worktrees are
        /// created under `.orca-worktrees/` and removed when the app exits.
        #[arg(long)]
        worktree: bool,

        /// Start one pane for every Git worktree registered in the current
        /// repository. Each pane runs in that worktree's checkout.
        #[arg(long)]
        all_worktrees: bool,

        /// Try to connect to a running Orca GUI daemon for session
        /// persistence + multi-client (GUI + TUI). Falls back to standalone
        /// (direct PTY) if no daemon is found or the connection fails.
        #[arg(long)]
        daemon: bool,

        /// Run each agent on a REMOTE host over SSH (Feature 8). The host spec
        /// is `user@host`, `host`, or `user@host:port`; each agent command is
        /// wrapped as `ssh <opts> <host> <command...>`.
        #[arg(long, value_name = "HOST")]
        remote: Option<String>,

        /// With `--remote`: auto-reconnect a dropped remote session on the same
        /// pane after an exponential backoff, up to a few attempts (Feature 8).
        #[arg(long)]
        reconnect: bool,

        /// Start the mobile-companion WebSocket server (Feature 10) alongside
        /// the agents, broadcasting live pane status to a phone/PWA. The URL
        /// and one-time token are printed at startup.
        #[arg(long, value_name = "PORT")]
        mobile: Option<u16>,

        /// One or more agent invocations. Each command (separated by `::`)
        /// becomes its own pane. Without `::`, each token is its own agent.
        /// Everything after `--` is captured verbatim, including flags
        /// intended for the agent itself.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            num_args = 1..,
            value_name = "COMMAND",
        )]
        command: Vec<String>,
    },

    /// Plan multi-agent orchestration from a spec (Feature 7).
    ///
    /// Splits the spec into tasks (one per non-empty line) and dispatches them
    /// to agents dependency-gated: by default tasks run **sequentially** (each
    /// depends on the previous), so only one is in flight at a time; pass
    /// `--parallel` to fan every task out at once. Each task's description is
    /// passed to the agent as its prompt argument.
    Orchestrate {
        /// Newline-separated task spec. Use a quoted multi-line string. Ignored
        /// when `--issues` is given.
        #[arg(long)]
        spec: Option<String>,
        /// Fan every task out in parallel instead of running them sequentially.
        #[arg(long)]
        parallel: bool,
        /// Source tasks from a GitHub repo's open issues via `gh`
        /// (`owner/name`); each issue becomes one task (Feature 9). Overrides
        /// `--spec`.
        #[arg(long, value_name = "REPO")]
        issues: Option<String>,
    },

    /// List open pull requests for a GitHub repo via `gh` (Feature 9).
    Prs {
        /// `owner/name` GitHub repository.
        repo: String,
    },

    /// List open issues for a GitHub repo via `gh` (Feature 9).
    Issues {
        /// `owner/name` GitHub repository.
        repo: String,
    },

    /// Start the mobile-companion WebSocket server (Feature 10).
    ///
    /// Binds a local WebSocket server a phone/PWA can connect to. Prints the
    /// URL and a one-time pairing token. For live snapshots while agents run,
    /// prefer `run --mobile <PORT>`.
    Mobile {
        /// Port to bind. Defaults to 0 (OS-assigned).
        #[arg(long, default_value_t = 0)]
        port: u16,
    },

    /// Start the built-in daemon server (run as a systemd/supervisor service).
    ///
    /// Owns agent PTYs and serves `orca-tui attach` clients over a Unix socket.
    /// Agents survive client disconnect — the daemon keeps running until all
    /// agents exit and no clients remain, or until SIGTERM.
    ///
    /// Designed for `systemctl --user start orca-tui` or equivalent. Logs to
    /// stdout/stderr (captured by journald/supervisor).
    Daemon {
        /// Unix socket path. Defaults to `$XDG_RUNTIME_DIR/orcatui.sock`.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,

        /// Agent commands (same `::` separator as `run`). If omitted, the
        /// daemon starts empty and clients create sessions via the protocol.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },

    /// Attach to a running orca-tui daemon as a TUI client.
    ///
    /// Connects to the daemon's Unix socket, renders all live agent panes,
    /// and forwards keyboard input. Multiple clients can attach simultaneously.
    /// Detaching (Ctrl+Q) does NOT kill the agents — they keep running in the
    /// daemon.
    Attach {
        /// Unix socket path. Defaults to `$XDG_RUNTIME_DIR/orcatui.sock`.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
    },
}

fn try_main(cli: Cli) -> Result<()> {
    dispatch_command(cli.command.unwrap_or_else(default_command))
}

/// Resolve the no-subcommand convenience behavior without mixing it into
/// subcommand dispatch.
fn default_command() -> Command {
    let socket = crate::daemon_server::default_socket_path();
    if socket.exists() {
        Command::Attach { socket: None }
    } else {
        let agent = crate::agent::AgentKind::detect_installed()
            .first()
            .map(crate::agent::AgentKind::binary)
            .unwrap_or("bash")
            .to_string();
        Command::Run {
            cwd: None,
            worktree: false,
            all_worktrees: true,
            daemon: false,
            remote: None,
            reconnect: false,
            mobile: None,
            command: vec![agent],
        }
    }
}

/// Top-level dispatch: select exactly one command-specific startup path.
fn dispatch_command(command: Command) -> Result<()> {
    match command {
        Command::Run {
            cwd,
            worktree,
            all_worktrees,
            daemon,
            remote,
            reconnect,
            mobile,
            command,
        } => {
            // In worktree-isolation mode `cwd` must resolve to a git repo; if
            // the caller didn't pass --cwd, default to the current directory so
            // the repo can be discovered.
            let cwd = if (worktree || all_worktrees) && cwd.is_none() {
                Some(std::env::current_dir()?)
            } else {
                cwd
            };

            let (specs, workspace_catalog) = if all_worktrees {
                if daemon || worktree || reconnect {
                    anyhow::bail!(
                        "--all-worktrees cannot be combined with --daemon, --worktree, or --reconnect"
                    );
                }
                prepare_all_worktree_specs_with_catalog(command, cwd.as_deref(), remote.as_deref())?
            } else {
                // The explicit command controls pane creation, not workspace
                // visibility: keep the full Orca catalog in the sidebar and
                // the `w` inventory view for every run mode.
                let catalog = orca_workspaces::list_all_with_scope().unwrap_or_default();
                (prepare_run_specs(command, remote.as_deref())?, catalog)
            };

            let mut app = App::spawn_agents_with_catalog(
                specs,
                cwd.as_deref(),
                worktree,
                workspace_catalog.workspaces,
            )?;
            app.set_workspace_catalog_scope(workspace_catalog.unresolved_host_ids);

            // Try to connect to an Orca GUI daemon (--daemon). Falls back to
            // standalone silently if no daemon is found; shows a toast if a
            // daemon was found but the connection failed.
            if daemon {
                app.try_connect_daemon();
            }

            // Feature 8: mark every pane reconnect-eligible so a dropped remote
            // session is re-spawned after a backoff (harmless without --remote).
            if reconnect {
                app.enable_reconnect();
            }

            // Feature 10: optionally start the mobile-companion WebSocket
            // server on a dedicated tokio runtime thread and feed it live
            // snapshots from the app loop. When `app` drops (run returns) the
            // sender drops too, so `serve` returns and the thread exits.
            if let Some(port) = mobile {
                let token = mobile::random_token();
                let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
                let (snap_tx, snap_rx) =
                    tokio::sync::mpsc::unbounded_channel::<Vec<mobile::AgentSnapshot>>();
                let token_for_server = token.clone();
                let addr_for_server = addr;
                std::thread::Builder::new()
                    .name("orca-mobile-ws".into())
                    .spawn(move || {
                        let Ok(runtime) = tokio::runtime::Runtime::new() else {
                            return;
                        };
                        let _ = runtime.block_on(mobile::serve(
                            addr_for_server,
                            token_for_server,
                            snap_rx,
                        ));
                    })?;
                eprintln!(
                    "orca-tui: mobile companion — ws://{addr}?token={token} \
                     (live pane status while the agents run)"
                );
                app.set_snapshot_sender(snap_tx);
            }

            app.run()?;
            Ok(())
        }

        Command::Orchestrate {
            spec,
            parallel,
            issues,
        } => {
            // Feature 7 + 9 — dependency-gated LIVE DISPATCH. Tasks come either
            // from `--spec` (free-form lines) or `--issues <owner/name>` (each
            // open GitHub issue → one task, via `gh`). Then a Coordinator drives
            // the App: sequential chain by default, --parallel fans out.
            let detected = AgentKind::detect_installed();
            let agent_bin = detected
                .first()
                .map(AgentKind::binary)
                .unwrap_or("claude")
                .to_string();

            // Resolve the task list to a newline spec the planner consumes.
            let task_spec: String = if let Some(repo) = &issues {
                let repo_ref = RepoRef::parse(repo)
                    .with_context(|| format!("parsing --issues repo {repo:?}"))?;
                let issues = integrations::list_issues(&repo_ref)?;
                if issues.is_empty() {
                    anyhow::bail!("no open issues in {repo_ref} to orchestrate");
                }
                issues
                    .iter()
                    .map(integrations::issue_to_prompt)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else if let Some(s) = spec {
                s
            } else {
                anyhow::bail!("orchestrate needs --spec <text> or --issues <owner/name>");
            };

            let coord = if parallel {
                coordinator::plan_from_spec(&task_spec, &[agent_bin.clone()])
            } else {
                coordinator::plan_chain(&task_spec, &[agent_bin.clone()])
            };
            let source = if issues.is_some() { "issues" } else { "spec" };
            let mode = if parallel { "parallel" } else { "sequential" };
            println!(
                "orca-tui: orchestrating {} task(s) via agent `{agent_bin}` \
                 ({mode}, source: {source}, task text passed as the prompt):",
                coord.tasks().len()
            );
            for task in coord.tasks() {
                println!("  [{}] {}", task.id, task.spec);
            }
            println!();

            // Start empty; the loop's pump spawns tasks as their deps allow.
            let mut app = App::spawn_agents(Vec::new(), None, false)?;
            app.set_orchestration(coord, agent_bin);
            app.run()?;
            Ok(())
        }

        Command::Prs { repo } => {
            let repo = RepoRef::parse(&repo)?;
            let prs = integrations::list_pull_requests(&repo)?;
            if prs.is_empty() {
                println!("orca-tui: no open pull requests for {repo}");
            }
            for pr in prs {
                match &pr.branch {
                    Some(b) => println!("#{}  {}  ({})", pr.number, pr.title, b),
                    None => println!("#{}  {}", pr.number, pr.title),
                }
            }
            Ok(())
        }

        Command::Issues { repo } => {
            let repo = RepoRef::parse(&repo)?;
            let issues = integrations::list_issues(&repo)?;
            if issues.is_empty() {
                println!("orca-tui: no open issues for {repo}");
            }
            for iss in issues {
                println!("#{}  {}", iss.number, iss.title);
            }
            Ok(())
        }

        Command::Mobile { port } => {
            let token = mobile::random_token();
            let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
            // No live App snapshot feed here; serve with an empty channel so
            // the server is up and accepting connections (clients hold socket).
            let (_snapshot_tx, snapshot_rx) =
                tokio::sync::mpsc::unbounded_channel::<Vec<mobile::AgentSnapshot>>();
            println!("orca-tui: mobile companion server");
            println!("  listen: ws://{addr}");
            println!("  token: {token}");
            println!("  (connect a mobile client to ws://{addr}?token={token})");
            println!("  Ctrl+C to stop.");
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(mobile::serve(addr, token, snapshot_rx))?;
            Ok(())
        }

        Command::Daemon { socket, command } => {
            use crate::daemon_server::{default_socket_path, DaemonServer};

            let socket_path = socket.unwrap_or_else(default_socket_path);

            let mut server = DaemonServer::new(&socket_path)?;

            // Parse initial agents (same `::` separator as `run`).
            if !command.is_empty() {
                let commands = split_agents(command);
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                server.spawn_initial(commands, cols.max(20), rows.max(3));
            }

            // Install SIGTERM handler for graceful shutdown.
            let shutdown_flag = server.shutdown_flag();
            install_sigterm_handler(shutdown_flag);

            server.run()?;
            Ok(())
        }

        Command::Attach { socket } => {
            use crate::daemon_server::default_socket_path;

            let socket_path = socket.unwrap_or_else(default_socket_path);
            run_attach(&socket_path)?;
            Ok(())
        }
    }
}

/// 将 CLI 的命令参数转换为 pane 启动规格。
///
/// 该函数把参数解析与 App 启动解耦，保证普通命令和 SSH 远程命令共享
/// 同一套 `::` 分段语义，也便于在不触碰终端的单元测试中验证 dispatch。
fn prepare_run_specs(command: Vec<String>, remote: Option<&str>) -> Result<Vec<AgentSpec>> {
    let commands = split_agents(command);
    if commands.is_empty() {
        anyhow::bail!("no agent command given — usage: orca-tui run -- <command>...");
    }
    let mut specs: Vec<AgentSpec> = commands.into_iter().map(AgentSpec::from_command).collect();
    if let Some(host) = remote {
        let target =
            SshTarget::parse(host).with_context(|| format!("parsing --remote host {host:?}"))?;
        for spec in &mut specs {
            spec.command = target
                .clone()
                .with_command(spec.command.clone())
                .command_vec();
        }
    }
    Ok(specs)
}

/// Build one launch spec for each worktree registered in the current repo.
/// The worktree path is retained on the spec so App can use it as the PTY cwd;
/// unlike `--worktree`, these checkouts are existing Orca/Git workspaces and
/// are not removed when the TUI exits.
#[cfg(test)]
fn prepare_all_worktree_specs(
    command: Vec<String>,
    cwd: Option<&Path>,
    remote: Option<&str>,
) -> Result<Vec<AgentSpec>> {
    Ok(prepare_all_worktree_specs_with_catalog(command, cwd, remote)?.0)
}

/// Build the launch plan and retain the complete Orca catalog for sidebar
/// rendering. Orca rows on another execution host are displayed but are not
/// handed to a local PTY; this prevents a remote path from being mistaken for
/// a local checkout while still keeping the workspace visible.
fn prepare_all_worktree_specs_with_catalog(
    command: Vec<String>,
    cwd: Option<&Path>,
    remote: Option<&str>,
) -> Result<(Vec<AgentSpec>, OrcaWorkspaceCatalog)> {
    if remote.is_some() {
        anyhow::bail!("--all-worktrees cannot be combined with --remote");
    }
    let command = if command.is_empty() {
        vec![AgentKind::detect_installed()
            .first()
            .map(AgentKind::binary)
            .unwrap_or("bash")
            .to_owned()]
    } else {
        command
    };
    // In this mode the command is replicated across worktrees, so preserve
    // ordinary agent arguments (`codex --model ...`) as one invocation. `::`
    // remains accepted for consistency with the regular run parser.
    let commands = if command.iter().any(|arg| arg == "::") {
        split_agents(command)
    } else {
        vec![command]
    };
    if commands.is_empty() {
        anyhow::bail!("no agent command given — usage: orca-tui run -- <command>...");
    }
    if commands.len() != 1 {
        anyhow::bail!("--all-worktrees accepts one agent command");
    }
    let command = commands.into_iter().next().expect("validated one command");

    let base = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if let Ok(catalog) = orca_workspaces::list_all_with_scope() {
        if !catalog.workspaces.is_empty() || !catalog.unresolved_host_ids.is_empty() {
            let mut specs = catalog
                .workspaces
                .iter()
                .filter(|workspace| workspace.is_local() && !workspace.is_archived)
                .map(|workspace| agent_spec_for_workspace(&command, workspace))
                .collect::<Vec<_>>();
            // Keep the TUI alive with a local fallback pane when Orca only
            // reported inaccessible hosts. The inventory still carries the
            // unresolved host warning instead of dropping the catalog state.
            if specs.is_empty() {
                specs.push(AgentSpec::from_command(command.clone()));
            }
            return Ok((specs, catalog));
        }
    }

    // Orca is optional. Preserve the previous local-Git behavior when its
    // catalog command is unavailable or disconnected.
    let manager = match WorktreeManager::open(&base) {
        Ok(manager) => manager,
        Err(_) => {
            return Ok((
                vec![AgentSpec::from_command(command)],
                OrcaWorkspaceCatalog::default(),
            ))
        }
    };
    let worktrees = manager.list_registered()?;
    if worktrees.is_empty() {
        return Ok((
            vec![AgentSpec::from_command(command)],
            OrcaWorkspaceCatalog::default(),
        ));
    }

    let workspaces = worktrees
        .into_iter()
        .map(|worktree| OrcaWorkspace {
            id: worktree.path.display().to_string(),
            path: worktree.path,
            branch: worktree.branch.clone(),
            display_name: worktree.branch,
            repo_id: String::new(),
            project_id: None,
            host_id: Some("local".to_owned()),
            is_archived: false,
            workspace_status: None,
            is_main_worktree: false,
        })
        .collect::<Vec<_>>();
    let specs = workspaces
        .iter()
        .map(|workspace| agent_spec_for_workspace(&command, workspace))
        .collect();
    Ok((
        specs,
        OrcaWorkspaceCatalog {
            workspaces,
            unresolved_host_ids: Vec::new(),
        },
    ))
}

fn agent_spec_for_workspace(command: &[String], workspace: &OrcaWorkspace) -> AgentSpec {
    let mut spec = AgentSpec::from_command(command.to_vec());
    spec.name = workspace.sidebar_name();
    spec.worktree_branch = Some(workspace.branch.clone());
    spec.workspace_id = Some(workspace.id.clone());
    spec.worktree = Some(workspace.path.clone());
    spec
}

/// Install a SIGTERM handler that sets the atomic shutdown flag.
fn install_sigterm_handler(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    // Use a raw signal handler via libc (avoids adding signal-hook dep).
    // SAFETY: AtomicBool::store is signal-safe.
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    static mut SHUTDOWN_FLAG: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
    unsafe {
        SHUTDOWN_FLAG = Some(flag);
        let handler = sigterm_handler as usize;
        signal(15, handler); // SIGTERM = 15
    }
    extern "C" fn sigterm_handler(_sig: i32) {
        unsafe {
            if let Some(flag) = &SHUTDOWN_FLAG {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// Connect to a daemon and run a TUI client loop.
fn run_attach(socket_path: &Path) -> Result<()> {
    use crate::daemon_server::AttachClient;
    use crate::layout::split_panes;
    use crate::pane::Pane;
    use crate::sidebar::{self, SidebarEntry};
    use crate::workspace_view::{self, WorkspaceRow};
    use base64::{engine::general_purpose, Engine as _};
    use crossterm::event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    };
    use crossterm::terminal::size as term_size;
    use crossterm::{
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    };
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;

    let (mut client, sessions) = AttachClient::connect(socket_path)?;
    // The daemon protocol only knows about live agent sessions. Load Orca's
    // global workspace catalog separately so an attach client still shows
    // every workspace across repositories and execution hosts, including
    // remote/archived rows that do not have a local PTY in this daemon.
    let (workspace_catalog, unresolved_host_ids) = match orca_workspaces::list_all_with_scope() {
        Ok(catalog) => (catalog.workspaces, catalog.unresolved_host_ids),
        Err(err) => {
            if crate::debug_log::enabled() {
                crate::debug_log::append(format_args!(
                    "{} attach workspace_catalog=unavailable error={}",
                    crate::debug_log::WORKTREE_PREFIX,
                    crate::debug_log::classify_error(err.as_ref())
                ));
            }
            (Vec::new(), Vec::new())
        }
    };
    let workspace_rows: Vec<WorkspaceRow> = workspace_catalog
        .iter()
        .map(|workspace| WorkspaceRow {
            name: workspace.sidebar_name(),
            detail: workspace.sidebar_detail().unwrap_or_default(),
        })
        .collect();
    if crate::debug_log::enabled() {
        crate::debug_log::append(format_args!(
            "{} attach session_count={} catalog_rows={} sidebar_source=daemon_sessions+orca_catalog",
            crate::debug_log::WORKTREE_PREFIX,
            sessions.len(),
            workspace_catalog.len()
        ));
    }

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (cols, rows) = term_size().unwrap_or((80, 24));
    let mut panes: Vec<Pane> = sessions
        .iter()
        .map(|s| {
            let mut p = Pane::new(s.id, &s.name, cols.max(20), rows.max(3));
            p.set_state(crate::agent::AgentState::Running);
            p
        })
        .collect();

    let mut focus: usize = 0;
    let mut workspace_open = false;
    let mut workspace_selected: usize = 0;
    let config = crate::config::Config::default();
    let theme = &config.theme;

    // Reader thread: reads NDJSON from the daemon, feeds output to a channel.
    let reader_stream = client.try_clone_stream()?;
    let (data_tx, data_rx) = mpsc::channel::<(usize, Vec<u8>)>(); // (session_id, bytes)
    let (exit_tx, exit_rx) = mpsc::channel::<(usize, Option<i32>)>(); // (session_id, code)
    std::thread::Builder::new()
        .name("orcatui-attach-reader".into())
        .spawn(move || {
            let mut reader = BufReader::new(reader_stream);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
                            let msg_type = msg.get("type").and_then(|v| v.as_str());
                            match msg_type {
                                Some("output") => {
                                    let session = msg["session"].as_u64().unwrap_or(0) as usize;
                                    if let Some(data) = msg["data"].as_str() {
                                        if let Ok(bytes) = general_purpose::STANDARD.decode(data) {
                                            let _ = data_tx.send((session, bytes));
                                        }
                                    }
                                }
                                Some("exit") => {
                                    let session = msg["session"].as_u64().unwrap_or(0) as usize;
                                    let code = msg["code"].as_i64().map(|c| c as i32);
                                    let _ = exit_tx.send((session, code));
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        })?;

    // Main attach loop.
    let result = (|| -> Result<()> {
        loop {
            // Drain pending output from the daemon.
            while let Ok((session_id, bytes)) = data_rx.try_recv() {
                if let Some(p) = panes.iter_mut().find(|p| p.id() == session_id) {
                    p.feed(&bytes);
                }
            }
            // Drain exit events.
            while let Ok((session_id, code)) = exit_rx.try_recv() {
                if let Some(p) = panes.iter_mut().find(|p| p.id() == session_id) {
                    let state = match code {
                        Some(0) | None => crate::agent::AgentState::Done(code),
                        Some(c) => crate::agent::AgentState::Failed(format!("exit code {c}")),
                    };
                    p.set_state(state);
                }
            }

            // Derive the attach sidebar before the draw closure mutably
            // borrows panes. Daemon sessions are live panes; catalog rows are
            // read-only workspace inventory entries and are intentionally
            // appended even when they have no matching local session.
            let mut sidebar_entries: Vec<SidebarEntry> = panes
                .iter()
                .map(|pane| SidebarEntry {
                    name: pane.name().to_owned(),
                    state: pane.state().clone(),
                    branch: pane.branch().map(str::to_owned),
                    activity: pane.activity().cloned(),
                    focused: false,
                    pinned: false,
                })
                .collect();
            sidebar_entries.extend(workspace_catalog.iter().map(|workspace| SidebarEntry {
                name: workspace.sidebar_name(),
                state: crate::agent::AgentState::Idle,
                branch: workspace.sidebar_detail(),
                activity: None,
                focused: false,
                pinned: false,
            }));
            if let Some(entry) = sidebar_entries.get_mut(focus) {
                entry.focused = true;
            }
            let configured_sidebar_width = config.layout.sidebar_width;

            // Render.
            terminal.draw(|f| {
                let total = f.area();
                let sidebar_width = sidebar::recommended_width(
                    &sidebar_entries,
                    configured_sidebar_width,
                    total.width,
                    22,
                );
                let show_sidebar =
                    sidebar_width > 0 && total.width > sidebar_width.saturating_add(22);
                let (sidebar_area, pane_area) = if show_sidebar {
                    let chunks = ratatui::layout::Layout::horizontal([
                        ratatui::layout::Constraint::Length(sidebar_width),
                        ratatui::layout::Constraint::Min(1),
                    ])
                    .spacing(1)
                    .split(total);
                    (Some(chunks[0]), chunks[1])
                } else {
                    (None, total)
                };
                if let Some(area) = sidebar_area {
                    sidebar::render_sidebar(
                        f,
                        area,
                        &sidebar_entries,
                        theme,
                        Some(("● Daemon", theme.success())),
                    );
                }
                let rects = split_panes(pane_area, panes.len());
                for (i, pane) in panes.iter_mut().enumerate() {
                    let pane_area = rects.get(i).copied().unwrap_or_default();
                    pane.render(f, pane_area, i == focus, theme);
                }
                if workspace_open {
                    workspace_view::render_workspace_overlay(
                        f,
                        total,
                        &workspace_rows,
                        workspace_selected,
                        &unresolved_host_ids,
                        theme,
                    );
                }
            })?;

            // Poll for input (10ms timeout — keeps the UI responsive to daemon output).
            if event::poll(std::time::Duration::from_millis(10))? {
                let ev = event::read()?;
                if let Event::Key(key) = ev {
                    if key.kind == KeyEventKind::Press {
                        // Ctrl+Q is global, including while the workspace
                        // inventory is open. Handle it before modal-specific
                        // navigation so attach clients can always detach.
                        if key.code == KeyCode::Char('q')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            break;
                        }
                        if workspace_open {
                            match key.code {
                                KeyCode::Esc | KeyCode::Char('w') => workspace_open = false,
                                KeyCode::Up | KeyCode::Char('k') => {
                                    workspace_selected = workspace_selected.saturating_sub(1);
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    if !workspace_rows.is_empty() {
                                        workspace_selected = (workspace_selected + 1)
                                            .min(workspace_rows.len().saturating_sub(1));
                                    }
                                }
                                KeyCode::PageUp => {
                                    workspace_selected = workspace_selected.saturating_sub(8);
                                }
                                KeyCode::PageDown => {
                                    if !workspace_rows.is_empty() {
                                        workspace_selected = (workspace_selected + 8)
                                            .min(workspace_rows.len().saturating_sub(1));
                                    }
                                }
                                _ => {}
                            }
                            continue;
                        }
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('w'), m) if !m.contains(KeyModifiers::CONTROL) => {
                                workspace_open = true;
                                workspace_selected = 0;
                            }
                            (KeyCode::Tab, _) => {
                                if !panes.is_empty() {
                                    focus = (focus + 1) % panes.len();
                                }
                            }
                            (KeyCode::Enter, _) => {
                                if let Some(p) = panes.get(focus) {
                                    let _ = client.write_session(p.id(), b"\r");
                                }
                            }
                            (KeyCode::Backspace, _) => {
                                if let Some(p) = panes.get(focus) {
                                    let _ = client.write_session(p.id(), &[0x7f]);
                                }
                            }
                            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                                if let Some(p) = panes.get(focus) {
                                    let mut buf = [0u8; 4];
                                    let s = c.encode_utf8(&mut buf);
                                    let _ = client.write_session(p.id(), s.as_bytes());
                                }
                            }
                            _ => {}
                        }
                    }
                } else if let Event::Mouse(mouse) = ev {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            if let Some(p) = panes.get_mut(focus) {
                                p.scroll_up(3);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if let Some(p) = panes.get_mut(focus) {
                                p.scroll_down(3);
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Exit if all panes are terminal.
            if !panes.is_empty()
                && panes.iter().all(|p| {
                    matches!(
                        p.state(),
                        crate::agent::AgentState::Done(_) | crate::agent::AgentState::Failed(_)
                    )
                })
            {
                break;
            }
        }
        Ok(())
    })();

    // Restore terminal.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;

    result
}

/// Split the trailing command list into per-agent command vectors on the
/// literal `::` separator.
///
/// - **No `::` present:** each token becomes its own one-element agent vector
///   (the original behavior, preserved for backward compatibility — e.g.
///   `claude codex` → two agents).
/// - **At least one `::` present:** the list is split into the segments
///   between `::` tokens; empty segments (from a leading, trailing, or doubled
///   `::`) are dropped.
///
/// A single consistent rule: a stray `::` switches from "one agent per token"
/// to "segment" mode. Documented in the `Run` subcommand help.
pub(crate) fn split_agents(args: Vec<String>) -> Vec<Vec<String>> {
    // No separator at all → backward-compatible one-agent-per-token.
    if !args.iter().any(|a| a == "::") {
        return args.into_iter().map(|t| vec![t]).collect();
    }

    // Split into segments between `::` tokens.
    let mut segments: Vec<Vec<String>> = vec![Vec::new()];
    for tok in args {
        if tok == "::" {
            segments.push(Vec::new());
        } else {
            segments
                .last_mut()
                .expect("segments starts with one element")
                .push(tok);
        }
    }

    // Drop empty segments (leading / trailing / doubled `::`).
    segments.into_iter().filter(|s| !s.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_agents_no_separator_is_one_per_token() {
        let got = split_agents(vec!["claude".into(), "codex".into(), "opencode".into()]);
        assert_eq!(
            got,
            vec![vec!["claude"], vec!["codex"], vec!["opencode"]],
            "no '::' → one agent per token (backward compatible)"
        );
    }

    #[test]
    fn split_agents_with_separator_is_segments() {
        let got = split_agents(vec![
            "claude".into(),
            "::".into(),
            "codex".into(),
            "--model".into(),
            "x".into(),
            "::".into(),
            "opencode".into(),
        ]);
        assert_eq!(
            got,
            vec![
                vec!["claude"],
                vec!["codex", "--model", "x"],
                vec!["opencode"]
            ],
            "'::' groups tokens into one agent"
        );
    }

    #[test]
    fn split_agents_drops_empty_segments() {
        let got = split_agents(vec![
            "::".into(),
            "echo".into(),
            "::".into(),
            "::".into(),
            "true".into(),
            "::".into(),
        ]);
        assert_eq!(got, vec![vec!["echo"], vec!["true"]]);
    }

    #[test]
    fn split_agents_single_token_no_separator() {
        let got = split_agents(vec!["claude".into()]);
        assert_eq!(got, vec![vec!["claude"]]);
    }

    #[test]
    fn split_agents_empty_input_yields_nothing() {
        assert!(split_agents(Vec::new()).is_empty());
    }

    #[test]
    fn split_agents_only_separators_yields_nothing() {
        let got = split_agents(vec!["::".into(), "::".into(), "::".into()]);
        assert!(got.is_empty(), "all-empty segments must be dropped");
    }

    #[test]
    fn prepare_run_specs_rejects_empty_commands() {
        let err = prepare_run_specs(vec!["::".into()], None).unwrap_err();
        assert!(err.to_string().contains("no agent command"));
    }

    #[test]
    fn prepare_run_specs_wraps_remote_commands() {
        let specs = prepare_run_specs(vec!["echo".into()], Some("example.com")).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].command.first().map(String::as_str), Some("ssh"));
        assert!(specs[0].command.iter().any(|arg| arg == "example.com"));
    }

    #[test]
    fn all_worktree_specs_follow_git_inventory() {
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let expected_specs = match orca_workspaces::list_all() {
            Ok(rows) if !rows.is_empty() => rows
                .iter()
                .filter(|workspace| workspace.is_local() && !workspace.is_archived)
                .count(),
            _ => WorktreeManager::open(&cwd)
                .expect("the test crate is in a git repo")
                .list_registered()
                .expect("list worktrees")
                .len(),
        };
        let specs = prepare_all_worktree_specs(vec!["echo".into()], Some(&cwd), None)
            .expect("build one spec per worktree");
        assert_eq!(specs.len(), expected_specs);
        assert!(specs.iter().all(|spec| {
            spec.worktree.is_some() && spec.worktree_branch.is_some() && spec.name != "echo"
        }));
    }

    #[test]
    fn all_worktree_specs_keep_agent_arguments_together() {
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let specs = prepare_all_worktree_specs(
            vec!["echo".into(), "workspace argument".into()],
            Some(&cwd),
            None,
        )
        .expect("build specs");
        assert!(specs
            .iter()
            .all(|spec| spec.command == ["echo", "workspace argument"]));
    }

    /// End-to-end check of the spec-building path: splitting + the empty guard.
    #[test]
    fn smoke_parse_three_agents_via_separator() {
        let commands = split_agents(vec![
            "echo".into(),
            "::".into(),
            "true".into(),
            "::".into(),
            "false".into(),
        ]);
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], vec!["echo"]);
        assert_eq!(commands[1], vec!["true"]);
        assert_eq!(commands[2], vec!["false"]);

        let specs: Vec<AgentSpec> = commands.into_iter().map(AgentSpec::from_command).collect();
        assert_eq!(specs.len(), 3);
        for s in &specs {
            assert_eq!(s.kind, crate::agent::AgentKind::Generic);
        }
    }

    // ---- clap argument-parsing coverage (struct fields + dispatch inputs) ----

    #[test]
    fn parse_run_with_all_flags() {
        let cli = Cli::try_parse_from([
            "orcatui",
            "run",
            "--cwd",
            "/tmp",
            "--worktree",
            "--all-worktrees",
            "--daemon",
            "--remote",
            "user@host",
            "--reconnect",
            "--mobile",
            "8080",
            "--",
            "claude",
        ])
        .expect("valid run invocation parses");
        match cli.command.unwrap() {
            Command::Run {
                cwd,
                worktree,
                all_worktrees,
                daemon,
                remote,
                reconnect,
                mobile,
                command,
            } => {
                assert_eq!(cwd.as_deref(), Some(std::path::Path::new("/tmp")));
                assert!(worktree, "--worktree parsed");
                assert!(all_worktrees, "--all-worktrees parsed");
                assert!(daemon, "--daemon parsed");
                assert!(reconnect, "--reconnect parsed");
                assert_eq!(remote.as_deref(), Some("user@host"));
                assert_eq!(mobile, Some(8080));
                assert_eq!(command, vec!["claude".to_string()]);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_run_defaults_when_flags_absent() {
        // Only the required trailing command is given; every flag defaults.
        let cli = Cli::try_parse_from(["orcatui", "run", "claude"]).expect("minimal run parses");
        match cli.command.unwrap() {
            Command::Run {
                cwd,
                worktree,
                all_worktrees,
                daemon,
                remote,
                reconnect,
                mobile,
                command,
            } => {
                assert!(cwd.is_none());
                assert!(!worktree);
                assert!(!all_worktrees);
                assert!(!daemon);
                assert!(remote.is_none());
                assert!(!reconnect);
                assert!(mobile.is_none());
                assert_eq!(command, vec!["claude".to_string()]);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_orchestrate_spec_and_parallel() {
        let cli = Cli::try_parse_from([
            "orcatui",
            "orchestrate",
            "--spec",
            "task one\ntask two",
            "--parallel",
        ])
        .expect("orchestrate --spec --parallel parses");
        match cli.command.unwrap() {
            Command::Orchestrate {
                spec,
                parallel,
                issues,
            } => {
                assert_eq!(spec.as_deref(), Some("task one\ntask two"));
                assert!(parallel);
                assert!(issues.is_none());
            }
            other => panic!("expected Orchestrate, got {other:?}"),
        }
    }

    #[test]
    fn parse_orchestrate_issues_overrides_spec() {
        let cli = Cli::try_parse_from(["orcatui", "orchestrate", "--issues", "owner/name"])
            .expect("orchestrate --issues parses");
        match cli.command.unwrap() {
            Command::Orchestrate {
                spec,
                parallel,
                issues,
            } => {
                assert_eq!(issues.as_deref(), Some("owner/name"));
                assert!(spec.is_none());
                assert!(!parallel, "--parallel defaults to false");
            }
            other => panic!("expected Orchestrate, got {other:?}"),
        }
    }

    #[test]
    fn parse_prs_and_issues_repo() {
        let prs =
            Cli::try_parse_from(["orcatui", "prs", "octocat/hello-world"]).expect("prs parses");
        match prs.command.unwrap() {
            Command::Prs { repo } => assert_eq!(repo, "octocat/hello-world"),
            other => panic!("expected Prs, got {other:?}"),
        }

        let issues = Cli::try_parse_from(["orcatui", "issues", "octocat/hello-world"])
            .expect("issues parses");
        match issues.command.unwrap() {
            Command::Issues { repo } => assert_eq!(repo, "octocat/hello-world"),
            other => panic!("expected Issues, got {other:?}"),
        }
    }

    #[test]
    fn parse_mobile_default_and_custom_port() {
        let default = Cli::try_parse_from(["orcatui", "mobile"]).expect("mobile parses");
        match default.command.unwrap() {
            Command::Mobile { port } => assert_eq!(port, 0, "port defaults to 0"),
            other => panic!("expected Mobile, got {other:?}"),
        }

        let custom =
            Cli::try_parse_from(["orcatui", "mobile", "--port", "9090"]).expect("mobile --port");
        match custom.command.unwrap() {
            Command::Mobile { port } => assert_eq!(port, 9090),
            other => panic!("expected Mobile, got {other:?}"),
        }
    }

    #[test]
    fn version_flag_is_recognized() {
        // clap handles `--version` by yielding a DisplayVersion error (in the
        // real binary it would print + exit). Assert the flag is wired up.
        let err = Cli::try_parse_from(["orcatui", "--version"])
            .expect_err("--version is a display action, not a normal parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn run_captures_separator_tokens_verbatim() {
        // Everything after `--` is captured verbatim, including `::`.
        let cli = Cli::try_parse_from([
            "orcatui", "run", "--", "claude", "::", "codex", "--model", "x", "::", "opencode",
        ])
        .expect("trailing capture parses");
        let command = match cli.command.unwrap() {
            Command::Run { command, .. } => command,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(
            command,
            vec!["claude", "::", "codex", "--model", "x", "::", "opencode",]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
            "clap must hand `::` and agent flags through untouched"
        );
        // And the dispatch-time step regroups them into per-agent segments.
        let agents = split_agents(command);
        assert_eq!(
            agents,
            vec![
                vec!["claude"],
                vec!["codex", "--model", "x"],
                vec!["opencode"],
            ]
        );
    }

    #[test]
    fn run_without_command_parses_empty_then_guarded_at_dispatch() {
        // clap's `trailing_var_arg` does NOT enforce a non-empty minimum, so
        // `run` with no trailing command parses successfully to an EMPTY
        // command vec. The "no agent command given" guard lives in `try_main`
        // (split_agents + bail), NOT in the parser — this test pins that
        // contract so the runtime guard is never accidentally removed.
        let cli = Cli::try_parse_from(["orcatui", "run"])
            .expect("clap accepts an empty trailing-var-arg list");
        let command = match cli.command.unwrap() {
            Command::Run { command, .. } => command,
            other => panic!("expected Run, got {other:?}"),
        };
        assert!(command.is_empty(), "no trailing args → empty command vec");
        // The dispatch-time guard fires on exactly this empty input.
        assert!(split_agents(command).is_empty());
    }

    #[test]
    fn prs_without_repo_is_a_required_arg_error() {
        let err =
            Cli::try_parse_from(["orcatui", "prs"]).expect_err("prs requires the repo positional");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn unknown_subcommand_is_an_error() {
        let err = Cli::try_parse_from(["orcatui", "frobnicate"])
            .expect_err("unknown subcommand rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
