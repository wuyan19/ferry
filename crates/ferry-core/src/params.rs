//! 全部请求参数 / 结果结构体（protocol.md §4）。
//!
//! serde 负责线格式，schemars 负责 `server.describe` 与 MCP `tools/list` 的
//! JSON Schema——同一份结构体是两种前端 schema 的单一事实来源。

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------- file.* ----------

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FileListParams {
    /// Directory path inside the sandbox (relative, '/'-separated). Empty = sandbox root.
    pub path: String,
    /// Recursively walk subdirectories. Default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursive: Option<bool>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DirEntry {
    pub name: String,
    /// "file" | "dir" | "other"
    pub r#type: String,
    pub size: u64,
    /// Unix epoch seconds.
    pub mtime: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileListResult {
    pub entries: Vec<DirEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FileStatParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileStatResult {
    pub name: String,
    pub r#type: String,
    pub size: u64,
    pub mtime: f64,
    /// Files only; null for directories.
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FileWriteParams {
    /// Destination path inside the sandbox. Parent directories are created automatically.
    pub path: String,
    pub content_base64: String,
    /// Unix permission bits (best effort, ignored on Windows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileWriteResult {
    pub bytes_written: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FileReadParams {
    pub path: String,
    /// Truncate after this many bytes. Default: read the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileReadResult {
    pub content_base64: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FileDeleteParams {
    pub path: String,
    /// Remove directories recursively. Required for non-empty directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursive: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileDeleteResult {
    pub deleted: bool,
}

// ---------- task / transfer ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Pending,
    Running,
    Success,
    Failed,
    Aborted,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Success | Self::Failed | Self::Aborted)
    }
}

/// 与 `FerryError::data()` 同构的任务层错误。
#[derive(Debug, Clone, Serialize)]
pub struct TaskErrorInfo {
    pub code: String,
    pub retryable: bool,
    pub suggestion: String,
    pub details: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferResult {
    pub task_id: String,
    pub state: TaskState,
    pub bytes_transferred: u64,
    pub duration_ms: u64,
    /// 数据完整性校验（fetch 成功时为落盘文件摘要）。
    pub sha256: Option<String>,
    pub error: Option<TaskErrorInfo>,
    /// 仅 transfer.fetch/push 的直接响应携带；事件推送中省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<bool>,
}

impl TransferResult {
    pub fn pending(task_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            state: TaskState::Pending,
            bytes_transferred: 0,
            duration_ms: 0,
            sha256: None,
            error: None,
            cached: Some(false),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TransferParams {
    /// IPv4 or hostname of the remote TFTP endpoint.
    pub host: String,
    /// Default 69.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub remote_path: String,
    /// Destination (fetch) / source (push) path inside the sandbox.
    pub local_path: String,
    /// RFC 2348 blksize, 8..=65464. Default 512.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_size: Option<u16>,
    /// RFC 7440 windowsize, 1..=16. Default 1 (classic per-block ACK).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_size: Option<u16>,
    /// Per-packet timeout in ms. Default 5000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Retries before giving up. Default 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    /// true: block until terminal state (custom protocol default false, MCP default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<bool>,
    /// Dedup key: in-flight tasks are reused; terminal results cached for 10 minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// task.cancel 的结果。
#[derive(Debug, Clone, Serialize)]
pub struct CancelResult {
    #[serde(flatten)]
    pub result: TransferResult,
    pub already_terminal: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TaskStatusParams {
    pub task_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TaskWaitParams {
    pub task_id: String,
    /// 0 = wait forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TaskCancelParams {
    pub task_id: String,
}

// ---------- listener ----------

// `inline`：嵌套结构的 schema 内联展开——部分 MCP 客户端不解析
// inputSchema 里的 $defs/$ref（会把字段当 string 处理）。
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[schemars(inline)]
pub struct ListenerAllow {
    /// Serve device RRQ (download) requests.
    #[serde(default)]
    pub read: bool,
    /// Accept device WRQ (upload) requests into `root/incoming/`.
    #[serde(default)]
    pub write: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListenerStartParams {
    pub port: u16,
    /// Bind address. Default "0.0.0.0".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// Sandbox-relative directory served to devices. Default sandbox root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub allow: ListenerAllow,
    /// Max concurrent inbound transfers. Default 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<usize>,
    /// CIDR allowlist; sources outside are rejected with TFTP ERROR(2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowlist: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListenerStartResult {
    pub listener_id: String,
    pub port: u16,
    pub state: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListenerStopParams {
    pub listener_id: String,
    /// Wait for in-flight transfers to finish before closing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListenerStopResult {
    pub stopped: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListenerStatusParams {
    pub listener_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListenerInfo {
    pub listener_id: String,
    pub port: u16,
    /// ["read"] / ["write"] / ["read","write"]
    pub mode: Vec<String>,
    pub root: String,
    pub state: String,
    pub active_transfers: usize,
}

// ---------- server ----------

#[derive(Debug, Clone, Serialize)]
pub struct SandboxInfo {
    pub root: String,
    pub used_bytes: u64,
    pub quota_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerStatusResult {
    pub uptime_s: u64,
    pub sandbox: SandboxInfo,
    pub listeners: Vec<ListenerInfo>,
    pub active_tasks: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ShutdownParams {
    /// Grace period for in-flight tasks. Default 5000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShutdownResult {
    pub ok: bool,
}
