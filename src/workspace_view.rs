//! 可滚动的 Orca workspace inventory 视图。
//!
//! 侧边栏是常驻概览；这个 overlay 负责在 workspace 数量超过侧边栏可见
//! 高度，或用户需要查看完整名称时，提供一个键盘可达的完整清单。

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::config::ThemeConfig;

/// 一个已经格式化为用户可读文本的 workspace 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceRow {
    pub(crate) name: String,
    pub(crate) detail: String,
}

/// 在终端中央绘制完整 workspace 清单。
///
/// 清单使用选中项驱动的窗口化：无论条目数量多少，当前选中项都可见，
/// 并在有隐藏条目时显示上下滚动提示。所有条目仍保留在输入模型中，
/// 不会因为视口高度而被静默丢弃。
pub(crate) fn render_workspace_overlay(
    frame: &mut Frame<'_>,
    total: Rect,
    rows: &[WorkspaceRow],
    selected: usize,
    theme: &ThemeConfig,
) {
    let pop = crate::overlay::centered_rect(
        total,
        total.width.saturating_sub(4).max(40),
        total.height.saturating_sub(2).max(8),
    );
    let title = format!(" Workspaces ({}) · ↑↓/j k scroll · Esc close ", rows.len());
    let inner = crate::overlay::begin_popup(frame, pop, Line::from(title), theme);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if rows.is_empty() {
        crate::overlay::render_lines(
            frame,
            inner,
            vec![Line::from("(Orca workspace catalog unavailable)")
                .style(Style::default().fg(theme.muted()))],
            theme,
        );
        return;
    }

    // Reserve one line for the indicator whenever the list overflows. This
    // keeps the indicator visible without hiding the selected row.
    let has_overflow = rows.len() > usize::from(inner.height);
    let indicator_rows = usize::from(has_overflow);
    let visible = usize::from(inner.height)
        .saturating_sub(indicator_rows)
        .max(1);
    let selected = selected.min(rows.len().saturating_sub(1));
    let max_start = rows.len().saturating_sub(visible);
    let start = selected.saturating_sub(visible / 2).min(max_start);

    let mut lines = Vec::with_capacity(visible + indicator_rows);
    for (index, row) in rows.iter().enumerate().skip(start).take(visible) {
        let selected_row = index == selected;
        let prefix = if selected_row { "▶ " } else { "  " };
        let style = if selected_row {
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg())
        };
        let detail = if row.detail.is_empty() {
            String::new()
        } else {
            format!("  {}", row.detail)
        };
        lines.push(
            Line::from(vec![
                Span::styled(format!("{prefix}{}", row.name), style),
                Span::styled(detail, Style::default().fg(theme.muted())),
            ])
            .style(Style::default().bg(theme.panel())),
        );
    }

    if has_overflow {
        let more_above = start > 0;
        let more_below = start + visible < rows.len();
        let indicator = match (more_above, more_below) {
            (true, true) => " ↑↓ more ",
            (true, false) => " ↑ top ",
            (false, true) => " ↓ more ",
            (false, false) => "",
        };
        lines.push(Line::from(indicator).style(Style::default().fg(theme.muted())));
    }

    crate::overlay::render_lines(frame, inner, lines, theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeConfig;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area();
        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|col| buf[(col, row)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn workspace_overlay_keeps_selected_row_visible_when_list_overflows() {
        let rows = (0..20)
            .map(|i| WorkspaceRow {
                name: format!("workspace-{i}"),
                detail: "main".to_owned(),
            })
            .collect::<Vec<_>>();
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|f| render_workspace_overlay(f, f.area(), &rows, 19, &ThemeConfig::default()))
            .expect("draw");
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("workspace-19"));
        assert!(text.contains("↑ top") || text.contains("↑↓ more"));
        assert!(text.contains("Workspaces (20)"));
    }
}
