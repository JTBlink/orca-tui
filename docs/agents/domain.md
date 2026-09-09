# Domain 文档

这是一个 single-context 仓库。

## 探索代码前

- 如果根目录存在且与当前工作相关，读取 `CONTEXT.md`。
- 读取 `docs/adr/` 下涉及当前修改范围的 ADR。
- 如果这些文件不存在，直接继续，不要为了满足这条规则而提前创建。

## 术语

领域概念使用 `CONTEXT.md` 中的术语表。不要默默用同义词替换已定义的术语。
如果某个概念还没有确定术语，记录这个缺口并使用 `/domain-modeling` 解决。

## 文件布局

```text
/
├── CONTEXT.md
├── docs/adr/
└── src/
```
