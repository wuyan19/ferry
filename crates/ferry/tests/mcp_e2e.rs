//! MCP stdio 前端 e2e：spawn `ferry mcp`，完整走 握手 → tools/list → tools/call
//! （含结构化输出、isError 路径、transfer 默认 wait、progressToken）→ ping。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::TempDir;

struct McpSession {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
}

impl McpSession {
    fn start(sandbox: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .args([
                "mcp",
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
            .expect("spawn ferry mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin: Some(stdin), stdout }
    }

    fn read_msg(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read stdout line");
        assert!(!line.is_empty(), "server closed stdout unexpectedly");
        serde_json::from_str(line.trim()).expect("valid JSON line")
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().expect("stdin closed");
        stdin.write_all(v.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// 请求并等到对应 id 的响应（跳过通知）。
    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let msg = self.read_msg();
            if msg.get("id").and_then(Value::as_i64) == Some(id) {
                return msg;
            }
        }
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tool_call(s: &mut McpSession, id: i64, name: &str, args: Value) -> Value {
    s.request(id, "tools/call", json!({"name": name, "arguments": args}))
}

/// 从 tools/call 响应取 structuredContent（或回退解析 text）。
fn payload(resp: &Value) -> Value {
    let result = &resp["result"];
    assert!(result.get("isError").is_none_or(|v| !v.as_bool().unwrap_or(false)), "unexpected isError: {resp}");
    if let Some(sc) = result.get("structuredContent") {
        return sc.clone();
    }
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn mcp_full_session() {
    let tmp = TempDir::new().unwrap();
    let mut s = McpSession::start(tmp.path());

    // 1. 握手（首条 stdout 消息不可能是 server_ready——MCP 模式不发）
    let init = s.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "ferry-test", "version": "0.0"}
        }),
    );
    let r = &init["result"];
    assert_eq!(r["protocolVersion"], "2025-06-18");
    assert!(r["capabilities"]["tools"].is_object());
    assert!(r["capabilities"]["logging"].is_object());
    assert_eq!(r["serverInfo"]["name"], "ferry");
    assert!(r["instructions"].as_str().unwrap().contains("sandbox"));

    s.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    // 2. ping
    let ping = s.request(2, "ping", json!({}));
    assert!(ping["result"].as_object().unwrap().is_empty());

    // 3. tools/list：完整工具面
    let tools = s.request(3, "tools/list", json!({}));
    let list = tools["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = list.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "file_write", "file_read", "file_list", "file_stat", "file_delete",
        "transfer_fetch", "transfer_push",
        "listener_start", "listener_stop", "listener_status",
        "task_status", "task_wait", "task_cancel",
        "server_status", "server_shutdown",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}: {names:?}");
    }
    assert!(!names.contains(&"server_describe"));
    let fw = list.iter().find(|t| t["name"] == "file_write").unwrap();
    assert_eq!(fw["inputSchema"]["properties"]["path"]["type"], "string");

    // 4. file_write / file_read（结构化输出）
    let data = b"mcp payload bytes".to_vec();
    let resp = tool_call(&mut s, 4, "file_write", json!({
        "path": "fw/bin", "content_base64": B64.encode(&data)
    }));
    let p = payload(&resp);
    assert_eq!(p["bytes_written"], data.len() as u64);
    let expected_sha: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(p["sha256"], expected_sha);

    let resp = tool_call(&mut s, 5, "file_read", json!({"path": "fw/bin"}));
    let p = payload(&resp);
    assert_eq!(B64.decode(p["content_base64"].as_str().unwrap()).unwrap(), data);

    // 5. 业务失败 → isError:true 的结果（非 JSON-RPC error）
    let resp = tool_call(&mut s, 6, "file_stat", json!({"path": "nope.bin"}));
    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let err: Value = serde_json::from_str(text).unwrap();
    assert_eq!(err["code"], "FILE_NOT_FOUND");
    assert!(err["retryable"].is_boolean() && err["suggestion"].is_string());

    // 6. 参数校验失败 → 标准码 -32602（JSON-RPC error）
    let resp = s.request(7, "tools/call", json!({"name": "file_write", "arguments": {"path": 1}}));
    assert_eq!(resp["error"]["code"], -32602);

    // 7. 未知工具 → -32602
    let resp = s.request(8, "tools/call", json!({"name": "nope_tool", "arguments": {}}));
    assert_eq!(resp["error"]["code"], -32602);

    // 8. transfer 默认 wait:true：mini 设备 loopback
    let dev_data: Vec<u8> = (0..900u32).map(|i| (i * 11 + 1) as u8).collect();
    let addr = {
        // std 阻塞 mini 读设备（同 ndjson e2e 的实现）
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        let data = dev_data.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 65535];
            let (_n, from) = sock.recv_from(&mut buf).unwrap();
            let mut oack = vec![0u8, 6];
            for (k, v) in [
                ("blksize", "512".to_string()),
                ("tsize", data.len().to_string()),
                ("timeout", "1".to_string()),
                ("windowsize", "1".to_string()),
            ] {
                oack.extend_from_slice(k.as_bytes());
                oack.push(0);
                oack.extend_from_slice(v.as_bytes());
                oack.push(0);
            }
            sock.send_to(&oack, from).unwrap();
            let mut wait_ack = |block: u16| loop {
                let (n, _) = sock.recv_from(&mut buf).unwrap();
                if n == 4 && buf[..4] == [0, 4, (block >> 8) as u8, (block & 0xff) as u8] {
                    break;
                }
            };
            wait_ack(0);
            for (i, chunk) in data.chunks(512).enumerate() {
                let block = (i + 1) as u16;
                let mut pkt = vec![0, 3, (block >> 8) as u8, (block & 0xff) as u8];
                pkt.extend_from_slice(chunk);
                sock.send_to(&pkt, from).unwrap();
                wait_ack(block);
            }
        });
        addr
    };
    let resp = s.request(
        9,
        "tools/call",
        json!({
            "name": "transfer_fetch",
            "arguments": {
                "host": "127.0.0.1", "port": addr.port(),
                "remote_path": "fw.bin", "local_path": "mcp/fw.bin",
                "timeout_ms": 5000
            },
            "_meta": {"progressToken": "tok-9"}
        }),
    );
    let p = payload(&resp);
    assert_eq!(p["state"], "success");
    assert_eq!(p["bytes_transferred"], 900);
    let expected: String = Sha256::digest(&dev_data).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(p["sha256"], expected);
    let got = std::fs::read(tmp.path().join("mcp/fw.bin")).unwrap();
    assert_eq!(got, dev_data);

    // 9. transfer 失败 → isError（业务码在 payload，而非 JSON-RPC error）
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let dead = probe.local_addr().unwrap();
    drop(probe);
    let resp = s.request(
        10,
        "tools/call",
        json!({
            "name": "transfer_fetch",
            "arguments": {
                "host": "127.0.0.1", "port": dead.port(),
                "remote_path": "x", "local_path": "x.bin",
                "timeout_ms": 250, "retries": 1
            }
        }),
    );
    // wait 预算与引擎超时共用 timeout_ms，首轮可能未终态 → task_wait 补齐
    let mut p: Value = {
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    };
    for _ in 0..5 {
        if p["state"] != "pending" && p["state"] != "running" {
            break;
        }
        let next = tool_call(&mut s, 100, "task_wait", json!({
            "task_id": p["task_id"].as_str().unwrap(), "timeout_ms": 2000
        }));
        let t = next["result"]["content"][0]["text"].as_str().unwrap();
        p = serde_json::from_str(t).unwrap();
    }
    assert_eq!(p["state"], "failed");
    assert_eq!(p["error"]["code"], "REMOTE_TIMEOUT");

    // 10. server_status
    let resp = tool_call(&mut s, 11, "server_status", json!({}));
    let p = payload(&resp);
    assert_eq!(p["sandbox"]["quota_bytes"], 1048576);

    // 11. stdin EOF → 优雅退出 0
    drop(s.stdin.take());
    let status = s.child.wait().unwrap();
    assert!(status.success());
}
