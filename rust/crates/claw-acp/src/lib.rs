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

pub use self::stdio::spawn_stdin_line_reader;
pub use self::{
    channel::{acp_channels, acp_send, AcpAgentChannel, AcpChannel, AcpClientChannel},
    common::{
        acp_channel_failure, acp_internal_error, AcpAgentRx, AcpAgentTx, AcpChannelFailure,
        AcpClientRx, AcpClientTx, AcpResult, AcpRxo, AcpTxo,
    },
    gateway::{
        acp_gateway, AcpAgentGatewayReceiver, AcpAgentGatewaySender, AcpClientGatewayReceiver,
        AcpClientGatewaySender, AcpGatewayReceiver, AcpGatewaySender,
    },
    message::{
        AcpAgentMessage, AcpAgentMessageBox, AcpAgentMessageGeneric, AcpArgs, AcpArgsBox,
        AcpClientMessage, AcpClientMessageBox, AcpClientMessageGeneric, AcpMethod, AcpRequest,
        AcpSide, Boxed, ModelSwitchError, StorageMarker, Unboxed,
    },
};

#[doc(hidden)]
pub use self::common::compact_json;

/// Build a 1.3 `SetSessionConfigOptionRequest` for model switching.
/// Only available under the `acp-1_5` feature flag.
#[cfg(feature = "acp-1_5")]
pub use self::message::build_set_model_request_v1_3;

/// Unified, version-agnostic model-switch capability check.
pub use self::message::set_session_model_compat;
