//! ACP (Agent Communication Protocol) channel/gateway layer for claw-code.
//!
//! Ported from xai-org/grok-build `xai-acp-lib` (Apache-2.0).
//! Provides in-process mpsc-based ACP channels and gateway dispatch,
//! decoupling the TUI/headless frontend from the agent runtime.
//!
//! 核心四模块:channel / common / gateway / message。
//! stdio transport:stdio 模块提供专用线程 stdin 行读取器,绕过
//! `tokio::io::stdin()` 的缓冲问题,用于 stdio ACP 服务器场景。

mod channel;
mod common;
mod gateway;
mod message;
pub mod stdio;

pub use self::{
    channel::{AcpAgentChannel, AcpChannel, AcpClientChannel, acp_channels, acp_send},
    common::{
        AcpAgentRx, AcpAgentTx, AcpChannelFailure, AcpClientRx, AcpClientTx, AcpResult, AcpRxo,
        AcpTxo, acp_channel_failure, acp_internal_error,
    },
    gateway::{
        AcpAgentGatewayReceiver, AcpAgentGatewaySender, AcpClientGatewayReceiver,
        AcpClientGatewaySender, AcpGatewayReceiver, AcpGatewaySender, acp_gateway,
    },
    message::{
        AcpAgentMessage, AcpAgentMessageBox, AcpAgentMessageGeneric, AcpArgs, AcpArgsBox,
        AcpClientMessage, AcpClientMessageBox, AcpClientMessageGeneric, AcpMethod, AcpRequest,
        AcpSide, Boxed, StorageMarker, Unboxed,
    },
};
pub use self::stdio::spawn_stdin_line_reader;

#[doc(hidden)]
pub use self::common::compact_json;
