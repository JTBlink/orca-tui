//! 共用 overlay 几何与边框渲染。
//!
//! 具体 overlay 的内容仍由 `App` 依据 `OverlayModel` 组装；本模块集中处理
//! popup 的尺寸、清屏和主题化边框，避免每个 modal 重复一套布局代码。

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use ratatui::Frame;

use crate::config::ThemeConfig;

/// 在总区域中央生成一个有上下限的 popup 矩形。
#[must_use]
pub(crate) fn centered_rect(total: Rect, width: u16, height: u16) -> Rect {
    let width = total.width.min(width).max(1);
    let height = total.height.min(height).max(1);
    Rect::new(
        total.x + total.width.saturating_sub(width) / 2,
        total.y + total.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

/// 清理 popup 区域并绘制统一的圆角主题边框，返回内容区域。
pub(crate) fn begin_popup(
    frame: &mut Frame<'_>,
    area: Rect,
    title: Line<'static>,
    theme: &ThemeConfig,
) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(theme.panel()))
        .border_style(Style::default().fg(theme.accent()))
        .title(title.style(Style::default().fg(theme.accent())));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_rect_clamps_to_terminal() {
        let rect = centered_rect(Rect::new(2, 3, 10, 6), 40, 20);
        assert_eq!(rect, Rect::new(2, 3, 10, 6));
    }
}
