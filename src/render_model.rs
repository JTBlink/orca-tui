//! 纯渲染模型。
//!
//! `RenderModel` 从运行时 pane slot 派生 sidebar 和状态汇总数据；它不持有
//! PTY、daemon 或终端句柄，使渲染准备步骤可以独立测试。

use crate::agent::{status_tally, AgentStatus};
use crate::input::InputMode;
use crate::pane_slot::PaneSlot;
use crate::sidebar::SidebarEntry;
use crate::workspace_view::WorkspaceRow;
use ratatui::layout::Rect;

/// 一帧 TUI 所需的只读派生数据。
#[derive(Debug, Clone)]
pub(crate) struct RenderModel {
    pub(crate) sidebar_entries: Vec<SidebarEntry>,
    pub(crate) tally: crate::agent::StatusTally,
    pub(crate) pane_rects: Vec<Rect>,
    pub(crate) footer_hint: String,
    pub(crate) overlay: OverlayModel,
}

/// Owned, read-only view data for modal overlays. Keeping this alongside the
/// pane/sidebar model means the draw closure consumes one immutable snapshot
/// instead of borrowing transient fields from `App` piecemeal.
#[derive(Debug, Clone, Default)]
pub(crate) struct OverlayModel {
    pub(crate) mode: InputMode,
    pub(crate) jump_query: String,
    pub(crate) jump_filtered: Vec<(usize, String)>,
    pub(crate) jump_selected: usize,
    pub(crate) spawn_options: Vec<(String, String)>,
    pub(crate) spawn_selected: usize,
    pub(crate) activity_lines: Vec<String>,
    pub(crate) sidebar_selected: usize,
    pub(crate) tasks_repo_input: String,
    pub(crate) tasks_items: Vec<(String, String, String)>,
    pub(crate) tasks_selected: usize,
    pub(crate) tasks_error: Option<String>,
    pub(crate) settings_cursor: usize,
    pub(crate) settings_sidebar_on: bool,
    pub(crate) settings_status_bar: bool,
    pub(crate) settings_default_agent: String,
    pub(crate) settings_theme_name: String,
    pub(crate) dashboard_entries: Vec<(String, AgentStatus)>,
    pub(crate) workspace_rows: Vec<WorkspaceRow>,
    pub(crate) workspace_selected: usize,
}

impl RenderModel {
    /// 从当前 slot 集合生成渲染模型。
    #[must_use]
    pub(crate) fn from_slots(slots: &[PaneSlot], focus: usize) -> Self {
        Self::from_slots_with_catalog(slots, focus, &[])
    }

    /// 从运行中 pane 和未被 pane 承载的 Orca catalog 行生成侧边栏模型。
    #[must_use]
    pub(crate) fn from_slots_with_catalog(
        slots: &[PaneSlot],
        focus: usize,
        catalog_entries: &[SidebarEntry],
    ) -> Self {
        let mut sidebar_entries = slots
            .iter()
            .enumerate()
            .map(|(i, slot)| SidebarEntry {
                name: slot.name().to_string(),
                state: slot.state().clone(),
                branch: slot.branch().and_then(|branch| {
                    let redundant = slot.workspace_id.is_some()
                        && slot
                            .name()
                            .strip_suffix(branch)
                            .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('/'));
                    (!redundant).then(|| branch.to_owned())
                }),
                activity: slot.activity().cloned(),
                focused: i == focus,
                pinned: slot.pinned,
            })
            .collect::<Vec<_>>();
        sidebar_entries.extend(catalog_entries.iter().cloned());
        let statuses = sidebar_entries
            .iter()
            .map(|entry| {
                AgentStatus::derive(
                    &entry.state,
                    entry.activity.as_ref().map(|a| a.state.as_str()),
                )
            })
            .collect::<Vec<_>>();
        let tally = status_tally(&statuses);
        Self {
            sidebar_entries,
            tally,
            pane_rects: Vec::new(),
            footer_hint: String::new(),
            overlay: OverlayModel::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::Pane;

    #[test]
    fn model_derives_focus_pin_and_status_from_slots() {
        let mut first = PaneSlot::new(Pane::new(1, "first", 40, 8), vec!["echo".into()]);
        first.pinned = true;
        let second = PaneSlot::new(Pane::new(2, "second", 40, 8), vec!["true".into()]);
        let model = RenderModel::from_slots(&[first, second], 1);
        assert!(model.sidebar_entries[0].pinned);
        assert!(model.sidebar_entries[1].focused);
        assert_eq!(model.sidebar_entries.len(), 2);
        assert_eq!(model.tally.total(), 2);
    }
}
