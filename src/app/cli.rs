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
    about = "终端多智能体编程编排器（Orca GUI 的 TUI 移植版）",
    subcommand_help_heading = "命令",
    subcommand_value_name = "命令",
    next_help_heading = "选项",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true,
    help_template = "{about-section}用法：{usage}\n\n{all-args}{after-help}",
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// 打印帮助信息
    #[arg(short = 'h', long = "help", action = clap::ArgAction::Help, global = true)]
    _help: Option<bool>,

    /// 打印版本信息
    #[arg(short = 'V', long = "version", action = clap::ArgAction::Version)]
    _version: Option<bool>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 在终端 tabs 中运行一个或多个智能体。
    ///
    /// 尾部的命令列表以 `::` 分隔符分割为各智能体的命令，每个命令
    /// 获得自己的窗格，例如：
    ///
    /// - `orca-tui run -- claude codex opencode` → 三个终端 tabs。
    /// - `orca-tui run -- claude :: codex --model x :: opencode` → 三个 tabs；
    ///   中间智能体的命令为 `codex --model x`。
    ///
    /// **分割规则：**如果存在 `::` 则按 `::` 分割（开头/结尾/连续
    /// `::` 产生的空段会被丢弃）；如果不存在 `::` 则每个词各自为一个
    /// 智能体（向后兼容）。
    Run {
        /// 所有智能体共享的工作目录。默认为当前目录。
        #[arg(long, value_name = "目录")]
        cwd: Option<PathBuf>,

        /// 为每个智能体创建独立的 git worktree。要求 `cwd`（或当前目录）
        /// 位于 git 仓库内；worktree 创建在 `.orca-worktrees/` 下，应用
        /// 退出时删除。
        #[arg(long)]
        worktree: bool,

        /// 为当前仓库中注册的每个 Git worktree 启动一个终端 tab。每个 tab 在
        /// 对应 worktree 的检出目录中运行。
        #[arg(long)]
        all_worktrees: bool,

        /// 尝试连接到运行中的 Orca GUI 守护进程以实现会话持久化和多客户端
        /// （GUI + TUI）。连接成功时仅展示/附加 Orca 已有终端，不会因尾部
        /// 命令参数自动创建 agent；新终端请在 TUI 中使用 `+` 或 `n`。如果
        /// 未找到守护进程或连接失败，则对显式命令回退到独立模式（直接 PTY）。
        #[arg(long)]
        daemon: bool,

        /// 通过 SSH 在远程主机上运行每个智能体（功能 8）。主机格式为
        /// `user@host`、`host` 或 `user@host:port`；每个智能体命令会被
        /// 包装为 `ssh <opts> <host> <command...>`。
        #[arg(long, value_name = "主机")]
        remote: Option<String>,

        /// 配合 `--remote` 使用：远程会话断开后，在同一窗格中按指数退避
        /// 自动重连，最多尝试若干次（功能 8）。
        #[arg(long)]
        reconnect: bool,

        /// 在运行智能体的同时启动移动端伴侣 WebSocket 服务器（功能 10），
        /// 向手机/PWA 广播实时窗格状态。启动时会打印 URL 和一次性令牌。
        #[arg(long, value_name = "端口")]
        mobile: Option<u16>,

        /// 一个或多个智能体调用。用 `::` 分隔的每个命令成为一个终端 tab。
        /// 不含 `::` 时，每个词为一个智能体。`--` 之后的内容会被原样
        /// 捕获，包括针对智能体本身的标志。
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            num_args = 0..,
            value_name = "命令",
        )]
        command: Vec<String>,
    },

    /// 根据规格说明规划多智能体编排（功能 7）。
    ///
    /// 将规格说明拆分为任务（每个非空行一个任务），按依赖关系调度到
    /// 智能体：默认**顺序执行**（每个任务依赖前一个），同一时间只有
    /// 一个任务运行；传入 `--parallel` 可同时并发所有任务。每个任务
    /// 的描述作为提示参数传递给智能体。
    Orchestrate {
        /// 以换行分隔的任务规格。使用引号包裹的多行字符串。指定
        /// `--issues` 时忽略。
        #[arg(long)]
        spec: Option<String>,
        /// 并行执行所有任务，而不是顺序执行。
        #[arg(long)]
        parallel: bool,
        /// 从 GitHub 仓库的开放问题中获取任务（`owner/name`）；每个问题
        /// 成为一个任务（功能 9，通过 `gh`）。优先于 `--spec`。
        #[arg(long, value_name = "仓库")]
        issues: Option<String>,
    },

    /// 启动移动端伴侣 WebSocket 服务器（功能 10）。
    ///
    /// 绑定一个本地 WebSocket 服务器，手机/PWA 可连接。打印 URL 和
    /// 一次性配对令牌。如需在智能体运行时获取实时快照，请优先使用
    /// `run --mobile <端口>`。
    Mobile {
        /// 绑定端口。默认为 0（由操作系统分配）。
        #[arg(long, default_value_t = 0)]
        port: u16,
    },

    /// 启动内置守护进程服务器（作为 systemd/supervisor 服务运行）。
    ///
    /// 拥有智能体 PTY 并通过 Unix 套接字服务 `orca-tui attach` 客户端。
    /// 智能体在客户端断开后继续存活——守护进程持续运行直到所有智能体退出
    /// 且无客户端连接，或收到 SIGTERM。
    ///
    /// 设计用于 `systemctl --user start orca-tui` 或等效方式。日志输出到
    /// stdout/stderr（由 journald/supervisor 捕获）。
    Daemon {
        /// Unix 套接字路径。默认为 `$XDG_RUNTIME_DIR/orcatui.sock`。
        #[arg(long, value_name = "路径")]
        socket: Option<PathBuf>,

        /// 智能体命令（与 `run` 使用相同的 `::` 分隔符）。如果省略，
        /// 守护进程以空状态启动，客户端通过协议创建会话。
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "命令"
        )]
        command: Vec<String>,
    },

    /// 作为 TUI 客户端连接到运行中的 orca-tui 守护进程。
    ///
    /// 连接到守护进程的 Unix 套接字，渲染所有实时智能体窗格，
    /// 并转发键盘输入。多个客户端可同时连接。
    /// 断开连接（Ctrl+Q）不会终止智能体——它们继续在守护进程中运行。
    Attach {
        /// Unix 套接字路径。默认为 `$XDG_RUNTIME_DIR/orcatui.sock`。
        #[arg(long, value_name = "路径")]
        socket: Option<PathBuf>,
    },
}

fn try_main(cli: Cli) -> Result<()> {
    dispatch_command(cli.command.unwrap_or_else(default_command))
}

/// Resolve the no-subcommand convenience behavior without mixing it into
/// subcommand dispatch.
fn default_command() -> Command {
    // Prefer the Orca GUI daemon when its versioned endpoint is present. This
    // makes a bare `orca-tui` launch a true viewer of every currently open
    // Orca terminal instead of creating local worktree panes. The built-in
    // daemon remains the fallback when no Orca GUI endpoint is discoverable.
    if crate::orca_daemon::DaemonEndpoint::discover().is_some() {
        return Command::Run {
            cwd: None,
            worktree: false,
            all_worktrees: false,
            daemon: true,
            remote: None,
            reconnect: false,
            mobile: None,
            command: Vec::new(),
        };
    }
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
            if daemon && worktree {
                anyhow::bail!(
                    "--daemon cannot be combined with --worktree; Orca daemon owns workspace PTYs"
                );
            }
            // In worktree-isolation mode `cwd` must resolve to a git repo; if
            // the caller didn't pass --cwd, default to the current directory so
            // the repo can be discovered.
            let cwd = if (worktree || all_worktrees) && cwd.is_none() {
                Some(std::env::current_dir()?)
            } else {
                cwd
            };

            // Preserve the explicit command list so daemon mode can use it
            // only if connection fails and we fall back to standalone PTYs.
            let command_for_fallback = command.clone();
            let (specs, workspace_catalog) = if all_worktrees {
                if daemon || worktree || reconnect {
                    anyhow::bail!(
                        "--all-worktrees cannot be combined with --daemon, --worktree, or --reconnect"
                    );
                }
                prepare_all_worktree_specs_with_catalog(command, cwd.as_deref(), remote.as_deref())?
            } else {
                // Keep the full Orca catalog in the sidebar and the `w`
                // inventory view for every run mode. In daemon mode the
                // explicit command is deliberately not a startup create;
                // it is retained separately for standalone fallback.
                let catalog = orca_workspaces::list_all_with_scope().unwrap_or_default();
                // A GUI daemon owns the PTYs that are already open in Orca.
                // Startup in daemon mode is therefore an attach/hydrate
                // operation; creating a terminal is an explicit in-TUI action
                // (`+`, `n`, or the custom-command picker). Keep CLI command
                // arguments available only for the standalone fallback below.
                let specs = if daemon {
                    Vec::new()
                } else {
                    prepare_run_specs(command, remote.as_deref())?
                };
                (specs, catalog)
            };
            let loaded_from_orca = workspace_catalog.loaded_from_orca;
            let unresolved_host_ids = workspace_catalog.unresolved_host_ids;
            let unverifiable_scope_host_ids = workspace_catalog.unverifiable_scope_host_ids;
            let catalog_rows = workspace_catalog.workspaces;
            // In daemon mode do not create local PTYs before connecting. The
            // app hydrates every live Orca session into a tab. Explicit
            // terminal creation happens from the TUI; CLI commands are kept
            // only for the no-daemon standalone fallback.
            let launch_specs = if daemon { Vec::new() } else { specs };
            let initial_specs = if daemon {
                Vec::new()
            } else {
                launch_specs.clone()
            };
            let mut app = App::spawn_agents_with_catalog(
                initial_specs,
                cwd.as_deref(),
                worktree,
                catalog_rows,
            )?;
            app.set_workspace_catalog_status(
                loaded_from_orca,
                unresolved_host_ids,
                unverifiable_scope_host_ids,
            );

            // Try to connect to an Orca GUI daemon (--daemon). Falls back to
            // standalone silently if no daemon is found; shows a toast if a
            // daemon was found but the connection failed.
            if daemon {
                // Orca v36 keeps exactly one attachment owner per PTY
                // session. Starting the viewer from inside an Orca terminal
                // would therefore steal every GUI session's attachment and
                // leave the GUI unable to receive input or output. There is
                // no observer/read-only attachment role in this protocol, so
                // fail closed instead of damaging the live Orca surface.
                if std::env::var_os("ORCA_TERMINAL_HANDLE").is_some()
                    || std::env::var("HERDR_ENV").as_deref() == Ok("1")
                {
                    anyhow::bail!(
                        "nested orca-tui is disabled inside an Orca/Herdr terminal; \
                         launch it from a separate system shell to avoid taking over PTYs"
                    );
                }
                app.try_connect_daemon();
                if !app.daemon_connected() && command_for_fallback.is_empty() {
                    anyhow::bail!(
                        "Orca daemon is unavailable and no standalone command was given — usage: orca-tui run --daemon -- <command>..."
                    );
                }
                // When connected, the app is a pure Orca session viewer:
                // hydrate_daemon_sessions() has already populated every live
                // terminal and no command is auto-created. If no daemon was
                // found, retain the historical standalone fallback for an
                // explicitly supplied command.
                if !app.daemon_connected() && !command_for_fallback.is_empty() {
                    let fallback_specs =
                        prepare_run_specs(command_for_fallback, remote.as_deref())?;
                    app.spawn_specs(fallback_specs);
                }
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
            catalog_source_host_id: None,
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
            unverifiable_scope_host_ids: Vec::new(),
            loaded_from_orca: false,
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
        let handler = sigterm_handler as *const () as usize;
        signal(15, handler); // SIGTERM = 15
    }
    extern "C" fn sigterm_handler(_sig: i32) {
        unsafe {
            let ptr = &raw const SHUTDOWN_FLAG;
            if let Some(flag) = &*ptr {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// The attach path predates `App::run`, so it has its own terminal lifecycle.
/// Keep that lifecycle guarded: a failed draw, input read, or thread spawn must
/// never leave the user's shell in raw mode or the alternate screen.
struct AttachTerminalGuard {
    active: bool,
}

impl AttachTerminalGuard {
    fn enter() -> Result<Self> {
        use crossterm::{
            event::EnableMouseCapture,
            execute,
            terminal::{
                disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
            },
        };

        enable_raw_mode().context("enabling raw mode")?;
        let mut stdout = std::io::stdout();
        // Set the flag before the fallible execute so the guard can clean up
        // even if entering the alternate screen or enabling mouse capture
        // fails halfway through.
        let mut guard = Self { active: true };
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = execute!(stdout, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            guard.active = false;
            return Err(error).context("entering alt screen + mouse");
        }
        Ok(guard)
    }

    fn restore<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
    ) -> Result<()> {
        use crossterm::{
            event::DisableMouseCapture,
            execute,
            terminal::{disable_raw_mode, LeaveAlternateScreen},
        };

        if !self.active {
            return Ok(());
        }
        let mut stdout = std::io::stdout();
        execute!(stdout, DisableMouseCapture, LeaveAlternateScreen)
            .context("leaving alt screen + mouse")?;
        disable_raw_mode().context("disabling raw mode")?;
        terminal.show_cursor().context("showing cursor")?;
        self.active = false;
        Ok(())
    }
}

impl Drop for AttachTerminalGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        use crossterm::{
            cursor::Show,
            event::DisableMouseCapture,
            execute,
            terminal::{disable_raw_mode, LeaveAlternateScreen},
        };
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen, Show);
        let _ = disable_raw_mode();
        self.active = false;
    }
}

/// Connect to a daemon and run the legacy built-in-daemon TUI client loop.
fn run_attach(socket_path: &Path) -> Result<()> {
    use crate::daemon_server::AttachClient;
    use crate::pane::Pane;
    use crate::sidebar::{self, SidebarEntry};
    use crate::tab_bar::{self, TabItem};
    use crate::workspace_view::{self, WorkspaceRow};
    use base64::{engine::general_purpose, Engine as _};
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
    use crossterm::terminal::size as term_size;
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;

    let (mut client, sessions) = AttachClient::connect(socket_path)?;
    // The daemon protocol only knows about live agent sessions. Load Orca's
    // global workspace catalog separately so an attach client still shows
    // every workspace across repositories and execution hosts, including
    // remote/archived rows that do not have a local PTY in this daemon.
    let (
        workspace_catalog,
        workspace_catalog_loaded,
        unresolved_host_ids,
        unverifiable_scope_host_ids,
    ) = match orca_workspaces::list_all_with_scope() {
        Ok(catalog) => (
            catalog.workspaces,
            catalog.loaded_from_orca,
            catalog.unresolved_host_ids,
            catalog.unverifiable_scope_host_ids,
        ),
        Err(err) => {
            if crate::debug_log::enabled() {
                crate::debug_log::append(format_args!(
                    "{} attach workspace_catalog=unavailable error={}",
                    crate::debug_log::WORKTREE_PREFIX,
                    crate::debug_log::classify_error(err.as_ref())
                ));
            }
            (Vec::new(), false, Vec::new(), Vec::new())
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

    // Set up terminal under an RAII guard. This guard remains active until the
    // normal restore below succeeds, and also handles every early return.
    let mut terminal_guard = AttachTerminalGuard::enter()?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let (mut cols, mut rows) = term_size().unwrap_or((80, 24));
    let mut panes: Vec<Pane> = sessions
        .iter()
        .map(|s| {
            let mut p = Pane::new(s.id, &s.name, cols.max(20), rows.max(3));
            p.set_state(crate::agent::AgentState::Running);
            p
        })
        .collect();

    let mut focus: usize = 0;
    let mut tab_hitboxes = crate::tab_bar::TabHitboxes::default();
    let mut sidebar_hitboxes: Vec<Option<ratatui::layout::Rect>> = Vec::new();
    let mut sidebar_rect: Option<ratatui::layout::Rect> = None;
    let mut last_resize: Option<(usize, u16, u16)> = None;
    let mut active_surface_size = (cols, rows);
    let mut workspace_open = false;
    let mut workspace_selected: usize = 0;
    let config = crate::config::Config::default();
    let theme = &config.theme;

    // Reader thread: reads NDJSON from the daemon, feeds output to a channel.
    let reader_stream = client.try_clone_stream()?;
    let (data_tx, data_rx) = mpsc::channel::<(usize, Vec<u8>)>(); // (session_id, bytes)
    let (exit_tx, exit_rx) = mpsc::channel::<(usize, Option<i32>)>(); // (session_id, code)
    let (created_tx, created_rx) = mpsc::channel::<(usize, String)>();
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
                                Some("created") => {
                                    let session = msg["id"].as_u64().unwrap_or(0) as usize;
                                    let name = msg["name"].as_str().unwrap_or("bash").to_owned();
                                    let _ = created_tx.send((session, name));
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
            while let Ok((session_id, name)) = created_rx.try_recv() {
                let mut pane = Pane::new(session_id, name, cols.max(20), rows.max(3));
                pane.set_state(crate::agent::AgentState::Running);
                panes.push(pane);
                focus = panes.len().saturating_sub(1);
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
            let tab_items: Vec<TabItem> = panes
                .iter()
                .map(|pane| TabItem {
                    label: pane.name().to_owned(),
                    status: None,
                })
                .collect();

            // Render.
            terminal.draw(|f| {
                let total = f.area();
                let sidebar_width = sidebar::recommended_width(
                    &sidebar_entries,
                    configured_sidebar_width,
                    total.width,
                    12,
                );
                let show_sidebar =
                    sidebar_width > 0 && total.width > sidebar_width.saturating_add(8);
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
                sidebar_rect = sidebar_area;
                sidebar_hitboxes = sidebar_area
                    .map(|area| sidebar::entry_hit_rects(area, &sidebar_entries, true))
                    .unwrap_or_default();
                if let Some(area) = sidebar_area {
                    sidebar::render_sidebar(
                        f,
                        area,
                        &sidebar_entries,
                        theme,
                        Some(("● Daemon", theme.success())),
                    );
                }
                let chrome = ratatui::layout::Layout::vertical([
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Min(1),
                ])
                .split(pane_area);
                tab_hitboxes = tab_bar::render_tab_bar(f, chrome[0], &tab_items, focus, theme);
                if let Some(pane) = panes.get_mut(focus) {
                    let inner_w = chrome[1].width.saturating_sub(2).max(20);
                    let inner_h = chrome[1].height.saturating_sub(2).max(3);
                    if pane.size() != (inner_w, inner_h) {
                        pane.resize_viewport(inner_w, inner_h);
                    }
                    active_surface_size = (inner_w, inner_h);
                    pane.render(f, chrome[1], true, theme);
                }
                if workspace_open {
                    workspace_view::render_workspace_overlay(
                        f,
                        total,
                        workspace_view::WorkspaceOverlay {
                            rows: &workspace_rows,
                            selected: workspace_selected,
                            catalog_loaded: workspace_catalog_loaded,
                            unresolved_hosts: &unresolved_host_ids,
                            unverifiable_scope_hosts: &unverifiable_scope_host_ids,
                        },
                        theme,
                    );
                }
            })?;

            if let Some(pane) = panes.get(focus) {
                let key = (pane.id(), active_surface_size.0, active_surface_size.1);
                if last_resize != Some(key) {
                    let _ = client.resize_session(
                        pane.id(),
                        active_surface_size.0,
                        active_surface_size.1,
                    );
                    last_resize = Some(key);
                }
            }

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
                            (KeyCode::BackTab, _) => {
                                if !panes.is_empty() {
                                    focus = if focus == 0 {
                                        panes.len() - 1
                                    } else {
                                        focus - 1
                                    };
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
                } else if let Event::Resize(new_cols, new_rows) = ev {
                    cols = new_cols.max(20);
                    rows = new_rows.max(3);
                    last_resize = None;
                } else if let Event::Mouse(mouse) = ev {
                    match mouse.kind {
                        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                            if tab_hitboxes.quit.is_some_and(|r| {
                                mouse.column >= r.x
                                    && mouse.column < r.right()
                                    && mouse.row >= r.y
                                    && mouse.row < r.bottom()
                            }) {
                                break;
                            }
                            if let Some(idx) = tab_hitboxes.tabs.iter().position(|r| {
                                mouse.column >= r.x
                                    && mouse.column < r.right()
                                    && mouse.row >= r.y
                                    && mouse.row < r.bottom()
                            }) {
                                if idx < panes.len() {
                                    focus = idx;
                                }
                            } else if tab_hitboxes.add.is_some_and(|r| {
                                mouse.column >= r.x
                                    && mouse.column < r.right()
                                    && mouse.row >= r.y
                                    && mouse.row < r.bottom()
                            }) {
                                let command = vec!["bash".to_owned()];
                                let _ = client.create_session("bash", &command, cols, rows);
                            } else if sidebar_rect.is_some_and(|r| {
                                mouse.column >= r.x
                                    && mouse.column < r.right()
                                    && mouse.row >= r.y
                                    && mouse.row < r.bottom()
                            }) {
                                if let Some(idx) = sidebar_hitboxes.iter().position(|r| {
                                    r.is_some_and(|rect| {
                                        mouse.column >= rect.x
                                            && mouse.column < rect.right()
                                            && mouse.row >= rect.y
                                            && mouse.row < rect.bottom()
                                    })
                                }) {
                                    if idx < panes.len() {
                                        focus = idx;
                                    } else if let Some(workspace) =
                                        workspace_catalog.get(idx.saturating_sub(panes.len()))
                                    {
                                        // The built-in daemon protocol does
                                        // not carry workspace IDs. Match an
                                        // attach session by its stable display
                                        // name when possible; otherwise leave
                                        // the catalog row read-only.
                                        if let Some(pane_idx) = panes.iter().position(|pane| {
                                            pane.name() == workspace.sidebar_name()
                                                || pane.name() == workspace.display_name
                                        }) {
                                            focus = pane_idx;
                                        }
                                    }
                                }
                            }
                        }
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

    // Restore terminal on the success path; the guard's Drop remains a
    // panic/error fallback if any teardown operation itself fails.
    terminal_guard.restore(&mut terminal)?;
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
    fn removed_prs_and_issues_subcommands_are_rejected() {
        for argv in [
            ["orcatui", "prs", "octocat/hello-world"],
            ["orcatui", "issues", "octocat/hello-world"],
        ] {
            let err = Cli::try_parse_from(argv).expect_err("legacy command is removed");
            assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
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
    fn removed_prs_without_repo_is_an_invalid_subcommand_error() {
        let err =
            Cli::try_parse_from(["orcatui", "prs"]).expect_err("legacy prs command is removed");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn unknown_subcommand_is_an_error() {
        let err = Cli::try_parse_from(["orcatui", "frobnicate"])
            .expect_err("unknown subcommand rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
