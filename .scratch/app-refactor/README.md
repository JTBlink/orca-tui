# App structure refactor

This plan keeps the current behavior and ratatui-facing APIs stable while
reducing the amount of policy hidden inside `App`.

## Dependency graph

```text
R0 baseline/invariants
 ├── R1 PaneSlot aggregate
 │    ├── R3 DaemonConnection adapter
 │    ├── R4 Input state + handlers
 │    └── R5 RenderModel + overlays
 ├── R2 daemon unknown-session routing
 └── R6 CLI dispatch split

R3 + R4 + R5 + R6 ──> R7 external I/O off the UI thread (optional)
all tickets ──> R8 docs, full verification, cleanup
```

Tickets are intentionally ordered to avoid several agents editing the same
large `app.rs` region at once. R0 is a short read-only baseline; R1 is the
first structural seam. R2 and R6 can proceed independently of R1.

## Published GitHub issues

The approved breakdown was published as GitHub Issues in this repository. The
local draft names are mapped to the published issue numbers below:

| Local draft | GitHub issue | Title |
|---|---:|---|
| R0 / routing correctness | #1 | 保护 pane 路由和缩放鼠标行为 |
| R1 / PaneSlot seam | #2 | 引入 PaneSlot per-pane seam |
| R1 / lifecycle migration | #3 | 迁移 pane 生命周期到 PaneSlot |
| R1 / contract cleanup | #4 | 删除 parallel vectors，收敛 pane 状态模型 |
| R2 / input | #5 | 拆出输入状态与交互处理 |
| R3 / rendering | #6 | 拆出 RenderModel 与 overlay 渲染 |
| R4 / daemon | #7 | 抽取 daemon connection adapter |
| R5 / background I/O | #8 | 将阻塞外部 I/O 移出 UI loop |
| R6 / CLI | #10 | 拆分 CLI dispatch 与启动流程 |
| R8 / closeout | #9 | 完成结构重构文档与验证收尾 |

The GitHub issues are the source of truth for assignment and status. The
markdown files in this directory are the original local drafts and preserve
the design rationale used when publishing the issues.

## Non-goals

- No rewrite of `Pane`, `TerminalEmulator`, OSC/sync/query scanners, or the
  coordinator.
- No new async runtime in the first structural pass.
- No protocol change to the Orca daemon beyond dropping unknown session events.
- Preserve `App::spawn_agents`, `App::run`, and existing keyboard behavior.
