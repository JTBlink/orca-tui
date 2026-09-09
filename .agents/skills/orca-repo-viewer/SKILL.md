---
name: orca-repo-viewer
description: 查看或分析同级目录中的 Orca 官方源码；默认使用项目旁的 ../orca，缺失时从 stablyai/orca 克隆，适合定位 Orca daemon、PTY、worktree、CLI 和前端实现。
metadata:
  short-description: 查看 Orca 官方源码
---

# Orca 源码查看

用于需要对照 Orca 官方实现回答问题、核对协议、定位调用链或分析 `orca-tui` 与 Orca
兼容性的问题。这个 skill 只负责准备和读取源码，不代表可以修改 Orca 仓库。

## 源码位置

默认位置是当前项目的同级目录 `../orca`。使用附带脚本准备仓库：

```bash
.agents/skills/orca-repo-viewer/scripts/ensure-orca.sh
```

脚本行为：

- `../orca` 已存在且是 Git 仓库时，只输出路径、当前 revision 和 remote，不覆盖文件。
- 目录不存在时，从 `https://github.com/stablyai/orca.git` 克隆到 `../orca`。
- 目标存在但不是 Git 仓库时停止并报错，绝不删除或覆盖该目录。
- 可通过 `ORCA_REPO_DIR=/path/to/orca` 或 `--path /path/to/orca` 指定位置。

本 skill 以 [ORCA-BASELINE.md](ORCA-BASELINE.md) 记录适配基线。每次核对协议或行为时，
先比较当前仓库的 package 版本和完整 commit hash；如果基线发生变化，更新该文件并说明
变更原因。不要只记录分支名或短 hash，因为它们不足以稳定复现适配环境。

仓库地址、路径和命令输出中不得记录用户目录、访问令牌、SSH 私钥、环境变量值或其他
敏感信息。报告路径时优先使用相对路径（例如 `../orca/src/...`）。

## 查看流程

1. 先运行 `ensure-orca.sh`，确认实际源码路径和 revision。
2. 在 Orca 仓库内用 `rg` 搜索符号、协议字段或文件名；优先读取最小相关范围。
3. 对 daemon / PTY 问题，先定位 `src/main/daemon/`、`src/main/pty/`、
   `src/main/runtime/` 和 `src/shared/`，再追踪调用方与测试。
4. 对 UI / renderer 问题，检查 `src/renderer/`、`src/preload/` 和对应的 `tests/`。
5. 回答时给出相对文件路径和行号；区分源码事实、测试覆盖和推断，不把本地镜像的
   分支名或用户信息当成官方事实。

## 常用命令

```bash
# 建立路径并查看版本
.agents/skills/orca-repo-viewer/scripts/ensure-orca.sh
git -C ../orca log -1 --oneline
node -p "require('../orca/package.json').version"

# 搜索 daemon 协议与 socket
rg -n "daemon|daemon-v|clientId|listSessions|createOrAttach" ../orca/src ../orca/tests

# 搜索 PTY、终端和 worktree
rg -n "node-pty|portable-pty|pty|worktree|worktreePath" ../orca/src ../orca/tests

# 读取小范围上下文
sed -n '1,220p' ../orca/src/main/daemon/<file>.ts
```

需要更新源码时必须由用户明确要求，并先确认目标分支、工作区状态和修改范围。默认只读。
