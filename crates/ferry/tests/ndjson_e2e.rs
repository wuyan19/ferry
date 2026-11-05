//! NDJSON 前端 e2e：spawn ferry serve，走真实 stdio 完成协议交互。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::TempDir;

struct Session {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Session {
    fn start(sandbox: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .args([
                "serve",
                "--sandbox-root",
                sandbox.to_str().unwrap(),
                "--quota-bytes",
                "1048576",
                "--log-level",
                "warn",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ferry");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout }
    }

    /// 阻塞读一行（30s 超时靠测试框架超时兜底）。
    fn read_msg(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read stdout line");
        assert!(!line.is_empty(), "server closed stdout unexpectedly");
        serde_json::from_str(line.trim()).expect("valid JSON line")
    }

    fn send(&mut self, v: Value) {
        self.stdin.write_all(v.to_string().as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    /// 发请求并等到对应 id 的响应（跳过其间插入的事件通知）。
    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let msg = self.read_msg();
            if msg.get("id").and_then(Value::as_i64) == Some(id) {
                return msg;
            }
        }
    }

    fn result(&mut self, id: i64, method: &str, params: Value) -> Value {
        let msg = self.request(id, method, params);
        assert!(
            msg.get("error").is_none(),
            "unexpected error for {method}: {msg}"
        );
        msg["result"].clone()
    }

    fn error(&mut self, id: i64, method: &str, params: Value) -> (i64, Value) {
        let msg = self.request(id, method, params);
        let e = msg.get("error").cloned().unwrap_or_else(|| panic!("expected error for {method}, got {msg}"));
        (e["code"].as_i64().unwrap(), e["data"].clone())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn ndjson_full_session() {
    let tmp = TempDir::new().unwrap();
    let mut s = Session::start(tmp.path());

    // 1. 首条消息必须是 server_ready，携带 capabilities
    let ready = s.read_msg();
    assert_eq!(ready["method"], "event.server_ready");
    let p = &ready["params"];
    assert_eq!(p["protocol_version"], "1.0");
    assert!(p["pid"].as_u64().is_some());
    assert!(p["sandbox_root"].is_string());
    assert!(p["capabilities"]["idempotency"].as_bool().unwrap());

    // 2. file.write（自动建父目录）→ bytes_written + sha256
    let payload = b"firmware-bytes-0123456789";
    let r = s.result(1, "file.write", json!({
        "path": "fw/v2.3.1/firmware.bin",
        "content_base64": B64.encode(payload),
    }));
    assert_eq!(r["bytes_written"], payload.len() as u64);
    assert_eq!(r["sha256"], sha256_hex(payload));

    // 3. file.list
    let r = s.result(2, "file.list", json!({"path": "fw/v2.3.1"}));
    let entries = r["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "firmware.bin");
    assert_eq!(entries[0]["type"], "file");

    // 3b. recursive list 根目录
    let r = s.result(3, "file.list", json!({"path": "", "recursive": true}));
    let entries = r["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["name"] == "fw/v2.3.1/firmware.bin"));

    // 4. file.stat
    let r = s.result(4, "file.stat", json!({"path": "fw/v2.3.1/firmware.bin"}));
    assert_eq!(r["sha256"], sha256_hex(payload));
    assert_eq!(r["type"], "file");

    // 5. file.read 往返
    let r = s.result(5, "file.read", json!({"path": "fw/v2.3.1/firmware.bin"}));
    assert_eq!(r["truncated"], false);
    assert_eq!(
        B64.decode(r["content_base64"].as_str().unwrap()).unwrap(),
        payload
    );

    // 5b. max_bytes 截断
    let r = s.result(6, "file.read", json!({"path": "fw/v2.3.1/firmware.bin", "max_bytes": 4}));
    assert_eq!(r["truncated"], true);

    // 6. 沙箱越界 / 坏 base64 / 未知方法 / 坏 JSON
    let (code, data) = s.error(7, "file.write", json!({"path": "../evil", "content_base64": "QUJD"}));
    assert_eq!(code, -32001);
    assert_eq!(data["code"], "SANDBOX_VIOLATION");
    assert!(data["retryable"].is_boolean() && data["suggestion"].is_string());

    let (code, _) = s.error(8, "file.write", json!({"path": "x.bin", "content_base64": "!!!"}));
    assert_eq!(code, -32602);

    let (code, _) = s.error(9, "no.such.method", json!({}));
    assert_eq!(code, -32601);

    s.send(json!({"jsonrpc": "2.0"})); // 缺 method 且无 id：静默忽略
    let (code, _) = {
        s.stdin.write_all(b"not-json\n").unwrap();
        s.stdin.flush().unwrap();
        let msg = s.read_msg();
        assert!(msg.get("id").is_none() || msg["id"].is_null());
        (msg["error"]["code"].as_i64().unwrap(), msg["error"]["data"].clone())
    };
    assert_eq!(code, -32700);

    // 7. file.delete
    let r = s.result(10, "file.delete", json!({"path": "fw/v2.3.1/firmware.bin"}));
    assert_eq!(r["deleted"], true);
    let (code, _) = s.error(11, "file.stat", json!({"path": "fw/v2.3.1/firmware.bin"}));
    assert_eq!(code, -32002);

    // 8. server.status：沙箱根与用量
    let r = s.result(12, "server.status", json!({}));
    assert_eq!(r["sandbox"]["used_bytes"], 0);
    assert_eq!(r["sandbox"]["quota_bytes"], 1048576);
    assert_eq!(r["sandbox"]["root"], tmp.path().canonicalize().unwrap().display().to_string().replace("\\\\?\\", ""));

    // 9. server.describe：schema 齐全
    let r = s.result(13, "server.describe", json!({}));
    let methods = r["methods"].as_object().unwrap();
    assert!(methods.contains_key("file.write"));
    assert!(methods.contains_key("transfer.fetch"));
    assert_eq!(methods["file.write"]["params"]["properties"]["path"]["type"], "string");
    assert_eq!(r["error_codes"]["-32002"], "FILE_NOT_FOUND");

    // 10. shutdown → 退出码 0
    let r = s.result(14, "server.shutdown", json!({"grace_ms": 1000}));
    assert_eq!(r["ok"], true);
    let status = s.child.wait().unwrap();
    assert!(status.success());
}
