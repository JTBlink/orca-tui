//! Terminal tab strip used by the main TUI.
//!
//! A tab is a view selector while it is active; switching tabs leaves the
//! underlying `PaneSlot` (and its PTY session) alive. An explicit close uses
//! the owning runtime's lifecycle path, mirroring Orca/Herdr's workspace →
//! tabs model without creating split terminal surfaces in the current window.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::Frame;

use crate::config::ThemeConfig;

/// A tab label and optional status marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabItem {
    pub(crate) label: String,
    pub(crate) status: Option<char>,
}

/// Hit boxes produced by [`layout_tabs`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TabHitboxes {
    pub(crate) tabs: Vec<Rect>,
    /// Optional close affordance inside each tab. Very narrow tabs omit the
    /// icon so the strip can still represent every open session.
    pub(crate) closes: Vec<Option<Rect>>,
    pub(crate) add: Option<Rect>,
    /// Visible exit control in the tab chrome. Clicking it is equivalent to
    /// the global Ctrl+Q shortcut and never touches the active PTY.
    pub(crate) quit: Option<Rect>,
}

/// Lay out tabs left-to-right and reserve a compact `+` button at the end.
/// Every returned rect is inside `area`; labels are clipped by the renderer
/// when the terminal is narrower than the complete tab strip.
pub(crate) fn layout_tabs(area: Rect, items: &[TabItem]) -> TabHitboxes {
    if area.width == 0 || area.height == 0 {
        return TabHitboxes::default();
    }
    let mut x = area.x;
    let right = area.right();
    // Keep every session represented, even when many windows are open. For a
    // crowded strip labels are clipped, but each tab still gets a hitbox and
    // remains mouse-selectable instead of silently disappearing off-screen.
    // Keep an explicit exit affordance in the chrome. It is compact on narrow
    // terminals (`×`) and expands to a localized label when there is room.
    let quit_width = if area.width >= 10 {
        8
    } else if area.width >= 3 {
        3
    } else {
        0
    };
    let content_right = right.saturating_sub(quit_width);
    let add_width =
        if items.len().saturating_add(1) * 6 <= usize::from(content_right.saturating_sub(x)) {
            4u16
        } else {
            0
        };
    let tabs_right = content_right.saturating_sub(add_width);
    let mut tabs = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        if x >= tabs_right {
            break;
        }
        let remaining = (items.len() - idx) as u16;
        let available = tabs_right.saturating_sub(x);
        let ideal = available / remaining.max(1);
        let status_width = usize::from(item.status.is_some());
        let label_width = item.label.chars().count().saturating_add(status_width + 4) as u16;
        let width = ideal.max(1).min(label_width.max(1));
        tabs.push(Rect::new(x, area.y, width, area.height));
        x = x.saturating_add(width);
    }
    let closes = tabs
        .iter()
        .map(|rect| {
            (rect.width >= 5)
                .then(|| Rect::new(rect.right().saturating_sub(2), rect.y, 2, rect.height))
        })
        .collect();
    let add = (add_width > 0 && x < content_right).then(|| {
        Rect::new(
            x,
            area.y,
            content_right.saturating_sub(x).min(add_width),
            area.height,
        )
    });
    let quit = (quit_width > 0).then(|| Rect::new(content_right, area.y, quit_width, area.height));
    TabHitboxes {
        tabs,
        closes,
        add,
        quit,
    }
}

/// Render a single-row tab strip and return its hit boxes for mouse input.
pub(crate) fn render_tab_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    items: &[TabItem],
    active: usize,
    theme: &ThemeConfig,
) -> TabHitboxes {
    let hitboxes = layout_tabs(area, items);
    if area.width == 0 || area.height == 0 {
        return hitboxes;
    }
    let buf = frame.buffer_mut();
    buf.set_style(area, Style::default().bg(theme.panel()));
    for (idx, (item, rect)) in items.iter().zip(hitboxes.tabs.iter()).enumerate() {
        let selected = idx == active;
        let bg = if selected {
            theme.element()
        } else {
            theme.panel()
        };
        let fg = if selected { theme.accent() } else { theme.fg() };
        buf.set_style(*rect, Style::default().bg(bg));
        let close = hitboxes.closes.get(idx).and_then(Option::as_ref);
        let label_rect = close.map_or(*rect, |close| {
            Rect::new(
                rect.x,
                rect.y,
                rect.width.saturating_sub(close.width),
                rect.height,
            )
        });
        // In a crowded strip every session still gets a hitbox. Once a tab is
        // three cells wide there is no room for a readable label, so show its
        // stable 1-based ordinal instead of rendering blank padding; the full
        // agent name remains visible in the sidebar and active terminal title.
        let text = if label_rect.width <= 3 {
            format!("{:>width$}", idx + 1, width = usize::from(label_rect.width))
        } else {
            let marker = item.status.map(|s| format!("{s} ")).unwrap_or_default();
            format!("  {}{}  ", marker, item.label)
        };
        let style = Style::default().fg(fg).bg(bg).add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
        let _ = buf.set_stringn(
            label_rect.x,
            label_rect.y,
            &text,
            usize::from(label_rect.width),
            style,
        );
        if let Some(close) = close {
            let close_style = Style::default()
                .fg(if selected {
                    theme.accent()
                } else {
                    theme.muted()
                })
                .bg(bg);
            let _ = buf.set_stringn(
                close.x,
                close.y,
                " ×",
                usize::from(close.width),
                close_style,
            );
        }
    }
    if let Some(add) = hitboxes.add {
        buf.set_style(add, Style::default().bg(theme.panel()));
        let _ = buf.set_stringn(
            add.x,
            add.y,
            "  +  ",
            usize::from(add.width),
            Style::default().fg(theme.accent()).bg(theme.panel()),
        );
    }
    if let Some(quit) = hitboxes.quit {
        buf.set_style(quit, Style::default().bg(theme.panel()));
        let label = if quit.width >= 8 {
            "  × 退出  "
        } else {
            " × "
        };
        let _ = buf.set_stringn(
            quit.x,
            quit.y,
            label,
            usize::from(quit.width),
            Style::default().fg(theme.error()).bg(theme.panel()),
        );
    }
    hitboxes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_layout_reserves_add_button() {
        let items = vec![
            TabItem {
                label: "one".into(),
                status: None,
            },
            TabItem {
                label: "two".into(),
                status: None,
            },
        ];
        let hit = layout_tabs(Rect::new(0, 0, 30, 1), &items);
        assert_eq!(hit.tabs.len(), 2);
        assert!(hit.add.is_some());
        assert!(hit.quit.is_some());
        assert_eq!(hit.closes.len(), hit.tabs.len());
        assert!(hit.closes.iter().all(Option::is_some));
        assert!(hit.tabs.iter().all(|r| r.right() <= 30));
    }

    #[test]
    fn tab_layout_keeps_every_open_session_visible_when_crowded() {
        let items = (0..12)
            .map(|index| TabItem {
                label: format!("session-{index}"),
                status: Some('●'),
            })
            .collect::<Vec<_>>();
        let hit = layout_tabs(Rect::new(0, 0, 32, 1), &items);
        assert_eq!(hit.tabs.len(), items.len());
        assert!(hit.tabs.iter().all(|rect| rect.width > 0));
        assert!(hit.tabs.windows(2).all(|pair| pair[0].right() <= pair[1].x));
        assert!(hit.quit.is_some());
    }

    #[test]
    fn quit_hitbox_is_reserved_and_inside_area() {
        let hit = layout_tabs(Rect::new(4, 2, 20, 1), &[]);
        let quit = hit.quit.expect("exit control remains visible");
        assert!(quit.x >= 4 && quit.right() <= 24);
    }

    #[test]
    fn close_hitboxes_are_inside_wide_tabs_and_omitted_for_narrow_tabs() {
        let items = vec![
            TabItem {
                label: "alpha".into(),
                status: None,
            },
            TabItem {
                label: "beta".into(),
                status: None,
            },
        ];
        let wide = layout_tabs(Rect::new(0, 0, 30, 1), &items);
        assert_eq!(wide.closes.len(), wide.tabs.len());
        for (tab, close) in wide.tabs.iter().zip(wide.closes.iter()) {
            let close = close.expect("wide tab exposes a close affordance");
            assert!(close.x >= tab.x);
            assert!(close.right() <= tab.right());
            assert!(close.y >= tab.y && close.bottom() <= tab.bottom());
        }

        let crowded = layout_tabs(Rect::new(0, 0, 12, 1), &items);
        assert_eq!(crowded.closes.len(), crowded.tabs.len());
        assert!(crowded
            .tabs
            .iter()
            .zip(crowded.closes.iter())
            .any(|(tab, close)| tab.width < 5 && close.is_none()));
    }
}
