//! Per-pane state aggregate.
//!
//! `PaneSlot` is the seam for state that belongs to one agent pane. The app
//! may use a vector position for layout and focus, but lifecycle metadata is
//! kept together here so future migrations cannot introduce another parallel
//! vector by accident.

use std::path::PathBuf;
use std::time::Instant;

use crate::agent::AgentStatus;
use crate::coordinator::TaskId;
use crate::pane::Pane;
use crate::pty_session::PtySession;
use crate::ssh::ReconnectSession;

/// Explicit lifecycle ownership for a pane.  Existing Orca tabs are views;
/// only tabs created by this TUI may receive input or be closed remotely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionOwner {
    Local,
    OrcaExisting,
    OrcaTuiOwned,
}

/// All state that belongs to one pane.
pub(crate) struct PaneSlot {
    pub(crate) pane: Pane,
    pub(crate) session: Option<PtySession>,
    pub(crate) command: Vec<String>,
    /// Orca catalog identity for the workspace hosting this pane.
    pub(crate) workspace_id: Option<String>,
    /// Existing checkout used when respawning this pane.
    pub(crate) worktree_path: Option<PathBuf>,
    pub(crate) task: Option<TaskId>,
    pub(crate) daemon_session_id: Option<String>,
    /// Runtime-issued Orca terminal handle used by the public CLI bridge.
    pub(crate) orca_handle: Option<String>,
    pub(crate) session_owner: SessionOwner,
    /// Existing Orca sessions are hydrated through the read-only snapshot
    /// RPC. They never receive input or resize requests from the TUI, because
    /// Orca's createOrAttach endpoint replaces the GUI's attachment owner.
    /// Explicit tab/window close only removes the local view. A remote close
    /// is allowed only for `SessionOwner::OrcaTuiOwned` handles.
    pub(crate) daemon_read_only: bool,
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
            workspace_id: None,
            worktree_path: None,
            task: None,
            daemon_session_id: None,
            orca_handle: None,
            session_owner: SessionOwner::Local,
            daemon_read_only: false,
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
            .field("has_orca_handle", &self.orca_handle.is_some())
            .field("session_owner", &self.session_owner)
            .field("daemon_read_only", &self.daemon_read_only)
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
        assert!(slot.orca_handle.is_none());
        assert_eq!(slot.session_owner, SessionOwner::Local);
        assert!(!slot.daemon_read_only);
        assert!(slot.reconnect.is_none());
        assert!(slot.reconnect_due.is_none());
        assert!(!slot.pinned);
        assert!(slot.last_status.is_none());
        assert!(matches!(slot.pane.state(), AgentState::Idle));
    }

    #[test]
    fn slot_deref_preserves_stable_pane_identity() {
        let slot = PaneSlot::new(Pane::new(42, "agent", 40, 8), vec!["agent".into()]);
        assert_eq!(slot.id(), 42);
        assert_eq!(slot.name(), "agent");
    }
}
