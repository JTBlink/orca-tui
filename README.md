# orca-tui

`orca-tui` 是一个终端多 Agent 编排工具。它把 Claude Code、Codex、OpenCode、Gemini CLI
等命令行 Agent 运行在独立 PTY 中，并在同一个 TUI 里并排展示、切换和管理。

项目支持直接管理本机 PTY、连接内置 daemon，以及连接 Orca GUI daemon。当前 crate 版本为
`0.5.0`。

推送形如 `v0.5.0` 的 Git 标签会触发 GitHub Actions 自动验证、构建并发布 Linux（x86_64、
aarch64）和 macOS（x86_64、arm64）压缩包；每个压缩包同时附带 SHA-256 校验文件。也可以在
Actions 页面手动运行 `Release packages`，指定一个已有标签重新发布构建产物。

## 主要能力

- 多 Agent 自适应网格和终端输出渲染
- Normal 模式输入直通当前 Agent，Pane 模式管理窗格
- 每个 Agent 独立 Git worktree
- SSH 远程执行、断线重连、运行时新增 Agent
- Activity 时间线、Tasks 任务浏览、Settings 设置和状态看板
- GitHub issue / PR 集成（通过 `gh`）
- 内置 daemon 持久会话和多客户端 attach
- Orca GUI daemon v36 客户端
- 移动端 WebSocket 状态服务
- `orca-tui-inject` 终端录制 / 回放工具

## 环境要求

- Rust stable 与 Cargo
- Linux、macOS 或 WSL（实现依赖 Unix socket 和 Unix 信号）
- 使用 `--worktree` 需要 Git，且工作目录必须在 Git 仓库内
- 使用 GitHub 功能需要已安装并登录 [GitHub CLI](https://cli.github.com/)
- 至少安装一个 Agent CLI；未检测到时默认使用 `bash`

## 安装与启动

```bash
# 从 crates.io 安装
cargo install orca-tui

# 从当前源码安装
cargo install --path .

# 自动检测 Agent，未检测到时使用 bash
orca-tui

# 在源码目录直接启动（自动复用已构建二进制）
./orca-tui.sh
```

直接启动时，TUI 会优先读取 Orca 的全局 workspace catalog，并为每个可访问的 local
checkout 建立一个 pane；Agent 会在对应 checkout 中运行。远程或不可访问的 workspace
也会保留在侧边栏中。也可以显式使用
`--all-worktrees`：

```bash
orca-tui run --all-worktrees -- codex
```

该模式使用已有工作区，不会在退出时删除它们。`--worktree` 仍表示为本次运行创建临时
隔离 worktree，两者不能同时使用。Orca catalog 不可用时，会回退到当前 Git 仓库的
worktree 清单；无法发现 Git 仓库时再回退为普通单 pane 模式。

运行 Agent：

```bash
# 单个 Agent
orca-tui run -- claude

# 不含 :: 时，每个 token 都是一个 Agent
orca-tui run -- claude codex opencode

# 含 :: 时，按段分组，可为 Agent 传参数
orca-tui run -- claude :: codex --model gpt-5 :: opencode

# 指定工作目录和独立 worktree
orca-tui run --cwd ./my-repo --worktree -- claude :: codex
```

参数分组规则：没有 `::` 时，`--` 后每个 token 启动一个 Agent；出现至少一个 `::` 后，
每个分段作为一个完整命令，空段会被忽略。需要给单个 Agent 传多个参数时，请使用 `::`。

## 三种运行模式

| 模式 | 命令 | 会话持久化 | 说明 |
|---|---|:---:|---|
| 独立模式 | `orca-tui run -- claude` | 否 | 当前进程直接创建和管理 PTY |
| 内置 daemon | `orca-tui daemon -- claude` + `orca-tui attach` | 是 | daemon 持有 PTY，可多客户端连接 |
| Orca GUI daemon | `orca-tui run --daemon -- claude` | 是 | 连接 Orca GUI v36，失败时回退独立模式 |

### 内置 daemon

```bash
# 前台启动 daemon
orca-tui daemon -- claude :: codex

# 在另一个终端连接
orca-tui attach
```

默认 socket 为 `$XDG_RUNTIME_DIR/orcatui.sock`；未设置时为 `/tmp/orcatui.sock`。两端都可以
通过 `--socket PATH` 指定路径。attach 中按 `Ctrl+Q` 只断开客户端，不会终止 daemon 中的 Agent。

### Orca GUI daemon

```bash
orca-tui run --daemon -- claude :: codex
```

客户端会自动查找 Orca daemon v36 的 socket 和 token。找不到 daemon 或握手失败时继续以独立
模式运行；运行中断线会按 `[daemon]` 配置尝试重连。内置 daemon 与 Orca GUI daemon 使用不同
协议，不能混用。详见[技术设计](docs/TECHNICAL-DESIGN.md)。

## 远程与移动状态

```bash
# 通过 SSH 在远端执行 Agent
orca-tui run --remote user@example.com --reconnect -- claude :: codex

# 随 Agent 启动移动端状态服务
orca-tui run --mobile 8080 -- claude

# 单独启动移动端 WebSocket 服务
orca-tui mobile --port 8080
```

`--remote` 支持 `host`、`user@host` 和 `user@host:port`。移动服务启动后会输出地址和一次性
token；当前仓库只提供服务端，不包含移动端页面。

## 键盘操作

普通模式下，除两个全局快捷键外，输入都会发送给当前 Agent。

| 按键 | 作用 |
|---|---|
| `Ctrl+Alt+P` | 进入 Pane 控制模式 |
| `Ctrl+Q` | 退出 TUI；attach 模式下只断开客户端 |
| 鼠标滚轮 | 滚动当前窗格历史输出 |
| 鼠标拖选 | 复制文本到系统剪贴板 |

Pane 模式：

| 按键 | 作用 |
|---|---|
| 方向键或 `h` `j` `k` `l` | 移动焦点 |
| `Tab` / `Shift+Tab` | 切换下一个 / 上一个窗格 |
| `p` | 固定或取消固定当前 Agent |
| `x` | 终止并关闭当前 Agent |
| `z` | 放大或恢复当前窗格 |
| `n` | 新建 Agent |
| `b` | 显示或隐藏侧边栏 |
| `/` | 按名称快速跳转 |
| `a` | Activity 时间线 |
| `d` | Agent 状态看板 |
| `s` | Activity / Tasks / Settings 导航 |
| `?` | 完整帮助 |
| `Esc` | 返回普通模式 |

Tasks 视图需要输入 `owner/name` 格式的 GitHub 仓库。选择 issue 或 PR 后按 `Enter`，使用
`default_agent` 新建任务窗格。Settings 修改在按 `Esc` 退出时保存。

## CLI 参考

```text
orca-tui run [--cwd DIR] [--worktree] [--all-worktrees] [--daemon] [--remote HOST] [--reconnect] [--mobile PORT] -- COMMAND...
orca-tui daemon [--socket PATH] [-- COMMAND...]
orca-tui attach [--socket PATH]
orca-tui orchestrate [--spec TEXT | --issues OWNER/NAME] [--parallel]
orca-tui prs OWNER/NAME
orca-tui issues OWNER/NAME
orca-tui mobile [--port PORT]
```

以当前二进制为准：

```bash
orca-tui --help
orca-tui run --help
```

`orchestrate --spec` 将每个非空行转换为任务，默认顺序执行；`--parallel` 并行派发。
`--issues OWNER/NAME` 从 GitHub 开放 issue 创建任务。

## 配置

配置路径为 `$XDG_CONFIG_HOME/orcatui/config.toml`；未设置时为
`~/.config/orcatui/config.toml`。配置缺失或解析失败时使用内置默认值。

```toml
default_agent = "bash"
# clipboard_command = "wl-copy"

[layout]
sidebar_width = 26
show_status_bar = true

[theme]
background = "#0d1117"
foreground = "#e6edf3"
accent = "#58a6ff"
success = "#3fb950"
warning = "#d29922"
error = "#f85149"
background_panel = "#161b22"
background_element = "#21262d"
border = "#30363d"
border_active = "#58a6ff"
text_muted = "#8b949e"

[daemon]
reconnect_initial_secs = 3
reconnect_max_secs = 30
reconnect_max_attempts = 0 # 0 表示不限次数
rpc_timeout_secs = 10
hello_timeout_secs = 5
```

## 开发与排查

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features
```

终端渲染问题可用 `orca-tui-inject` 确定性复现：

```bash
orca-tui-inject record --for-secs 8 --size 80x24 --out recording.bin -- opencode
orca-tui-inject replay recording.bin --size 80x24 --chunk 256 --render
```

设置 `ORCA_DEBUG_LOG=1` 会把底层数据、resize 以及 worktree/sidebar
拓扑诊断写入 `/tmp/orca-live.log`。worktree 诊断只记录数量和受控状态，
不记录用户路径、命令、提示词、token 或原始 Git 输出；
`ORCA_NO_RESPOND=1` 可临时关闭终端能力查询响应。

### 与 Orca 工作区清单对照

Orca CLI 获取的是 Orca runtime 的持久化工作区目录；不加
repo/limit 时会返回当前可覆盖 host 范围内的全局 catalog：

```bash
orca repo list --json
orca worktree list --json
orca worktree ps --json
orca worktree current --json
```

其中 `worktree list` 返回所有已登记工作区，`worktree ps` 额外汇总
终端存活和 agent 状态；两者都可能包含多个 repo 和 host。`worktree current`
只解析当前目录对应的一行。

默认启动和 `orca-tui run --all-worktrees` 会优先读取
`orca worktree list --json` 的完整 catalog，因此可以同时显示多个 repo、多个 host、
远程和 archived workspace。当前机器上能访问的 local checkout 会启动 pane；远程或不可访问
的 workspace 仍会以只读条目显示，不会把远程路径误当作本地 cwd。Orca CLI 不可用时才回退
到当前 Git 仓库的 `git worktree list`。`--worktree` 仍是本地会话级隔离：只创建本次运行的
`.orca-worktrees/`，退出时清理。`orca-tui attach` 会把 daemon session 与同一份全局
workspace catalog 并列显示，但不会把 daemon session 当成 workspace catalog。开启
`ORCA_DEBUG_LOG=1` 后，可用
`[DEBUG-worktree]` 行确认 catalog、pane 和 sidebar 的数量。

## 文档与许可证

- [技术设计](docs/TECHNICAL-DESIGN.md)：模块职责、运行路径、数据流和协议边界
- [LICENSE-MIT](LICENSE-MIT)
- [LICENSE-APACHE](LICENSE-APACHE)

许可证：MIT OR Apache-2.0。
