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

// ---------- 传输场景：std 阻塞 mini TFTP 设备 ----------

fn push_cstr(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

/// 单请求读设备：OACK → 按 512B 块发送 → 等每块 ACK。
fn std_read_device(data: Vec<u8>) -> std::net::SocketAddr {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sock.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 65535];
        let (_n, from) = sock.recv_from(&mut buf).unwrap(); // RRQ
        let mut oack = vec![0u8, 6];
        for (k, v) in [
            ("blksize", "512".to_string()),
            ("tsize", data.len().to_string()),
            ("timeout", "1".to_string()),
            ("windowsize", "1".to_string()),
        ] {
            push_cstr(&mut oack, k);
            push_cstr(&mut oack, &v);
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
}

/// 单请求写设备：OACK → 收块回 ACK，短块结束。收到的数据经 channel 返回。
fn std_write_device() -> (std::net::SocketAddr, std::sync::mpsc::Receiver<Vec<u8>>) {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sock.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 65535];
        let (_n, from) = sock.recv_from(&mut buf).unwrap(); // WRQ（含 tsize）
        let mut oack = vec![0u8, 6];
        for (k, v) in [
            ("blksize", "512".to_string()),
            ("tsize", "0".to_string()),
            ("timeout", "1".to_string()),
            ("windowsize", "1".to_string()),
        ] {
            push_cstr(&mut oack, k);
            push_cstr(&mut oack, &v);
        }
        sock.send_to(&oack, from).unwrap();
        let mut sink = Vec::new();
        loop {
            let (n, _) = sock.recv_from(&mut buf).unwrap();
            if n >= 4 && buf[..2] == [0, 4] {
                continue; // ACK(0)
            }
            if n >= 4 && buf[..2] == [0, 3] {
                let block = u16::from_be_bytes([buf[2], buf[3]]);
                let payload = &buf[4..n];
                let short = payload.len() < 512;
                sink.extend_from_slice(payload);
                let ack = [0, 4, (block >> 8) as u8, (block & 0xff) as u8];
                sock.send_to(&ack, from).unwrap();
                if short {
                    break;
                }
            }
        }
        let _ = tx.send(sink);
    });
    (addr, rx)
}

fn dead_addr() -> std::net::SocketAddr {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let a = s.local_addr().unwrap();
    drop(s);
    a
}

#[test]
fn ndjson_transfers() {
    let tmp = TempDir::new().unwrap();
    let mut s = Session::start(tmp.path());
    let _ = s.read_msg(); // server_ready

    // 1. fetch wait:true：成功 + sha256 + 事件流中的进度/完成
    let data: Vec<u8> = (0..1200u32).map(|i| (i * 13 + 5) as u8).collect();
    let device = std_read_device(data.clone());
    let r = s.result(1, "transfer.fetch", json!({
        "host": "127.0.0.1", "port": device.port(),
        "remote_path": "fw.bin", "local_path": "backup/fw.bin",
        "wait": true, "timeout_ms": 5000,
        "idempotency_key": "e2e-fetch"
    }));
    assert_eq!(r["state"], "success");
    assert_eq!(r["bytes_transferred"], 1200);
    assert_eq!(r["cached"], false);
    assert_eq!(r["sha256"], sha256_hex(&data));

    // 2. 落盘内容核对
    let r2 = s.result(2, "file.read", json!({"path": "backup/fw.bin"}));
    let got = B64.decode(r2["content_base64"].as_str().unwrap()).unwrap();
    assert_eq!(got, data);

    // 3. 幂等缓存命中
    let r3 = s.result(3, "transfer.fetch", json!({
        "host": "127.0.0.1", "port": device.port(),
        "remote_path": "fw.bin", "local_path": "backup/fw.bin",
        "wait": true, "timeout_ms": 5000,
        "idempotency_key": "e2e-fetch"
    }));
    assert_eq!(r3["cached"], true);

    // 4. 同 key 不同参数 → RPC 错误 -32008
    let (code, _) = s.error(4, "transfer.fetch", json!({
        "host": "127.0.0.1", "port": device.port(),
        "remote_path": "OTHER.bin", "local_path": "backup/fw.bin",
        "idempotency_key": "e2e-fetch"
    }));
    assert_eq!(code, -32008);

    // 5. wait:false 异步 + event.transfer_completed
    let device2 = std_read_device(data.clone());
    let r5 = s.result(5, "transfer.fetch", json!({
        "host": "127.0.0.1", "port": device2.port(),
        "remote_path": "fw.bin", "local_path": "backup/fw2.bin",
        "timeout_ms": 5000
    }));
    let task_id = r5["task_id"].as_str().unwrap().to_string();
    let mut completed = None;
    for _ in 0..20 {
        let msg = s.read_msg();
        if msg["method"] == "event.transfer_completed" && msg["params"]["task_id"] == task_id.as_str() {
            completed = Some(msg["params"].clone());
            break;
        }
    }
    let done = completed.expect("event.transfer_completed");
    assert_eq!(done["state"], "success");
    // task.status 亦可见
    let r6 = s.result(6, "task.status", json!({"task_id": task_id}));
    assert_eq!(r6["state"], "success");
    // 终态后 cancel → already_terminal
    let r7 = s.result(7, "task.cancel", json!({"task_id": task_id}));
    assert_eq!(r7["already_terminal"], true);

    // 6. 失败也是 result：死端口 → REMOTE_TIMEOUT。
    //    wait 预算与引擎超时共用 timeout_ms（§4.3），首轮可能返回 running，
    //    用 task.wait 补到终态——这也覆盖了 agent 的恢复路径。
    let dead = dead_addr();
    let resp = s.request(8, "transfer.fetch", json!({
        "host": "127.0.0.1", "port": dead.port(),
        "remote_path": "x", "local_path": "x.bin",
        "wait": true, "timeout_ms": 300, "retries": 1
    }));
    assert!(resp.get("result").is_some(), "failure must be a result, got: {resp}");
    let mut final_state = resp["result"].clone();
    for _ in 0..5 {
        if final_state["state"] != "pending" && final_state["state"] != "running" {
            break;
        }
        let task = final_state["task_id"].as_str().unwrap().to_string();
        final_state = s.result(80, "task.wait", json!({"task_id": task, "timeout_ms": 2000}));
    }
    assert_eq!(final_state["state"], "failed");
    assert_eq!(final_state["error"]["code"], "REMOTE_TIMEOUT");
    assert_eq!(final_state["error"]["retryable"], true);

    // 7. push：成功 + 设备端收到内容
    let push_data: Vec<u8> = (0..700u32).map(|i| (i * 3) as u8).collect();
    let r9 = s.result(9, "file.write", json!({
        "path": "upload/cfg.bin", "content_base64": B64.encode(&push_data)
    }));
    assert_eq!(r9["bytes_written"], 700);
    let (waddr, wrx) = std_write_device();
    let r10 = s.result(10, "transfer.push", json!({
        "host": "127.0.0.1", "port": waddr.port(),
        "remote_path": "cfg.bin", "local_path": "upload/cfg.bin",
        "wait": true, "timeout_ms": 5000
    }));
    assert_eq!(r10["state"], "success", "push failed: {r10:?}");
    assert_eq!(r10["bytes_transferred"], 700);
    let received = wrx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(received, push_data);

    // 8. 优雅退出
    let r = s.result(11, "server.shutdown", json!({}));
    assert_eq!(r["ok"], true);
    let status = s.child.wait().unwrap();
    assert!(status.success());
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
