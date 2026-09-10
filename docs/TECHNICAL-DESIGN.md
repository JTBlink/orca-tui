# orcatui 技术设计

本文面向维护者，描述当前实现的模块边界、运行路径和协议约束。用户安装和操作方法见
[根目录 README](../README.md)。协议版本与字段应以源码和 Orca 仓库对应测试为准。

## 目标与运行路径

`orcatui` 在一个 ratatui 界面中运行和观察多个命令行 Agent。每个 Agent 都有独立 PTY、
终端模拟状态和窗格；`App` 负责输入路由、生命周期、布局、状态聚合和可选编排。

当前有三条互相独立的路径：

```text
独立模式：Agent <-> portable-pty <-> Pane <-> App <-> ratatui

内置 daemon：Agent <-> DaemonServer <-> Unix socket <-> AttachClient <-> TUI

Orca daemon：Orca daemon --control/stream sockets-->
             DaemonClient <-> App <-> Pane <-> ratatui
```

内置 daemon 和 Orca GUI daemon 不是同一个服务，协议不能混用。代码分别位于
`src/daemon_server.rs` 和 `src/orca_daemon.rs`。

## 模块职责

| 模块 | 职责 |
|---|---|
| `cli.rs` | clap 参数、命令分发、Agent 参数分组 |
| `app.rs` | 主循环、输入状态机、窗格管理、渲染和运行时编排 |
| `agent.rs` | Agent 类型、命令规范和生命周期状态 |
| `bus.rs` | PTY/daemon 输出到应用的事件通道 |
| `pane.rs` | 单个终端的模拟、滚动、选择和边框 |
| `pty_session.rs` | 本机 PTY 创建、写入、resize、退出和回收 |
| `terminal_emu.rs` | vt100 ANSI 解析与 cell 网格 |
| `query.rs` / `osc.rs` / `sync.rs` | 终端能力查询、活动 OSC、mode 2026 同步输出 |
| `scheduler.rs` / `layout.rs` / `sidebar.rs` | 刷新调度、网格布局和侧边栏 |
| `daemon_server.rs` | 内置 daemon、attach 协议和会话持有 |
| `orca_daemon.rs` | Orca GUI daemon v36 客户端 |
| `worktree.rs` | Git worktree 创建、分支和生命周期清理 |
| `debug_log.rs` | 可选的脱敏诊断日志（worktree/sidebar 拓扑与终端探针） |
| `coordinator.rs` | 顺序/并行任务依赖和派发 |
| `integrations.rs` | GitHub CLI issue/PR 数据源 |
| `ssh.rs` | SSH 目标解析、命令包装和重连策略 |
| `mobile.rs` | 带 token 的 WebSocket snapshot 服务 |
| `config.rs` | 配置、主题和原子保存 |

## 独立 PTY 链路

```text
PTY bytes
  -> QueryResponder（响应 OSC/DECRQM/DA/DCS 查询）
  -> OscScanner（提取 Agent 活动）
  -> SyncScanner（原子处理 mode 2026 批次）
  -> TerminalEmulator（vt100）
  -> Pane
  -> App::render
```

子进程会注入 `TERM=xterm-256color` 和 `COLORTERM=truecolor`。应用按帧批量消费
`AgentUpdate`，由 `FrameScheduler` 控制 60 FPS 目标和空闲退避。窗格边框使用
`ratatui-ppalla` 的 `PreparedBlock`，终端 cell 仍由 ratatui 完整绘制。

`--worktree` 时，`WorktreeManager` 在仓库根目录的 `.orca-worktrees/` 下创建
`<slug>-<id>` 工作区和 `orca/<slug>-<id>` 分支；`OwnedWorktrees` 在应用销毁时尽力清理。
这组 worktree 是本地会话级资源，不等同于 Orca GUI 的全局 workspace catalog。

## 内置 daemon 协议

内置协议版本为 `1`，使用单 Unix socket、逐行 JSON 和无 token 的 hello：

```json
{"type":"hello","version":1}
```

daemon 持有 PTY，attach 客户端断开不影响 Agent。默认 socket 是
`$XDG_RUNTIME_DIR/orcatui.sock`，否则为 `/tmp/orcatui.sock`。该实现目前应视为单用户本机
服务：socket 权限和客户端认证需要在引入多用户部署前补齐。

## Orca GUI daemon v36

`orca_daemon::PROTOCOL_VERSION` 当前为 `36`。客户端连接两个 Unix socket：control 使用
NDJSON RPC，stream 使用二进制帧：

```text
[1 byte type][4 byte big-endian payload length][payload]
```

帧类型 `1` 为 PTY 数据，`2` 为 NDJSON 事件，单帧上限为 16 MiB。两个 socket 都要发送
带 token、`clientId` 和 role 的 hello；control 与 stream 的 `daemonIdentity` 必须对应同一
daemon 实例。发现逻辑查找 Orca 的 versioned `daemon-v36.sock` / token，并兼容旧布局。

维护协议适配时，必须同时核对 `src/orca_daemon.rs` 与同级 `../orca/src/main/daemon/` 中的
client、stream reader、request router 和测试。不要仅依据旧文档中的字段名或帧格式。

## CLI 与状态机约束

`split_agents` 的兼容语义：无 `::` 时每个 token 是一个 Agent；出现 `::` 时按分段形成
完整 argv；空段丢弃。Normal 模式把输入转发给焦点 Agent，`Ctrl+Alt+P` 进入 Pane 模式，
`Ctrl+Q` 为全局退出键。

`App` 通过 `PaneSlot` 聚合单个窗格的终端状态、启动命令、编排任务、daemon session、重连、
pin 和活动状态。Vec position 只承担布局与焦点索引；异步输出始终先用稳定 pane id 反查当前
位置，不能把稳定 id 当作当前 Vec 下标。未知 daemon session id 应丢弃，不能默认注入第一个
窗格。

所有 per-pane 生命周期字段均由 `PaneSlot` 唯一持有；App 不再维护 session、task、command、
reconnect、pin 或 status 的平行状态字段。新增字段必须先归入 `PaneSlot`，Vec position
只能作为布局和焦点索引。

输入边界由 `input` 模块维护 `InputMode`、`FocusDirection`、`InteractionState` 和无副作用的
`InputCommand` reducer。`App` 通过 `Deref` 暴露该聚合状态，仍只执行语义命令；PTY、daemon
与 GitHub 请求不属于交互状态，handler 不直接执行阻塞 I/O。

渲染边界由 `render_model::RenderModel` 负责从 slot 派生 sidebar entries、状态 tally，并生成
一次性的 `OverlayModel`（Jump、Spawn、Tasks、Settings、Activity、Dashboard 等 modal 的只读
视图数据），再交给 ratatui 绘制。daemon 控制面由 `daemon_connection::DaemonConnection` 包装，输入写入
通过专用 writer 线程排队，避免 UI loop 等待 RPC。
动态 session 创建同样通过短生命周期 worker connection 执行；占位 pane 在结果返回前保持
Idle，成功后再注入 snapshot 并注册稳定 session id，失败则转为 Failed/toast。

## 配置与外部集成

配置读取自 `$XDG_CONFIG_HOME/orcatui/config.toml`，否则为 `$HOME/.config/orcatui/config.toml`。
Settings 先写同目录临时文件再 rename，配置中不保存 GitHub 或 daemon 凭据。

GitHub 集成通过 `gh issue list`、`gh pr list` 和 `gh issue view` 实现；`orchestrate` 把 spec
非空行或开放 issue 转成顺序/并行任务。移动端服务当前只推送 snapshot；SSH 模式通过包装
本地 `ssh` 命令复用 PTY 链路。

## 验证与已知约束

提交前运行：

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features
```

`orcatui-inject` 用于录制和回放终端字节；`ORCA_DEBUG_LOG=1` 写入
`/tmp/orca-live.log`。当前实现还存在以下边界，修复时应补回归测试：

- daemon 初始窗格、daemon stream 断线、关闭 daemon 窗格和动态命令参数需要保持一致的会话语义。
- 内置 daemon 的 socket 认证/权限与广播背压尚未达到多用户服务要求。
- 移动服务不应在非必要场景绑定 `0.0.0.0`，配对 token 应使用 CSPRNG。
- 自定义 Spawn 命令只按空白拆分，不解析 shell 引号。
- Tasks 的 `gh` 请求为同步调用，慢网络会暂时阻塞界面。
- SSH IPv6、重连次数和 worktree 清理失败路径需要单独覆盖。
- 当前 sidebar 只从 `PaneSlot` 派生运行中 pane；不会读取 Orca CLI 的
  `worktree list/ps` 全局工作区目录。需要展示全局工作区时，应新增独立 inventory
  与 host/repo scope 处理，不能把 Git 当前仓库的数量直接当作工作台总数。
