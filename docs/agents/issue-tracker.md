# Issue tracker：GitHub

本仓库的 issue 和 spec 使用 GitHub Issues 管理。所有操作使用 `gh` CLI；
在本仓库内执行命令时，仓库会根据 GitHub remote 自动推断。

## 约定

- 创建 issue：`gh issue create --title "..." --body "..."`
- 查看 issue：`gh issue view <number> --comments`
- 列出 issue：`gh issue list --state open`
- 评论 issue：`gh issue comment <number> --body "..."`
- 添加或移除标签：`gh issue edit <number> --add-label "..."` 或
  `gh issue edit <number> --remove-label "..."`
- 关闭 issue：`gh issue close <number> --comment "..."`

## Pull Request 是否作为 triage 来源

默认不把 PR 作为本仓库的 triage 请求来源。

## 与 skills 集成

当 skill 要求发布到 issue tracker 时，创建 GitHub issue；
当 skill 要求读取相关 ticket 时，使用 `gh issue view <number> --comments`。

多 ticket 工作优先使用 GitHub 原生 issue dependency；
如果不可用，在子 issue 正文顶部记录 `Blocked by: #<number>`。
