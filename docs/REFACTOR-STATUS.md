# 结构重构验证记录

本文记录本轮 App 结构重构的边界、验证命令和已知环境差异，便于后续维护者继续拆分。

## 已落地的 seam

- `PaneSlot` 是唯一的 per-pane 状态集合，包含 PTY、启动命令、task、daemon session、重连、pin
  和派生状态。
- `input` 提供 `InputMode`、方向焦点和无副作用的 `InputCommand` reducer；App 负责执行命令。
- `input::InteractionState` 统一持有 mode、modal 光标/文本、zoom、sidebar 与鼠标拖拽状态；
  `App` 不再维护这些 transient state 的平行字段。
- `render_model::RenderModel` 从 slot 派生 sidebar 与状态 tally，并携带一次性的
  `OverlayModel` 快照（Jump、Spawn、Tasks、Settings、Activity、Dashboard 等 modal 的只读视图数据）。
- `overlay` 模块统一 popup 几何、清屏和主题边框；所有 modal 已通过该边界创建内容区域，
  并通过统一的 `render_lines` 委托文本内容绘制。
- `daemon_connection::DaemonConnection` 封装 daemon RPC/stream，并提供专用 writer 线程。
- GitHub Tasks 列表和 daemon 写入不再阻塞 UI loop。
- daemon 动态 `createOrAttach` 通过独立 worker 完成，UI 先展示占位 pane，再以非阻塞轮询
  应用成功快照或错误状态。
- CLI 已将默认命令解析与顶层子命令 dispatch 分离；各子命令的具体启动逻辑仍在逐步下沉到
  独立入口函数。

## 验证命令

在仓库根目录执行：

```text
cargo fmt --check
cargo check --lib
cargo test --lib
cargo clippy --all-targets --all-features
```

当前环境中 `cargo fmt --check`、`cargo check --lib` 和完整测试
`cargo test -- --test-threads=1` 已通过（库 `441 passed, 6 ignored`，其他 targets 与 doctest
也通过）。移动端 pairing token 使用进程内单调计数器避免高速调用碰撞。Clippy 可执行，但仓库还存在若干历史 lint（主要是 unused/dead-code、
signal handler 和跨模块风格建议），后续可单独清理，不应把这些 warning 误认为协议或行为回归。

## 兼容性与迁移风险

- daemon wire protocol 未改变；未知 session 事件继续丢弃，不会回退到 pane 0。
- 稳定 pane ID 与当前 Vec position 分离；关闭 pane 后异步 survivor 更新仍按 ID 路由。
- daemon 输入写入改为排队发送，极端断线时写入可能丢失；连接状态仍由控制 RPC/重连逻辑负责。
- GitHub 查询在后台线程执行，列表在结果到达前显示加载状态；关闭应用时后台查询只丢弃结果，不修改 App。
