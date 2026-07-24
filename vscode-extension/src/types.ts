// ACP (Agent Communication Protocol) 类型定义
//
// 对齐 agent-client-protocol 0.10.4 的 JSON-RPC 消息结构。
// 仅声明扩展实际使用到的子集，避免过度类型化。

/** JSON-RPC 2.0 请求（带 id，期待响应） */
export interface AcpRequest {
    jsonrpc: '2.0';
    method: string;
    params?: unknown;
    id: number | string;
}

/** JSON-RPC 2.0 通知（无 id，无响应） */
export interface AcpNotification {
    jsonrpc: '2.0';
    method: string;
    params?: unknown;
}

/** JSON-RPC 2.0 成功响应 */
export interface AcpSuccessResponse {
    jsonrpc: '2.0';
    id: number | string;
    result: unknown;
}

/** JSON-RPC 2.0 错误响应 */
export interface AcpErrorResponse {
    jsonrpc: '2.0';
    id: number | string;
    error: {
        code: number;
        message: string;
        data?: unknown;
    };
}

export type AcpResponse = AcpSuccessResponse | AcpErrorResponse;

/** 传输层接收到的一条任意消息 */
export type AcpMessage = AcpRequest | AcpNotification | AcpResponse;

/** ACP 标准错误码 */
export const AcpErrorCode = {
    PARSE_ERROR: -32700,
    INVALID_REQUEST: -32600,
    METHOD_NOT_FOUND: -32601,
    INVALID_PARAMS: -32602,
    INTERNAL_ERROR: -32603,
} as const;

/** initialize 请求参数 */
export interface InitializeParams {
    protocolVersion: number;
    clientCapabilities: {
        fs_read_text_file?: boolean;
        fs_write_text_file?: boolean;
        session_request_permission?: boolean;
    };
}

/** initialize 响应 */
export interface InitializeResult {
    protocolVersion: number;
    agentName: string;
    agentVersion: string;
    authMethods: string[];
}

/** session/new 请求参数 */
export interface NewSessionParams {
    cwd?: string;
    mcpServers?: unknown[];
    customInstructions?: string;
}

/** session/new 响应 */
export interface NewSessionResult {
    sessionId: string;
}

/** session/prompt 请求参数 */
export interface PromptParams {
    sessionId: string;
    prompt: Array<{ type: 'text'; text: string }>;
}

/** session/prompt 响应 */
export interface PromptResult {
    stopReason: string;
}

/** session/cancel 通知参数 */
export interface CancelParams {
    sessionId: string;
}

/** fs/read_text_file 反向请求参数 */
export interface ReadTextFileParams {
    path: string;
}

/** fs/read_text_file 响应 */
export interface ReadTextFileResult {
    content: string;
}

/** fs/write_text_file 反向请求参数 */
export interface WriteTextFileParams {
    path: string;
    content: string;
}

/** fs/write_text_file 响应 */
export interface WriteTextFileResult {
    written: boolean;
}

/** session/request_permission 反向请求参数 */
export interface RequestPermissionParams {
    toolName: string;
    toolInput: unknown;
    options: string[];
}

/** session/request_permission 响应 */
export interface RequestPermissionResult {
    outcome: 'allow' | 'deny' | 'always_allow';
}

/** session/update 通知参数（agent → IDE 流式推送） */
export interface SessionUpdateParams {
    sessionId: string;
    update: SessionUpdate;
}

/** ACP 0.10.4 SessionUpdate 枚举子集 */
export type SessionUpdate =
    | { type: 'AgentMessageChunk'; content: { type: 'text'; text: string } }
    | { type: 'ToolCall'; toolCallId: string; toolName: string; status: 'pending' | 'completed' | 'failed'; input?: unknown }
    | { type: 'ToolCallStatus'; toolCallId: string; status: 'pending' | 'completed' | 'failed' };

/** 扩展配置（对应 package.json configuration） */
export interface ClawConfig {
    binaryPath: string;
    model: string;
    permissionMode: 'read-only' | 'workspace-write' | 'danger-full-access';
    autoStart: boolean;
    logLevel: 'error' | 'warn' | 'info' | 'debug';
}
