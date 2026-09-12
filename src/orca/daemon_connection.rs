//! App-facing daemon connection adapter.
//!
//! `DaemonClient` owns wire framing; this adapter limits the surface exposed to
//! `App` to connection lifecycle and RPC operations, keeping protocol details
//! out of input/render code.

use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Sender};
use std::thread;

use crate::orca_daemon::{
    DaemonClient, DaemonConnectOptions, DaemonEndpoint, DaemonError, DaemonIdentity, Frame,
};

/// 供 App 使用的 daemon 连接边界。
enum DaemonCommand {
    Write {
        session_id: String,
        data: Vec<u8>,
    },
    Resize {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    Kill {
        session_id: String,
    },
}

pub(crate) struct DaemonConnection {
    client: DaemonClient,
    options: DaemonConnectOptions,
    command_tx: Option<Sender<DaemonCommand>>,
}

impl DaemonConnection {
    /// 使用给定超时选项尝试建立连接。
    pub(crate) fn try_connect(options: DaemonConnectOptions) -> Option<Result<Self, DaemonError>> {
        let saved_options = options.clone();
        DaemonClient::try_connect_with(options).map(|result| {
            result.map(|client| Self {
                client,
                options: saved_options,
                command_tx: None,
            })
        })
    }

    /// 连接对应的 daemon 身份。
    pub(crate) fn identity(&self) -> &DaemonIdentity {
        self.client.identity()
    }

    /// Query every session currently owned by the Orca daemon.
    pub(crate) fn list_sessions(
        &mut self,
    ) -> Result<Vec<crate::orca_daemon::DaemonSessionInfo>, DaemonError> {
        let payload = self.client.list_sessions()?;
        let sessions = payload
            .get("sessions")
            .cloned()
            .ok_or_else(|| DaemonError::Protocol("listSessions missing sessions".into()))?;
        serde_json::from_value(sessions)
            .map_err(|err| DaemonError::Protocol(format!("invalid listSessions payload: {err}")))
    }

    /// Read an existing session without replacing Orca GUI's attachment.
    pub(crate) fn snapshot_session(
        &mut self,
        session_id: &str,
    ) -> Result<serde_json::Value, DaemonError> {
        self.client.snapshot_session(session_id)
    }

    /// Read the daemon's best-effort foreground process name. This is used
    /// only as a fallback label for legacy sessions that do not carry Orca's
    /// structured `agentSessionOwners` metadata.
    pub(crate) fn foreground_process(
        &mut self,
        session_id: &str,
    ) -> Result<Option<String>, DaemonError> {
        let payload = self.client.rpc(
            "getForegroundProcess",
            serde_json::json!({ "sessionId": session_id }),
        )?;
        Ok(payload
            .get("foregroundProcess")
            .and_then(|value| value.as_str())
            .map(str::to_owned))
    }

    /// 获取控制端点（调试和诊断使用）。
    #[allow(dead_code)]
    pub(crate) fn endpoint(&self) -> &DaemonEndpoint {
        self.client.endpoint()
    }

    /// 将输入排入专用 writer 线程，避免 UI loop 等待 daemon RPC 响应。
    pub(crate) fn enqueue_write(&mut self, session_id: String, data: Vec<u8>) {
        self.ensure_command_tx();
        if let Some(tx) = &self.command_tx {
            let _ = tx.send(DaemonCommand::Write { session_id, data });
        }
    }

    /// 将 resize 排入专用 daemon RPC 线程，避免 render loop 阻塞。
    pub(crate) fn enqueue_resize(&mut self, session_id: String, cols: u16, rows: u16) {
        self.ensure_command_tx();
        if let Some(tx) = &self.command_tx {
            let _ = tx.send(DaemonCommand::Resize {
                session_id,
                cols,
                rows,
            });
        }
    }

    fn ensure_command_tx(&mut self) {
        if self.command_tx.is_none() {
            let (tx, rx) = mpsc::channel::<DaemonCommand>();
            let endpoint = self.client.endpoint().clone();
            let options = self.options.clone();
            thread::Builder::new()
                .name("orca-daemon-rpc-writer".into())
                .spawn(move || {
                    let Ok(mut writer) = DaemonClient::connect_with(endpoint, options) else {
                        return;
                    };
                    while let Ok(command) = rx.recv() {
                        match command {
                            DaemonCommand::Write { session_id, data } => {
                                let _ = writer.rpc(
                                    "write",
                                    serde_json::json!({
                                        "sessionId": session_id,
                                        "data": String::from_utf8_lossy(&data),
                                    }),
                                );
                            }
                            DaemonCommand::Resize {
                                session_id,
                                cols,
                                rows,
                            } => {
                                let _ = writer.rpc(
                                    "resize",
                                    serde_json::json!({
                                        "sessionId": session_id,
                                        "cols": cols,
                                        "rows": rows,
                                    }),
                                );
                            }
                            DaemonCommand::Kill { session_id } => {
                                let _ = writer.rpc(
                                    "kill",
                                    serde_json::json!({
                                        "sessionId": session_id,
                                    }),
                                );
                            }
                        }
                    }
                })
                .ok();
            self.command_tx = Some(tx);
        }
    }

    /// Explicitly terminate a daemon-owned session. This is used only by the
    /// pane close action (`x`); disconnecting the TUI or clicking the global
    /// exit control never kills Orca sessions.
    pub(crate) fn enqueue_kill(&mut self, session_id: String) {
        self.ensure_command_tx();
        if let Some(tx) = &self.command_tx {
            let _ = tx.send(DaemonCommand::Kill { session_id });
        }
    }

    /// Start a daemon session without waiting for the control RPC on the UI
    /// thread. The worker owns a short-lived control connection, so the main
    /// connection remains available for stream/reconnect traffic.
    pub(crate) fn spawn_session_async(
        &self,
        params: serde_json::Value,
    ) -> mpsc::Receiver<Result<serde_json::Value, DaemonError>> {
        let endpoint = self.client.endpoint().clone();
        let options = self.options.clone();
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("orca-daemon-create-session".into())
            .spawn(move || {
                let result = DaemonClient::connect_with(endpoint, options)
                    .and_then(|mut client| client.rpc("createOrAttach", params));
                let _ = tx.send(result);
            })
            .ok();
        rx
    }

    /// 取出 stream socket，交给后台 reader 线程。
    pub(crate) fn take_stream(&mut self) -> Option<UnixStream> {
        self.client.take_stream()
    }

    /// 读取 stream 帧；协议解析仍集中在 `orca_daemon`。
    pub(crate) fn read_stream_frame(stream: &mut UnixStream) -> Result<Frame, DaemonError> {
        DaemonClient::read_stream_frame(stream)
    }
}
