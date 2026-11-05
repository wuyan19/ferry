//! NDJSON JSON-RPC 2.0 前端（protocol.md §1–§7）。
//!
//! - 首条 stdout 消息必须是 `event.server_ready`（§5）
//! - 每行一个请求任务（并发处理，响应经单写者串行落盘）
//! - stdin EOF / server.shutdown → 优雅收尾
//! - 传输失败不是 JSON-RPC error（§4.3）：任务层错误只出现在 result 里

use ferry_core::error::{ErrCode, FerryError};
use ferry_core::event::Event;
use ferry_core::params::*;
use ferry_core::Api;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinSet;

use super::LineWriter;
use crate::schema;

const DEFAULT_GRACE_MS: u64 = 5000;

pub async fn run(api: Api) -> i32 {
    let (writer, writer_task) = super::spawn_line_writer();

    // §5：server_ready 必须是第一条 stdout 消息
    writer
        .json(json!({
            "jsonrpc": "2.0",
            "method": "event.server_ready",
            "params": ready_params(&api),
        }))
        .await;

    // 事件转发：core broadcast → event.* 通知
    let forwarder = tokio::spawn({
        let writer = writer.clone();
        let mut rx = api.events();
        async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => writer.json(event_to_json(ev)).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("event forwarder skipped {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    // 请求循环：每行一个任务，读循环永不阻塞在单个请求上（wait:true 依赖此性质）
    let stdin = tokio::io::stdin();
    let mut lines = tokio::io::BufReader::new(stdin).lines();
    let mut inflight: JoinSet<()> = JoinSet::new();
    let cancel = api.shutdown_token().clone();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    let api = api.clone();
                    let writer = writer.clone();
                    inflight.spawn(async move { handle_line(api, writer, line).await });
                }
                Ok(None) => break, // stdin EOF = 优雅关闭（§1）
                Err(e) => {
                    tracing::warn!("stdin read error: {e}");
                    break;
                }
            },
        }
    }

    drain(inflight, std::time::Duration::from_millis(DEFAULT_GRACE_MS)).await;
    drop(writer);
    forwarder.abort();
    let _ = writer_task.await;
    0
}

async fn drain(mut inflight: JoinSet<()>, grace: std::time::Duration) {
    let all = async { while inflight.join_next().await.is_some() {} };
    if tokio::time::timeout(grace, all).await.is_err() {
        inflight.abort_all();
    }
}

/// 单行消息处理：解析 → 分发 → 响应。
async fn handle_line(api: Api, writer: LineWriter, line: String) {
    let line = line.trim().trim_end_matches('\r');
    if line.is_empty() {
        return;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            writer
                .json(err_response(Value::Null, &FerryError::new(ErrCode::ParseError, e.to_string())))
                .await;
            return;
        }
    };
    let Value::Object(obj) = value else {
        writer
            .json(err_response(
                Value::Null,
                &FerryError::new(ErrCode::InvalidRequest, "message must be a JSON object"),
            ))
            .await;
        return;
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        if let Some(id) = id {
            writer
                .json(err_response(
                    id,
                    &FerryError::new(ErrCode::InvalidRequest, "missing 'method'"),
                ))
                .await;
        }
        return;
    };
    let Some(id) = id else {
        // 协议中不存在 Agent → Server 通知；按 JSON-RPC 语义不回包
        tracing::debug!("ignoring notification from agent: {method}");
        return;
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    let response = dispatch(&api, method, params, &id).await;
    writer.json(response).await;
}

async fn dispatch(api: &Api, method: &str, params: Value, id: &Value) -> Value {
    macro_rules! typed {
        ($t:ty, $api:ident . $call:ident) => {
            match parse_params::<$t>(method, params, id) {
                Ok(p) => match $api.$call(p).await {
                    Ok(r) => ok_response(id.clone(), &r),
                    Err(e) => err_response(id.clone(), &e),
                },
                Err(resp) => resp,
            }
        };
    }
    match method {
        "server.describe" => ok_response(id.clone(), &schema::describe()),
        "server.status" => match api.server_status().await {
            Ok(r) => ok_response(id.clone(), &r),
            Err(e) => err_response(id.clone(), &e),
        },
        "server.shutdown" => typed!(ShutdownParams, api.server_shutdown),
        "file.list" => typed!(FileListParams, api.file_list),
        "file.stat" => typed!(FileStatParams, api.file_stat),
        "file.write" => typed!(FileWriteParams, api.file_write),
        "file.read" => typed!(FileReadParams, api.file_read),
        "file.delete" => typed!(FileDeleteParams, api.file_delete),
        _ => err_response(
            id.clone(),
            &FerryError::new(ErrCode::MethodNotFound, format!("unknown method: {method}"))
                .with_suggestion("Call server.describe to list available methods."),
        ),
    }
}

fn parse_params<T: DeserializeOwned>(method: &str, params: Value, id: &Value) -> Result<T, Value> {
    serde_json::from_value(params).map_err(|e| {
        err_response(
            id.clone(),
            &FerryError::invalid_params(format!("{method}: {e}")),
        )
    })
}

fn ok_response(id: Value, result: &impl serde::Serialize) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err_response(id: Value, e: &FerryError) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": e.code.num(),
            "message": e.rpc_message(),
            "data": e.data(),
        }
    })
}

fn ready_params(api: &Api) -> Value {
    json!({
        "pid": std::process::id(),
        "protocol_version": schema::PROTOCOL_VERSION,
        "server_version": env!("CARGO_PKG_VERSION"),
        "sandbox_root": api.sandbox().display_root().display().to_string(),
        "capabilities": {
            "transfer": ["fetch", "push"],
            "listener": {"read": true, "write": true, "allowlist": true},
            "tftp_options": ["blksize", "tsize", "timeout", "windowsize"],
            "idempotency": true,
            "max_concurrent_tasks": api.max_concurrent_tasks(),
        }
    })
}

/// core Event → `event.*` 通知（方法名映射属于本前端协议）。
fn event_to_json(ev: Event) -> Value {
    let (method, params): (&str, Value) = match ev {
        Event::InboundRequest {
            listener_id,
            peer,
            operation,
            path,
            task_id,
        } => (
            "event.inbound_request",
            json!({"listener_id": listener_id, "peer": peer, "operation": operation,
                   "path": path, "task_id": task_id}),
        ),
        Event::TransferProgress {
            task_id,
            bytes_transferred,
            total_bytes,
            percent,
        } => (
            "event.transfer_progress",
            json!({"task_id": task_id, "bytes_transferred": bytes_transferred,
                   "total_bytes": total_bytes, "percent": percent}),
        ),
        Event::TransferCompleted { result } => {
            ("event.transfer_completed", serde_json::to_value(&result).unwrap_or(Value::Null))
        }
        Event::ListenerStopped { listener_id, reason } => (
            "event.listener_stopped",
            json!({"listener_id": listener_id, "reason": reason}),
        ),
    };
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}
