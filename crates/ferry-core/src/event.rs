//! core → 前端 的事件（protocol.md §5）。
//!
//! `server_ready` / `server_log` 由前端自行生成，不在此列。
//! 前端订阅 broadcast 后各自翻译：ndjson → `event.*` 通知；MCP → `notifications/*`。

use crate::params::{TransferResult};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// 设备向监听器发起 RRQ/WRQ。
    InboundRequest {
        listener_id: String,
        /// "ip:port"
        peer: String,
        /// "read" | "write"
        operation: String,
        path: String,
        task_id: String,
    },
    TransferProgress {
        task_id: String,
        bytes_transferred: u64,
        /// 依赖对端 tsize，可能未知。
        total_bytes: Option<u64>,
        percent: Option<f64>,
    },
    /// 任务进入终态（含完整 TransferResult）。
    TransferCompleted {
        #[serde(flatten)]
        result: TransferResult,
    },
    ListenerStopped {
        listener_id: String,
        reason: String,
    },
}
