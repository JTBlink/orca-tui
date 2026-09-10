# 仓库贡献指南

## 项目结构

这是一个 Rust 项目，包含一个库 crate 和两个二进制程序：

- `src/core/` 放置 Agent、活动记录与任务协调等领域状态；`src/app/` 放置应用状态机、CLI、事件总线
  和调度逻辑。
- `src/ui/` 放置 TUI 渲染；`src/terminal/` 放置 PTY、终端模拟和字节流协议。
- `src/orca/` 放置 Orca daemon 与 workspace catalog 适配；`src/adapters/` 放置系统和外部集成；
  `src/support/` 放置配置、诊断和性能探针。
- `src/main.rs` 是 `orca-tui` 入口；`src/bin/inject.rs` 构建 `orca-tui-inject` 排查工具。
- 单元测试与实现代码放在同一文件中，位于 `src/**/*.rs` 的 `#[cfg(test)]` 模块内。
- `docs/TECHNICAL-DESIGN.md` 记录架构和协议边界。
- `.agents/skills/` 存放仓库级 contributor skill；其中不得包含凭据、个人路径或其他敏感信息。

## 构建、测试与开发命令

以下命令均在仓库根目录执行：

```bash
cargo build                         # 构建 debug 版本
cargo run -- run -- bash            # 使用 bash 启动 TUI
cargo test                          # 运行单元测试和集成测试
cargo fmt --check                   # 检查 rustfmt 格式
cargo clippy --all-targets --all-features
cargo bench --bench orca            # 运行 Criterion 基准测试
```

使用 `cargo fmt` 应用格式化。排查终端渲染问题时，使用
`cargo run --bin orca-tui-inject -- record ...` 和 `replay ...` 确定性复现 PTY 输出。

## 编码风格与命名

遵循 Rust 2021 标准风格，使用四个空格缩进。保持模块职责单一；相比松散 JSON 或并行的基础
类型集合，优先使用有类型的 struct/enum。函数和变量使用 `snake_case`，类型使用 `CamelCase`，
协议字段使用能表达含义的名称。公共 API 添加简洁的 rustdoc。不得记录 token、凭据、绝对用户
路径或原始敏感数据。

## 测试要求

单元测试应与被测代码放在同一文件中。测试名称使用可观察行为命名，例如
`split_agents_with_separator_is_segments`。修改协议帧、PTY 生命周期、重连/错误路径或 UI 状态
转换时，应补充对应覆盖。提交前先运行聚焦测试，再运行完整的 `cargo test`。

## Commit 与 Pull Request

Commit 标题应简短、使用祈使语气，并带有 `feat:`、`fix:` 或 `docs:` 等常见前缀（现有历史使用
`feat: ...`）。每个 commit 保持单一目的。Pull Request 应说明行为变化、列出执行过的验证命令、
关联 issue 或设计文档；如果修改 TUI 渲染或快捷键，应附终端截图或短录屏。协议兼容性和迁移
风险必须明确说明。

## 安全与配置

绝不要提交 daemon token、GitHub 凭据、SSH 私钥、包含隐私数据的录制文件或机器专用配置。提交
前检查 `git diff` 和 `git status`，并将生成的录制文件与本地配置排除在仓库之外。

## Agent skills

### Issue tracker

本仓库的 issue 和 spec 使用 GitHub Issues 管理，统一使用 `gh` CLI。详见 `docs/agents/issue-tracker.md`。

### Triage labels

使用默认标签：`needs-triage`、`needs-info`、`ready-for-agent`、`ready-for-human`、`wontfix`。详见 `docs/agents/triage-labels.md`。

### Domain docs

这是 single-context 仓库。相关工作开始前读取根目录 `CONTEXT.md`（若存在）和 `docs/adr/` 下相关 ADR。详见 `docs/agents/domain.md`。
