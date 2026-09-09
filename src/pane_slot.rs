//! Per-pane state aggregate.
//!
//! `PaneSlot` is the seam for state that belongs to one agent pane. The app
//! may use a vector position for layout and focus, but lifecycle metadata is
//! kept together here so future migrations cannot introduce another parallel
//! vector by accident.

use std::time::Instant;

use crate::agent::AgentStatus;
use crate::coordinator::TaskId;
use crate::pane::Pane;
use crate::pty_session::PtySession;
use crate::ssh::ReconnectSession;

/// All state that belongs to one pane.
pub(crate) struct PaneSlot {
    pub(crate) pane: Pane,
    pub(crate) session: Option<PtySession>,
    pub(crate) command: Vec<String>,
    pub(crate) task: Option<TaskId>,
    pub(crate) daemon_session_id: Option<String>,
    pub(crate) reconnect: Option<ReconnectSession>,
    pub(crate) reconnect_due: Option<Instant>,
    pub(crate) pinned: bool,
    pub(crate) last_status: Option<AgentStatus>,
}

impl std::ops::Deref for PaneSlot {
    type Target = Pane;

    fn deref(&self) -> &Self::Target {
        &self.pane
    }
}

impl std::ops::DerefMut for PaneSlot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.pane
    }
}

impl PaneSlot {
    /// 创建一个拥有独立生命周期状态的 pane 槽位。
    #[must_use]
    pub(crate) fn new(pane: Pane, command: Vec<String>) -> Self {
        Self {
            pane,
            session: None,
            command,
            task: None,
            daemon_session_id: None,
            reconnect: None,
            reconnect_due: None,
            pinned: false,
            last_status: None,
        }
    }
}

impl std::fmt::Debug for PaneSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneSlot")
            .field("pane", &self.pane)
            .field("has_session", &self.session.is_some())
            .field("command_len", &self.command.len())
            .field("has_task", &self.task.is_some())
            .field("has_daemon_session", &self.daemon_session_id.is_some())
            .field("reconnect", &self.reconnect.is_some())
            .field("reconnect_due", &self.reconnect_due)
            .field("pinned", &self.pinned)
            .field("last_status", &self.last_status)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentState;

    #[test]
    fn new_slot_owns_per_pane_defaults() {
        let pane = Pane::new(7, "codex", 40, 8);
        let slot = PaneSlot::new(pane, vec!["codex".into()]);

        assert_eq!(slot.pane.id(), 7);
        assert_eq!(slot.command, vec!["codex"]);
        assert!(slot.session.is_none());
        assert!(slot.task.is_none());
        assert!(slot.daemon_session_id.is_none());
        assert!(slot.reconnect.is_none());
        assert!(slot.reconnect_due.is_none());
        assert!(!slot.pinned);
        assert!(slot.last_status.is_none());
        assert!(matches!(slot.pane.state(), AgentState::Idle));
    }
}
