//! App-facing daemon connection adapter.
//!
//! `DaemonClient` owns wire framing; this adapter limits the surface exposed to
//! `App` to connection lifecycle and RPC operations, keeping protocol details
//! out of input/render code.

use std::os::unix::net::UnixStream;

use crate::orca_daemon::{
    DaemonClient, DaemonConnectOptions, DaemonEndpoint, DaemonError, DaemonIdentity, Frame,
};

/// 供 App 使用的 daemon 连接边界。
pub(crate) struct DaemonConnection {
    client: DaemonClient,
}

impl DaemonConnection {
    /// 使用给定超时选项尝试建立连接。
    pub(crate) fn try_connect(options: DaemonConnectOptions) -> Option<Result<Self, DaemonError>> {
        DaemonClient::try_connect_with(options).map(|result| result.map(|client| Self { client }))
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

    /// 执行一个 JSON RPC。
    pub(crate) fn rpc(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, DaemonError> {
        self.client.rpc(method, params)
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
