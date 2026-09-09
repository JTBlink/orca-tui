# Orca 适配基线

该文件记录 `orca-tui` 对照 Orca 源码时使用的可复现基线。协议、daemon、PTY 或目录布局
发生变化时，先确认新的版本与完整 commit hash，再更新这里的记录。

| 字段 | 当前值 |
|---|---|
| 官方仓库 | `https://github.com/stablyai/orca.git` |
| Orca package version | `1.4.197` |
| Git commit | `b38c1313c4350a56bec53a45add2a3510cc66ef7` |
| 适配用途 | Orca daemon / PTY / worktree / CLI 源码对照 |

## 更新记录方法

在项目根目录运行：

```bash
.agents/skills/orca-repo-viewer/scripts/ensure-orca.sh
git -C ../orca rev-parse HEAD
node -p "require('../orca/package.json').version"
```

将输出的完整 hash 和 package version 更新到上表。只记录公开仓库地址、版本和 hash；不要
把本机绝对路径、用户目录、token、SSH 地址、凭据或环境变量值写入此文件。
