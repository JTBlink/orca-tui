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
pub(crate) struct DaemonConnection {
    client: DaemonClient,
    options: DaemonConnectOptions,
    write_tx: Option<Sender<(String, Vec<u8>)>>,
}

impl DaemonConnection {
    /// 使用给定超时选项尝试建立连接。
    pub(crate) fn try_connect(options: DaemonConnectOptions) -> Option<Result<Self, DaemonError>> {
        let saved_options = options.clone();
        DaemonClient::try_connect_with(options).map(|result| {
            result.map(|client| Self {
                client,
                options: saved_options,
                write_tx: None,
            })
        })
    }

    /// 连接对应的 daemon 身份。
    pub(crate) fn identity(&self) -> &DaemonIdentity {
        self.client.identity()
    }

    /// 获取控制端点（调试和诊断使用）。
    #[allow(dead_code)]
    pub(crate) fn endpoint(&self) -> &DaemonEndpoint {
        self.client.endpoint()
    }

    /// 将输入排入专用 writer 线程，避免 UI loop 等待 daemon RPC 响应。
    pub(crate) fn enqueue_write(&mut self, session_id: String, data: Vec<u8>) {
        if self.write_tx.is_none() {
            let (tx, rx) = mpsc::channel::<(String, Vec<u8>)>();
            let endpoint = self.client.endpoint().clone();
            let options = self.options.clone();
            thread::Builder::new()
                .name("orca-daemon-writer".into())
                .spawn(move || {
                    let Ok(mut writer) = DaemonClient::connect_with(endpoint, options) else {
                        return;
                    };
                    while let Ok((session_id, data)) = rx.recv() {
                        let _ = writer.rpc(
                            "write",
                            serde_json::json!({
                                "sessionId": session_id,
                                "data": String::from_utf8_lossy(&data),
                            }),
                        );
                    }
                })
                .ok();
            self.write_tx = Some(tx);
        }
        if let Some(tx) = &self.write_tx {
            let _ = tx.send((session_id, data));
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
