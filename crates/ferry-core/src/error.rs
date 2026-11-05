//! 统一错误模型（protocol.md §6）。
//!
//! 错误分两层：
//! - **RPC 层**：请求本身不合法，前端渲染为 JSON-RPC `error`；
//! - **任务层**：传输过程失败，填入 `TransferResult.error`，绝不作为 RPC error 出现。
//!
//! `FerryError::data()` 产出的对象结构在两层完全一致：
//! `{code, retryable, suggestion, details}`。

use serde_json::{json, Value};

/// 业务错误码：符号名 + JSON-RPC 数字码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    SandboxViolation,
    FileNotFoundError,
    PermissionDenied,
    TaskNotFound,
    QuotaExceeded,
    RemoteTimeout,
    RemoteError,
    IdempotencyConflict,
    InvalidState,
    PortInUse,
    NetworkUnreachable,
}

impl ErrCode {
    pub fn num(self) -> i32 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::InternalError => -32603,
            Self::SandboxViolation => -32001,
            Self::FileNotFoundError => -32002,
            Self::PermissionDenied => -32003,
            Self::TaskNotFound => -32004,
            Self::QuotaExceeded => -32005,
            Self::RemoteTimeout => -32006,
            Self::RemoteError => -32007,
            Self::IdempotencyConflict => -32008,
            Self::InvalidState => -32009,
            Self::PortInUse => -32010,
            Self::NetworkUnreachable => -32011,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ParseError => "PARSE_ERROR",
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::MethodNotFound => "METHOD_NOT_FOUND",
            Self::InvalidParams => "INVALID_PARAMS",
            Self::InternalError => "INTERNAL_ERROR",
            Self::SandboxViolation => "SANDBOX_VIOLATION",
            Self::FileNotFoundError => "FILE_NOT_FOUND",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::TaskNotFound => "TASK_NOT_FOUND",
            Self::QuotaExceeded => "QUOTA_EXCEEDED",
            Self::RemoteTimeout => "REMOTE_TIMEOUT",
            Self::RemoteError => "REMOTE_ERROR",
            Self::IdempotencyConflict => "IDEMPOTENCY_CONFLICT",
            Self::InvalidState => "INVALID_STATE",
            Self::PortInUse => "PORT_IN_USE",
            Self::NetworkUnreachable => "NETWORK_UNREACHABLE",
        }
    }

    /// 属于任务层（只会出现在 `TransferResult.error`，不作为 RPC error 返回）。
    pub fn is_task_level(self) -> bool {
        matches!(self, Self::RemoteTimeout | Self::RemoteError | Self::NetworkUnreachable)
    }

    pub fn default_retryable(self) -> bool {
        matches!(
            self,
            Self::InternalError
                | Self::RemoteTimeout
                | Self::NetworkUnreachable
                | Self::QuotaExceeded // 清理后（§6 表）
                | Self::PortInUse     // 换端口（§6 表）
        )
    }
}

/// ferry 全场景错误。`data()` 的返回值同时用于：
/// RPC 层 JSON-RPC `error.data` 与任务层 `TransferResult.error`。
#[derive(Debug, Clone)]
pub struct FerryError {
    pub code: ErrCode,
    pub message: String,
    pub retryable: bool,
    pub suggestion: String,
    pub details: Value,
}

impl FerryError {
    pub fn new(code: ErrCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.default_retryable(),
            suggestion: String::new(),
            details: json!({}),
        }
    }

    pub fn with_suggestion(mut self, s: impl Into<String>) -> Self {
        self.suggestion = s.into();
        self
    }

    pub fn with_details(mut self, d: Value) -> Self {
        self.details = d;
        self
    }

    /// `{code, retryable, suggestion, details}`，两层错误共用。
    pub fn data(&self) -> Value {
        json!({
            "code": self.code.name(),
            "retryable": self.retryable,
            "suggestion": self.suggestion,
            "details": self.details,
        })
    }

    /// JSON-RPC error 的 message 字段：`"CODE: 具体原因"`。
    pub fn rpc_message(&self) -> String {
        format!("{}: {}", self.code.name(), self.message)
    }

    // ---- 常用构造器 ----

    pub fn sandbox_violation(path: &str) -> Self {
        Self::new(
            ErrCode::SandboxViolation,
            format!("path escapes sandbox root: {path:?}"),
        )
        .with_suggestion("Use a path relative to the sandbox root; '..', absolute paths and backslashes are rejected.")
        .with_details(json!({ "path": path }))
    }

    pub fn file_not_found(path: &str) -> Self {
        Self::new(ErrCode::FileNotFoundError, format!("file not found: {path:?}"))
            .with_suggestion("Call file.list to verify the path exists inside the sandbox.")
            .with_details(json!({ "path": path }))
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(ErrCode::InvalidParams, message)
            .with_suggestion("Fix the request params according to the method schema (server.describe).")
    }

    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::new(ErrCode::InvalidState, message).with_suggestion("Check server.status for the current state before retrying.")
    }

    pub fn task_not_found(task_id: &str) -> Self {
        Self::new(ErrCode::TaskNotFound, format!("unknown task_id: {task_id}"))
            .with_suggestion("Tasks are only addressable until a short while after reaching a terminal state.")
    }

    pub fn internal(source: impl std::fmt::Display) -> Self {
        Self::new(ErrCode::InternalError, source.to_string())
            .with_suggestion("Retry; if it persists, inspect stderr logs for the root cause.")
    }

    pub fn io(context: &str, e: std::io::Error) -> Self {
        Self::internal(format!("{context}: {e}"))
    }
}

impl std::fmt::Display for FerryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.rpc_message())
    }
}

impl std::error::Error for FerryError {}
