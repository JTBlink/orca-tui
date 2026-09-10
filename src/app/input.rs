//! 输入状态与语义命令。
//!
//! 该模块只负责把按键解释成交互意图，不直接访问 PTY、daemon 或 GitHub。
//! `App` 负责执行命令，因此输入策略可以在没有终端和外部 I/O 的情况下测试。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// TUI 当前交互模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum InputMode {
    #[default]
    Normal,
    Pane,
    Jump,
    Spawn,
    Activity,
    Dashboard,
    Sidebar,
    SpawnCustom,
    TasksRepo,
    TasksList,
    Settings,
    Workspaces,
}

/// 键盘、鼠标和 modal 共享的瞬态 UI 状态集合。
#[derive(Debug)]
pub struct InteractionState {
    pub(crate) mode: InputMode,
    pub(crate) sidebar_hidden: bool,
    pub(crate) jump_query: String,
    pub(crate) jump_selected: usize,
    pub(crate) spawn_selected: usize,
    pub(crate) custom_cmd: String,
    pub(crate) zoomed: bool,
    pub(crate) show_help: bool,
    pub(crate) sidebar_nav: usize,
    pub(crate) drag_origin: Option<(usize, u16, u16)>,
    pub(crate) tasks_repo_input: String,
    pub(crate) tasks_selected: usize,
    pub(crate) tasks_error: Option<String>,
    pub(crate) settings_cursor: usize,
    pub(crate) workspace_selected: usize,
}

impl Default for InteractionState {
    fn default() -> Self {
        Self {
            mode: InputMode::Normal,
            sidebar_hidden: false,
            jump_query: String::new(),
            jump_selected: 0,
            spawn_selected: 0,
            custom_cmd: String::new(),
            zoomed: false,
            show_help: false,
            sidebar_nav: 0,
            drag_origin: None,
            tasks_repo_input: String::new(),
            tasks_selected: 0,
            tasks_error: None,
            settings_cursor: 0,
            workspace_selected: 0,
        }
    }
}

/// Pane 网格中的方向性焦点移动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusDirection {
    Up,
    Down,
    Left,
    Right,
}

/// 输入 reducer 产生的语义命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputCommand {
    Noop,
    Quit,
    EnterPaneMode,
    SetNormalMode,
    Forward(KeyEvent),
    Focus(FocusDirection),
    TogglePin,
    ClosePane,
    ToggleZoom,
    ToggleHelp,
    OpenJump,
    OpenActivity,
    OpenDashboard,
    OpenSpawn,
    ToggleSidebar,
    OpenSidebar,
    OpenWorkspaces,
}

/// 将全局、Normal 和 Pane 模式的按键转换为语义命令。
///
/// Jump/Tasks/Settings 等 modal 的文本编辑规则仍由各自 handler 处理，
/// 但它们不会在这里执行任何副作用或阻塞 I/O。
#[must_use]
pub(crate) fn reduce_key(mode: InputMode, key: KeyEvent) -> Option<InputCommand> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('q') {
        return Some(InputCommand::Quit);
    }
    if ctrl && key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('p') {
        return Some(InputCommand::EnterPaneMode);
    }

    match mode {
        InputMode::Normal => Some(InputCommand::Forward(key)),
        InputMode::Pane => {
            let command = match key.code {
                KeyCode::Esc => InputCommand::SetNormalMode,
                KeyCode::Tab => InputCommand::Focus(FocusDirection::Right),
                KeyCode::BackTab => InputCommand::Focus(FocusDirection::Left),
                KeyCode::Up | KeyCode::Char('k') => InputCommand::Focus(FocusDirection::Up),
                KeyCode::Down | KeyCode::Char('j') => InputCommand::Focus(FocusDirection::Down),
                KeyCode::Left | KeyCode::Char('h') => InputCommand::Focus(FocusDirection::Left),
                KeyCode::Right | KeyCode::Char('l') => InputCommand::Focus(FocusDirection::Right),
                KeyCode::Char('p') => InputCommand::TogglePin,
                KeyCode::Char('x') => InputCommand::ClosePane,
                KeyCode::Char('z') => InputCommand::ToggleZoom,
                KeyCode::Char('?') => InputCommand::ToggleHelp,
                KeyCode::Char('/') => InputCommand::OpenJump,
                KeyCode::Char('a') => InputCommand::OpenActivity,
                KeyCode::Char('d') => InputCommand::OpenDashboard,
                KeyCode::Char('n') => InputCommand::OpenSpawn,
                KeyCode::Char('b') => InputCommand::ToggleSidebar,
                KeyCode::Char('s') => InputCommand::OpenSidebar,
                KeyCode::Char('w') => InputCommand::OpenWorkspaces,
                _ => InputCommand::Noop,
            };
            Some(command)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn reducer_keeps_global_quit_and_gateway_semantic() {
        assert_eq!(
            reduce_key(
                InputMode::Normal,
                key(KeyCode::Char('q'), KeyModifiers::CONTROL)
            ),
            Some(InputCommand::Quit)
        );
        assert_eq!(
            reduce_key(
                InputMode::Normal,
                key(
                    KeyCode::Char('p'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT
                )
            ),
            Some(InputCommand::EnterPaneMode)
        );
    }

    #[test]
    fn reducer_maps_pane_actions_without_side_effects() {
        assert_eq!(
            reduce_key(InputMode::Pane, key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(InputCommand::ClosePane)
        );
        assert_eq!(
            reduce_key(InputMode::Pane, key(KeyCode::Char('z'), KeyModifiers::NONE)),
            Some(InputCommand::ToggleZoom)
        );
    }

    #[test]
    fn reducer_opens_workspace_inventory_from_pane_mode() {
        assert_eq!(
            reduce_key(InputMode::Pane, key(KeyCode::Char('w'), KeyModifiers::NONE)),
            Some(InputCommand::OpenWorkspaces)
        );
    }
}
