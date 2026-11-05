//! MCP stdio 前端（protocol.md §9）。
//!
//! 手写最小 MCP 面（initialize / initialized / tools/list / tools/call / ping /
//! logging/set_level），与 NDJSON 前端共享 core、stdout 单写者与分行纪律。
//!
//! 语义差异（§9.3–9.4）：
//! - `transfer_*` 默认 `wait:true`；
//! - 业务失败（含全部业务码）一律 `isError:true` 的 tools/call 结果，
//!   JSON-RPC error 仅保留标准码（-32700 ~ -32603）；
//! - 事件 → `notifications/message`（按 set_level 过滤）；
//!   带 progressToken 的调用额外获得 `notifications/progress`。

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use ferry_core::error::FerryError;
use ferry_core::event::Event;
use ferry_core::params::*;
use ferry_core::Api;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinSet;

use super::LineWriter;
use crate::schema;

/// 支持的 MCP 协议版本（最低基线）。
const SUPPORTED_VERSION: &str = "2025-06-18";

fn level_rank(s: &str) -> u8 {
    match s {
        "debug" => 0,
        "info" => 1,
        "warning" | "warn" => 2,
        _ => 3, // error 及以上
    }
}

#[derive(Debug, Clone, Default)]
struct McpState {
    client_version: Option<String>,
}

impl McpState {
    fn supports_structured(&self) -> bool {
        self.client_version.as_deref().unwrap_or(SUPPORTED_VERSION) >= SUPPORTED_VERSION
    }
}

pub async fn run(api: Api) -> i32 {
    let (writer, writer_task) = super::spawn_line_writer();
    let threshold = Arc::new(AtomicU8::new(level_rank("info")));
    let state = Arc::new(Mutex::new(McpState::default()));

    // 事件转发：event.* → notifications/message（§9.4）
    let forwarder = tokio::spawn({
        let writer = writer.clone();
        let api = api.clone();
        let threshold = threshold.clone();
        async move {
            let mut rx = api.events();
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let (level, data) = match ev {
                            Event::TransferProgress { task_id, bytes_transferred, total_bytes, percent } => (
                                "info",
                                json!({"kind":"transfer_progress","task_id":task_id,
                                       "bytes_transferred":bytes_transferred,
                                       "total_bytes":total_bytes,"percent":percent}),
                            ),
                            Event::TransferCompleted { result } => (
                                "info",
                                json!({"kind":"transfer_completed","result":result}),
                            ),
                            Event::InboundRequest { listener_id, peer, operation, path, task_id } => (
                                "info",
                                json!({"kind":"inbound_request","listener_id":listener_id,
                                       "peer":peer,"operation":operation,"path":path,"task_id":task_id}),
                            ),
                            Event::ListenerStopped { listener_id, reason } => (
                                "warning",
                                json!({"kind":"listener_stopped","listener_id":listener_id,"reason":reason}),
                            ),
                        };
                        if level_rank(level) >= threshold.load(Ordering::Relaxed) {
                            writer
                                .json(json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/message",
                                    "params": {"level": level, "logger": "ferry", "data": data}
                                }))
                                .await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("mcp event forwarder skipped {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

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
                    let state = state.clone();
                    let threshold = threshold.clone();
                    inflight.spawn(async move {
                        handle_line(api, writer, line, state, threshold).await;
                    });
                }
                Ok(None) => break, // stdin EOF = 优雅关闭（§9.5）
                Err(e) => {
                    tracing::warn!("stdin read error: {e}");
                    break;
                }
            },
        }
    }

    let drain = async { while inflight.join_next().await.is_some() {} };
    if tokio::time::timeout(std::time::Duration::from_secs(5), drain).await.is_err() {
        inflight.abort_all();
    }
    drop(writer);
    forwarder.abort();
    let _ = writer_task.await;
    0
}

async fn handle_line(
    api: Api,
    writer: LineWriter,
    line: String,
    state: Arc<Mutex<McpState>>,
    threshold: Arc<AtomicU8>,
) {
    let line = line.trim().trim_end_matches('\r');
    if line.is_empty() {
        return;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            writer.json(std_err(Value::Null, -32700, &e.to_string())).await;
            return;
        }
    };
    let Value::Object(obj) = value else {
        writer.json(std_err(Value::Null, -32600, "message must be a JSON object")).await;
        return;
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        if let Some(id) = id {
            writer.json(std_err(id, -32600, "missing 'method'")).await;
        }
        return;
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    if let Some(id) = id {
        let response = dispatch(&api, &writer, &state, &threshold, method, params, id).await;
        writer.json(response).await;
    } else {
        match method {
            "notifications/initialized" => tracing::debug!("mcp client initialized"),
            "notifications/cancelled" => tracing::debug!("mcp client cancelled a request (ignored)"),
            other => tracing::debug!("ignoring client notification: {other}"),
        }
    }
}

async fn dispatch(
    api: &Api,
    writer: &LineWriter,
    state: &Arc<Mutex<McpState>>,
    threshold: &AtomicU8,
    method: &str,
    params: Value,
    id: Value,
) -> Value {
    match method {
        "initialize" => initialize(state, params, id),
        "ping" => ok(id, json!({})),
        "logging/set_level" => {
            let level = params.get("level").and_then(Value::as_str).unwrap_or("info");
            threshold.store(level_rank(level), Ordering::Relaxed);
            ok(id, json!({}))
        }
        "tools/list" => ok(id, json!({"tools": schema::mcp_tools()})),
        "tools/call" => tool_call(api, writer, state, params, id).await,
        _ => std_err(id, -32601, &format!("unknown method: {method}")),
    }
}

fn initialize(state: &Arc<Mutex<McpState>>, params: Value, id: Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(SUPPORTED_VERSION)
        .to_string();
    let negotiated = if requested.as_str() >= SUPPORTED_VERSION {
        requested.clone()
    } else {
        SUPPORTED_VERSION.to_string()
    };
    state.lock().unwrap().client_version = Some(requested);
    ok(
        id,
        json!({
            "protocolVersion": negotiated,
            "capabilities": {
                "tools": {"listChanged": false},
                "logging": {}
            },
            "serverInfo": {"name": "ferry", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "ferry moves files between you and TFTP devices through a sandboxed root. \
Use transfer_fetch/transfer_push for active transfers (they wait for completion by default), \
listener_start to let devices download or upload, file_* tools for sandbox housekeeping. \
Paths are sandbox-relative ('a/b.bin'); device uploads land under <listener root>/incoming/."
        }),
    )
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn std_err(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// tools/call 结果统一封装：text 恒有 JSON；新版本客户端附 structuredContent。
fn tool_result(id: Value, payload: Value, is_error: bool, state: &McpState) -> Value {
    let mut result = json!({
        "content": [{"type": "text", "text": payload.to_string()}]
    });
    if state.supports_structured() {
        result["structuredContent"] = payload;
    }
    if is_error {
        result["isError"] = Value::Bool(true);
    }
    ok(id, result)
}

enum Outcome {
    Ok(Value),
    Fail(FerryError),
}

fn parse_args<T: DeserializeOwned>(name: &str, arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|e| format!("{name}: {e}"))
}

async fn tool_call(
    api: &Api,
    writer: &LineWriter,
    state: &Arc<Mutex<McpState>>,
    params: Value,
    id: Value,
) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let progress_token = params.pointer("/_meta/progressToken").cloned().filter(|v| !v.is_null());
    let st = state.lock().unwrap().clone();

    macro_rules! tool {
        ($t:ty, $call:ident) => {
            match parse_args::<$t>(&name, arguments.clone()) {
                Ok(p) => match api.$call(p).await {
                    Ok(r) => Outcome::Ok(serde_json::to_value(&r).unwrap_or(Value::Null)),
                    Err(e) => Outcome::Fail(e),
                },
                Err(e) => return std_err(id, -32602, &e),
            }
        };
    }

    let outcome = match name.as_str() {
        "server_status" => match api.server_status().await {
            Ok(r) => Outcome::Ok(serde_json::to_value(&r).unwrap_or(Value::Null)),
            Err(e) => Outcome::Fail(e),
        },
        "file_list" => tool!(FileListParams, file_list),
        "file_stat" => tool!(FileStatParams, file_stat),
        "file_write" => tool!(FileWriteParams, file_write),
        "file_read" => tool!(FileReadParams, file_read),
        "file_delete" => tool!(FileDeleteParams, file_delete),
        "task_status" => tool!(TaskStatusParams, task_status),
        "task_wait" => tool!(TaskWaitParams, task_wait),
        "task_cancel" => tool!(TaskCancelParams, task_cancel),
        "listener_start" => tool!(ListenerStartParams, listener_start),
        "listener_stop" => tool!(ListenerStopParams, listener_stop),
        "listener_status" => tool!(ListenerStatusParams, listener_status),
        "server_shutdown" => tool!(ShutdownParams, server_shutdown),
        "transfer_fetch" | "transfer_push" => {
            return transfer_tool(api, writer, &name, arguments, progress_token, id, &st).await;
        }
        _ => return std_err(id, -32602, &format!("unknown tool: {name}")),
    };

    match outcome {
        Outcome::Ok(v) => tool_result(id, v, false, &st),
        Outcome::Fail(e) => tool_result(id, e.data(), true, &st),
    }
}

/// transfer_fetch / transfer_push：MCP 默认 wait:true + progressToken 进度通知。
async fn transfer_tool(
    api: &Api,
    writer: &LineWriter,
    name: &str,
    arguments: Value,
    progress_token: Option<Value>,
    id: Value,
    st: &McpState,
) -> Value {
    let p: TransferParams = match parse_args(name, arguments) {
        Ok(p) => p,
        Err(e) => return std_err(id, -32602, &e),
    };
    let wait = p.wait.unwrap_or(true); // §9.3：MCP 模式默认 true
    let start = if name == "transfer_fetch" {
        api.transfer_fetch(p.clone()).await
    } else {
        api.transfer_push(p.clone()).await
    };
    let start = match start {
        Ok(s) => s,
        Err(e) => return tool_result(id, e.data(), true, st),
    };

    if let Some(token) = progress_token {
        spawn_progress_watcher(api.clone(), writer.clone(), token, start.task_id.clone());
    }

    let mut snapshot = start.snapshot;
    if wait && !snapshot.state.is_terminal() {
        let timeout = p
            .timeout_ms
            .filter(|&ms| ms > 0)
            .map(std::time::Duration::from_millis);
        match api.task_wait_raw(&start.task_id, timeout).await {
            Ok(mut s) => {
                s.cached = snapshot.cached;
                snapshot = s;
            }
            Err(e) => return tool_result(id, e.data(), true, st),
        }
    }
    tool_result(id, serde_json::to_value(&snapshot).unwrap_or(Value::Null), false, st)
}

/// 该任务的进度事件 → notifications/progress，直至任务终态。
fn spawn_progress_watcher(api: Api, writer: LineWriter, token: Value, task_id: String) {
    tokio::spawn(async move {
        let mut rx = api.events();
        loop {
            match rx.recv().await {
                Ok(Event::TransferProgress { task_id: t, bytes_transferred, total_bytes, .. })
                    if t == task_id =>
                {
                    let mut params = json!({
                        "progressToken": token,
                        "progress": bytes_transferred,
                    });
                    if let Some(total) = total_bytes {
                        params["total"] = total.into();
                    }
                    writer
                        .json(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": params}))
                        .await;
                }
                Ok(Event::TransferCompleted { result }) if result.task_id == task_id => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
}
