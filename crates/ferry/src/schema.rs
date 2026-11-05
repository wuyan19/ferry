//! 方法/工具的 schema 注册表：`server.describe` 与 MCP `tools/list` 共用。
//!
//! schemars derive 在 ferry-core::params 上，本模块只做名称映射与汇总，
//! 保证线格式、自描述 schema 永远同源。

use ferry_core::params::*;
use ferry_core::ErrCode;
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "1.0";

fn schema_of<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| json!({"type": "object"}))
}

struct MethodDef {
    /// 自定义协议方法名（点分）。MCP 工具名 = 本名 '.' → '_'。
    method: &'static str,
    summary: &'static str,
    params: fn() -> Value,
}

const NO_PARAMS: fn() -> Value = || json!({"type": "object", "additionalProperties": false});

/// 与前端实现同步维护：新增方法必须同时加进 ndjson::dispatch。
const METHODS: &[MethodDef] = &[
    MethodDef {
        method: "server.describe",
        summary: "Return all method schemas and the error-code catalog.",
        params: NO_PARAMS,
    },
    MethodDef {
        method: "server.status",
        summary: "Runtime status: sandbox usage, listeners, active tasks.",
        params: NO_PARAMS,
    },
    MethodDef {
        method: "server.shutdown",
        summary: "Graceful shutdown; in-flight tasks get grace_ms before being aborted.",
        params: || schema_of::<ShutdownParams>(),
    },
    MethodDef {
        method: "file.list",
        summary: "List a sandbox directory.",
        params: || schema_of::<FileListParams>(),
    },
    MethodDef {
        method: "file.stat",
        summary: "Stat a sandbox file (size, mtime, sha256).",
        params: || schema_of::<FileStatParams>(),
    },
    MethodDef {
        method: "file.write",
        summary: "Write base64 payload into the sandbox (<= 1 MiB recommended).",
        params: || schema_of::<FileWriteParams>(),
    },
    MethodDef {
        method: "file.read",
        summary: "Read a sandbox file as base64 (optionally truncated).",
        params: || schema_of::<FileReadParams>(),
    },
    MethodDef {
        method: "file.delete",
        summary: "Delete a sandbox file or directory.",
        params: || schema_of::<FileDeleteParams>(),
    },
    MethodDef {
        method: "transfer.fetch",
        summary: "Pull a file from a remote TFTP server into the sandbox.",
        params: || schema_of::<TransferParams>(),
    },
    MethodDef {
        method: "transfer.push",
        summary: "Push a sandbox file to a remote TFTP server.",
        params: || schema_of::<TransferParams>(),
    },
    MethodDef {
        method: "listener.start",
        summary: "Start a passive TFTP listener for devices to read/upload.",
        params: || schema_of::<ListenerStartParams>(),
    },
    MethodDef {
        method: "listener.stop",
        summary: "Stop a listener (optionally draining in-flight transfers).",
        params: || schema_of::<ListenerStopParams>(),
    },
    MethodDef {
        method: "listener.status",
        summary: "Query a listener.",
        params: || schema_of::<ListenerStatusParams>(),
    },
    MethodDef {
        method: "task.status",
        summary: "Snapshot of a transfer task.",
        params: || schema_of::<TaskStatusParams>(),
    },
    MethodDef {
        method: "task.wait",
        summary: "Block until a task reaches a terminal state (or timeout).",
        params: || schema_of::<TaskWaitParams>(),
    },
    MethodDef {
        method: "task.cancel",
        summary: "Cooperatively cancel a task; idempotent for terminal tasks.",
        params: || schema_of::<TaskCancelParams>(),
    },
];

/// `server.describe` 的 result。
pub fn describe() -> Value {
    let mut methods = serde_json::Map::new();
    for def in METHODS {
        methods.insert(
            def.method.to_string(),
            json!({
                "summary": def.summary,
                "params": (def.params)(),
                "result": {"type": "object"},
            }),
        );
    }
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "server_version": env!("CARGO_PKG_VERSION"),
        "methods": Value::Object(methods),
        "error_codes": error_codes(),
    })
}

fn error_codes() -> Value {
    let codes = [
        ErrCode::ParseError,
        ErrCode::InvalidRequest,
        ErrCode::MethodNotFound,
        ErrCode::InvalidParams,
        ErrCode::InternalError,
        ErrCode::SandboxViolation,
        ErrCode::FileNotFoundError,
        ErrCode::PermissionDenied,
        ErrCode::TaskNotFound,
        ErrCode::QuotaExceeded,
        ErrCode::RemoteTimeout,
        ErrCode::RemoteError,
        ErrCode::IdempotencyConflict,
        ErrCode::InvalidState,
        ErrCode::PortInUse,
        ErrCode::NetworkUnreachable,
    ];
    let mut map = serde_json::Map::new();
    for c in codes {
        map.insert(c.num().to_string(), Value::String(c.name().to_string()));
    }
    Value::Object(map)
}

/// MCP `tools/list` 的工具数组（M4 前端使用）。
#[allow(dead_code)] // M4 接入
pub fn mcp_tools() -> Value {
    METHODS
        .iter()
        .filter(|d| d.method != "server.describe") // tools/list 自描述
        .map(|d| {
            json!({
                "name": d.method.replace('.', "_"),
                "description": d.summary,
                "inputSchema": (d.params)(),
            })
        })
        .collect::<Vec<_>>()
        .into()
}
