# orcatui 技术设计

本文面向维护者，描述当前实现的模块边界、运行路径和协议约束。用户安装和操作方法见
[根目录 README](../README.md)。协议版本与字段应以源码和 Orca 仓库对应测试为准。

## 目标与运行路径

`orcatui` 在一个 ratatui 界面中运行和观察多个命令行 Agent。独立模式下每个 Agent 有本机
PTY；Orca GUI 模式下已有 tab 通过公开 CLI 以文本屏幕镜像呈现，TUI 自己创建的 tab 由 Orca runtime
持有。当前窗口只绘制活动
tab 的一个真实终端表面，切换 tab 不会销毁或重建后台会话。`App` 负责输入路由、生命周期、
布局、状态聚合和可选编排。

当前有三条互相独立的路径：

```text
独立模式：Agent <-> portable-pty <-> Pane <-> App <-> ratatui

内置 daemon：Agent <-> DaemonServer <-> Unix socket <-> AttachClient <-> TUI

Orca GUI：orca terminal list/read（CLI 子进程） -> App -> Pane -> ratatui
         TUI-owned input/create/close -> orca terminal send/create/close
```

内置 daemon 和 Orca GUI runtime 不是同一个服务，协议不能混用。内置 daemon 代码位于
`src/adapters/daemon_server.rs`，Orca 私有协议模型保留在 `src/orca/daemon.rs`，但 GUI 集成不再使用它。

## 源码目录

`src/lib.rs` 是稳定的模块 seam：源码按职责放入以下目录，但保留现有
`orca_tui::app`、`orca_tui::pane` 等公共模块路径，避免目录整理破坏调用方。

| 目录 | 职责 |
|---|---|
| `src/core/` | Agent 身份、活动记录、任务协调等领域状态 |
| `src/app/` | CLI、应用状态机、输入命令、事件总线和帧调度 |
| `src/ui/` | 布局、Pane、Sidebar、TabBar、Overlay、Toast 和渲染模型 |
| `src/terminal/` | PTY 生命周期、终端模拟和 OSC/查询/同步输出协议 |
| `src/orca/` | Orca daemon 协议与跨 host workspace catalog |
| `src/adapters/` | 内置 daemon、Git/GitHub、SSH、移动端和剪贴板适配 |
| `src/support/` | 配置、崩溃日志、脱敏诊断和性能探针 |

## 模块职责

| 模块 | 职责 |
|---|---|
| `cli.rs` | clap 参数、命令分发、Agent 参数分组 |
| `app.rs` | 主循环、输入状态机、终端 tab 管理、渲染和运行时编排 |
| `agent.rs` | Agent 类型、命令规范和生命周期状态 |
| `bus.rs` | PTY/daemon 输出到应用的事件通道 |
| `pane.rs` | 单个终端的模拟、滚动、选择和边框 |
| `pty_session.rs` | 本机 PTY 创建、写入、resize、退出和回收 |
| `terminal_emu.rs` | vt100 ANSI 解析与 cell 网格 |
| `query.rs` / `osc.rs` / `sync.rs` | 终端能力查询、活动 OSC、mode 2026 同步输出 |
| `scheduler.rs` / `layout.rs` / `sidebar.rs` / `tab_bar.rs` | 刷新调度、兼容网格算法、侧边栏和终端 tabs |
| `daemon_server.rs` | 内置 daemon、attach 协议和会话持有 |
| `orca_daemon.rs` | Orca GUI daemon v36 客户端 |
| `cli_bridge.rs` | Orca 公共 `terminal` CLI 的 list/read/send/create/close 适配 |
| `orca_workspaces.rs` | Orca CLI 全局 workspace catalog 读取、完整性校验和降级 |
| `workspace_view.rs` | 完整 workspace inventory overlay 与窗口化滚动 |
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
`AgentUpdate`，由 `FrameScheduler` 控制 60 FPS 目标和空闲退避。终端边框使用
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

## Orca GUI 公共 CLI 集成

`run --daemon` 不再连接 Orca 的私有 Unix socket，也不使用 `createOrAttach`、stream reader、
`write`、`resize` 或 `kill`。`cli_bridge.rs` 通过 argv 调用以下公开能力：

- `orca terminal list --json --include-visual-layouts`：发现 handle、ptyId、标题、工作区和顺序；
- `orca terminal read --terminal <handle> --screen --json`：读取文本屏幕镜像；
- `orca terminal send --terminal <handle> ... --json`：仅发送到 TUI 明确创建的会话；
- `orca terminal create/close --json`：创建和关闭 TUI-owned tab。

后台 poller 每 500ms 刷新 list/read，结果经 `AgentBus` 进入 UI。已有 Orca tab 标记为
`SessionOwner::OrcaExisting`，只读展示；关闭 TUI 或点击其 `×` 只移除本地视图，不改变 Orca
会话。`+`/`n` 创建的 tab 标记为 `OrcaTuiOwned`，输入走 `terminal send`，退出时才调用
`terminal close --tab`。这条 ownership seam 保证 TUI 独占自己的 raw mode，而不会取得或释放
Orca GUI 的输入 attachment。

`read --screen` 是文本投影，不包含完整 ANSI/alternate-screen 状态；因此 GUI-owned pane
可能丢失部分颜色和终端交互语义，这是避免输入冲突的明确取舍。若未来 Orca 提供公开的
serialized snapshot/stream API，可在 `OrcaCliBridge` 内替换实现而不改变 App 接口。

维护协议适配时，必须同时核对 `src/orca/daemon.rs` 与同级 `../orca/src/main/daemon/` 中的
client、stream reader、request router 和测试。不要仅依据旧文档中的字段名或帧格式。

## CLI 与状态机约束

`split_agents` 的兼容语义：无 `::` 时每个 token 是一个 Agent；出现 `::` 时按分段形成
完整 argv；空段丢弃。Normal 模式把输入转发给焦点 Agent，`Ctrl+Alt+P` 进入 Pane 模式，
`Ctrl+Q` 为全局退出键。

`App` 通过 `PaneSlot` 聚合单个窗格的终端状态、启动命令、编排任务、daemon session、Orca
runtime handle、会话归属、重连、
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
一次性的 `OverlayModel`（Jump、Spawn、Tasks、Settings、Activity、Dashboard、完整 Workspaces
清单等 modal 的只读视图数据），再交给 ratatui 绘制。`w` 打开可滚动的全量 workspace inventory，
所以 sidebar 的视口窗口化不会静默丢失前面的 workspace。内置 daemon 控制面由
`daemon_connection::DaemonConnection` 包装；GUI runtime 控制面由 `orca_cli_bridge::OrcaCliBridge`
包装，输入写入通过专用 worker 线程排队，避免 UI loop 等待外部进程。
动态 Orca session 创建同样通过 worker 执行；占位 pane 在结果返回前保持 Idle，成功后记录
runtime handle 并注入 screen projection，失败则转为 Failed/toast。

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

可选的 `orca-tui-inject` 用于录制和回放终端字节（启用 `inject` feature）；`ORCA_DEBUG_LOG=1` 写入
`/tmp/orca-live.log`。当前实现还存在以下边界，修复时应补回归测试：

- daemon 初始窗格、daemon stream 断线、关闭 daemon 窗格和动态命令参数需要保持一致的会话语义。
- 内置 daemon 的 socket 认证/权限与广播背压尚未达到多用户服务要求。
- 移动服务不应在非必要场景绑定 `0.0.0.0`，配对 token 应使用 CSPRNG。
- 自定义 Spawn 命令只按空白拆分，不解析 shell 引号。
- Tasks 的 `gh` 请求为同步调用，慢网络会暂时阻塞界面。
- SSH IPv6、重连次数和 worktree 清理失败路径需要单独覆盖。
- 普通启动和 `run --all-worktrees` 会优先读取 Orca CLI 的
  `worktree list --json` 全局 catalog，并主动枚举 `environment list --json` 中的所有已配对
  runtime；这与 Orca 桌面的 all-host catalog 加载边界一致，不依赖本机先在
  `hostScope.omittedHostIds` 中记录远端。跨 host 合并以“来源 runtime + execution host +
  workspace ID”去重，保留不同 host 上 ID 相同的 workspace。local checkout 映射到 pane，
  远程或不可访问的 workspace 作为只读 sidebar 条目保留。对当前 CLI 无法覆盖的 host，`w`
  inventory 会显示 `not covered`；未返回 `hostScope` 的旧版 host 显示 `scope unknown`，避免把
  不完整结果伪装成完整目录。
  仅当 Orca CLI 不可用时才回退到当前 Git 仓库的 `git worktree list`，不能把 Git 当前仓库的
  数量当作全局工作台总数。`attach` 会将 daemon session 与这份 catalog 并列渲染，daemon
  协议不负责提供 workspace inventory。

## Workspace 与终端 Tab 布局

主界面遵循 Herdr 的层级：左侧常驻 workspace 导航，右侧顶部为 tab strip，剩余区域是活动
tab 的单个终端 surface：

```text
┌──────────────┬──────────────────────────────────────┐
│ workspaces   │ [● tab-1 ×] [○ tab-2 ×] [+]         │
│              ├──────────────────────────────────────┤
│              │ 当前活动 tab 的完整 PTY 终端          │
└──────────────┴──────────────────────────────────────┘
```

tab 只是视图选择器；所有 tab 复用同一个 TUI 终端 surface，后端会话由 `PaneSlot` 持续消费，
切回时直接显示最新状态。横向 tab 只投影当前焦点工作区的终端；鼠标点击 tab、`+` 或左侧
workspace 均可导航，点击 tab 右侧 `×` 可关闭当前 TUI tab；只有 `SessionOwner::OrcaTuiOwned`
才调用 `terminal close --tab`。退出 TUI 窗口不会清理 Orca-existing sessions。普通模式下 `Tab` /
`Shift+Tab` 也只在该工作区内循环切换 tab。这样终端
尺寸按整个内容区同步给活动 PTY，不再按 split pane
网格缩小，fullscreen TUI 与真实当前终端的行为保持一致。
