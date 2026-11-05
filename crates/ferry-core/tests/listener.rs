//! listener.* 集成测试：ferry 同时充当 TFTP 服务端（监听器）与客户端，
//! loopback 全链路（fetch 拉自己的 listener / push 推给 listener），
//! 以及 allowlist / 路径穿越 / 未授权操作 / 并发闸 / drain。

use ferry_core::api::{Api, ApiConfig};
use ferry_core::params::TaskState;
use ferry_core::{ErrCode, Event};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u64 * 17 + 3) as u8).collect()
}

async fn test_api(dir: &std::path::Path) -> Api {
    Api::new(ApiConfig {
        sandbox_root: dir.to_path_buf(),
        quota_bytes: 1 << 20,
        max_concurrent_tasks: 8,
    })
    .unwrap()
}

fn start_params(port: u16, read: bool, write: bool) -> serde_json::Value {
    serde_json::json!({ "port": port, "allow": { "read": read, "write": write } })
}

async fn wait_terminal(api: &Api, task_id: &str) -> ferry_core::params::TransferResult {
    api.task_wait_raw(task_id, Some(Duration::from_secs(10))).await.unwrap()
}

/// device fetch：从 listener 拉指定文件，返回最终结果。
async fn device_fetch(api: &Api, port: u16, remote: &str, local: &str) -> ferry_core::params::TransferResult {
    let start = api
        .transfer_fetch(serde_json::from_value(serde_json::json!({
            "host": "127.0.0.1", "port": port,
            "remote_path": remote, "local_path": local,
            "timeout_ms": 1500, "retries": 2,
        }))
        .unwrap())
        .await
        .unwrap();
    wait_terminal(api, &start.task_id).await
}

/// device push：向 listener 写指定文件。
async fn device_push(api: &Api, port: u16, remote: &str, local: &str) -> ferry_core::params::TransferResult {
    let start = api
        .transfer_push(serde_json::from_value(serde_json::json!({
            "host": "127.0.0.1", "port": port,
            "remote_path": remote, "local_path": local,
            "timeout_ms": 1500, "retries": 2,
        }))
        .unwrap())
        .await
        .unwrap();
    wait_terminal(api, &start.task_id).await
}

#[tokio::test]
async fn listener_read_serves_sandbox_file() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path()).await;
    let data = pattern(1500);
    std::fs::create_dir_all(tmp.path().join("fw/v1")).unwrap();
    std::fs::write(tmp.path().join("fw").join("v1").join("fw.bin"), &data).unwrap();

    let mut rx = api.events();
    let r = api
        .listener_start(serde_json::from_value(serde_json::json!({
            "port": 0, "root": "fw", "allow": { "read": true, "write": false }
        }))
        .unwrap())
        .await
        .unwrap();
    assert_eq!(r.state, "listening");
    assert!(r.port > 0);

    let result = device_fetch(&api, r.port, "v1/fw.bin", "got/fw.bin").await;
    assert_eq!(result.state, TaskState::Success, "err: {:?}", result.error);
    assert_eq!(result.bytes_transferred, 1500);
    assert_eq!(std::fs::read(tmp.path().join("got/fw.bin")).unwrap(), data);

    // InboundRequest 事件
    let mut inbound = None;
    while let Ok(ev) = rx.try_recv() {
        if let Event::InboundRequest { operation, path, listener_id, task_id, .. } = ev {
            inbound = Some((operation, path, listener_id, task_id));
        }
    }
    let (op, path, lid, tid) = inbound.expect("event.inbound_request");
    assert_eq!(op, "read");
    assert_eq!(path, "v1/fw.bin");
    assert_eq!(lid, "L-01");
    // inbound 任务进入统一任务空间
    let status = api
        .task_status(serde_json::from_value(serde_json::json!({ "task_id": tid })).unwrap())
        .await
        .unwrap();
    assert_eq!(status.state, TaskState::Success);
}

#[tokio::test]
async fn listener_write_lands_in_incoming() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path()).await;
    let data = pattern(800);
    std::fs::write(tmp.path().join("out.bin"), &data).unwrap();

    let r = api
        .listener_start(serde_json::from_value(start_params(0, false, true)).unwrap())
        .await
        .unwrap();
    let result = device_push(&api, r.port, "backup/cfg.bin", "out.bin").await;
    assert_eq!(result.state, TaskState::Success, "err: {:?}", result.error);
    assert_eq!(result.bytes_transferred, 800);
    // 客户端成功（收到末块 ACK）与服务端 rename 落盘之间存在微小窗口，轮询等待
    let final_path = tmp.path().join("incoming/backup/cfg.bin");
    let landed = {
        let mut got = None;
        for _ in 0..60 {
            if let Ok(v) = std::fs::read(&final_path) {
                got = Some(v);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        got.unwrap_or_else(|| {
            panic!(
                "upload never landed at {} (tree: {:?})",
                final_path.display(),
                std::fs::read_dir(tmp.path().join("incoming/backup")).map(|rd| {
                    rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>()
                })
            )
        })
    };
    assert_eq!(landed, data);
    // sha256 = 上传内容摘要
    use sha2::{Digest, Sha256};
    let expected: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(result.sha256.as_deref(), Some(expected.as_str()));
    // 无 .part 残留
    assert!(std::fs::read_dir(tmp.path().join("incoming/backup")).unwrap().count() == 1);
}

#[tokio::test]
async fn write_rejected_when_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path()).await;
    std::fs::write(tmp.path().join("out.bin"), b"x").unwrap();
    let r = api
        .listener_start(serde_json::from_value(start_params(0, true, false)).unwrap())
        .await
        .unwrap();
    let result = device_push(&api, r.port, "cfg.bin", "out.bin").await;
    assert_eq!(result.state, TaskState::Failed);
    let err = result.error.unwrap();
    assert_eq!(err.code, "REMOTE_ERROR");
    assert_eq!(err.details["tftp_error_code"], 2);
}

#[tokio::test]
async fn allowlist_rejects_other_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path()).await;
    let data = pattern(64);
    std::fs::write(tmp.path().join("fw.bin"), &data).unwrap();
    // 只允许 10.0.0.0/8 —— 本机 127.0.0.1 会被拒
    let r = api
        .listener_start(serde_json::from_value(serde_json::json!({
            "port": 0, "allow": { "read": true },
            "allowlist": ["10.0.0.0/8"]
        }))
        .unwrap())
        .await
        .unwrap();
    let result = device_fetch(&api, r.port, "fw.bin", "x.bin").await;
    assert_eq!(result.state, TaskState::Failed);
    let err = result.error.unwrap();
    assert_eq!(err.code, "REMOTE_ERROR");
    assert_eq!(err.details["tftp_error_code"], 2);
}

#[tokio::test]
async fn path_traversal_rejected_at_tftp_layer() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path()).await;
    let secret = tmp.path().parent().unwrap().join("ferry-secret.txt");
    std::fs::write(&secret, b"top secret").unwrap();
    std::fs::write(tmp.path().join("fw.bin"), pattern(64)).unwrap();

    let r = api
        .listener_start(serde_json::from_value(start_params(0, true, false)).unwrap())
        .await
        .unwrap();
    let result = device_fetch(&api, r.port, "../../ferry-secret.txt", "leak.bin").await;
    assert_eq!(result.state, TaskState::Failed);
    assert_eq!(result.error.unwrap().details["tftp_error_code"], 2);
    assert!(!tmp.path().join("leak.bin").exists());
}

#[tokio::test]
async fn missing_file_and_netascii_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path()).await;
    let r = api
        .listener_start(serde_json::from_value(start_params(0, true, false)).unwrap())
        .await
        .unwrap();
    // 缺文件 → ERROR(1)
    let result = device_fetch(&api, r.port, "ghost.bin", "g.bin").await;
    assert_eq!(result.state, TaskState::Failed);
    assert_eq!(result.error.unwrap().details["tftp_error_code"], 1);

    // netascii → ERROR(4)（引擎层拒绝）
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let req: Vec<u8> = {
        let mut v = vec![0, 1];
        v.extend_from_slice(b"net-file\0netascii\0");
        v
    };
    sock.send_to(&req, format!("127.0.0.1:{}", r.port)).await.unwrap();
    let mut buf = vec![0u8; 1024];
    let (n, _) = sock.recv_from(&mut buf).await.unwrap();
    let pkt = ferry_tftp::Packet::decode(&buf[..n]).unwrap();
    assert!(matches!(pkt, ferry_tftp::Packet::Error { code: 4, .. }), "got {pkt:?}");
}

#[tokio::test]
async fn max_concurrent_rejects_with_busy() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path()).await;
    std::fs::write(tmp.path().join("big.bin"), pattern(64)).unwrap();
    let r = api
        .listener_start(serde_json::from_value(serde_json::json!({
            "port": 0, "allow": { "read": true, "write": true },
            "max_concurrent": 1
        }))
        .unwrap())
        .await
        .unwrap();

    // 占位客户端：WRQ + OACK + ACK(0) 后不发数据，吃掉唯一并发额度
    let holder = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let h = holder.clone();
    let stall = tokio::spawn(async move {
        fn put(v: &mut Vec<u8>, s: &str) {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        let mut wrq = vec![0u8, 2];
        put(&mut wrq, "hold.bin");
        put(&mut wrq, "octet");
        put(&mut wrq, "blksize");
        put(&mut wrq, "512");
        put(&mut wrq, "tsize");
        put(&mut wrq, "100");
        put(&mut wrq, "timeout");
        put(&mut wrq, "1");
        put(&mut wrq, "windowsize");
        put(&mut wrq, "1");
        h.send_to(&wrq, format!("127.0.0.1:{}", r.port)).await.unwrap();
        let mut buf = vec![0u8; 2048];
        let (n, _) = h.recv_from(&mut buf).await.unwrap();
        assert!(matches!(ferry_tftp::Packet::decode(&buf[..n]).unwrap(), ferry_tftp::Packet::Oack { .. }));
        // ACK(0) 后挂起
        h.send_to(&[0, 4, 0, 0], format!("127.0.0.1:{}", r.port)).await.unwrap();
        std::future::pending::<()>().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 第二个请求：被并发闸拒绝（ERROR 0）
    let result = device_fetch(&api, r.port, "big.bin", "b.bin").await;
    assert_eq!(result.state, TaskState::Failed);
    assert_eq!(result.error.unwrap().details["tftp_error_code"], 0);
    stall.abort();
}

#[tokio::test]
async fn stop_and_status_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path()).await;
    let r = api
        .listener_start(serde_json::from_value(start_params(0, true, true)).unwrap())
        .await
        .unwrap();

    // status
    let info = api
        .listener_status(serde_json::from_value(serde_json::json!({ "listener_id": r.listener_id })).unwrap())
        .await
        .unwrap();
    assert_eq!(info.state, "listening");
    assert_eq!(info.mode, vec!["read".to_string(), "write".to_string()]);

    // server.status 里可见
    let status = api.server_status().await.unwrap();
    assert_eq!(status.listeners.len(), 1);
    assert_eq!(status.listeners[0].port, r.port);

    // stop
    let stop = api
        .listener_stop(serde_json::from_value(serde_json::json!({
            "listener_id": r.listener_id, "drain": true
        }))
        .unwrap())
        .await
        .unwrap();
    assert!(stop.stopped);
    // 再 stop / status → INVALID_STATE
    let e = api
        .listener_stop(serde_json::from_value(serde_json::json!({ "listener_id": r.listener_id })).unwrap())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrCode::InvalidState);
    let e = api
        .listener_status(serde_json::from_value(serde_json::json!({ "listener_id": r.listener_id })).unwrap())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrCode::InvalidState);

    // 端口已释放（能重新绑定）
    let r2 = api
        .listener_start(serde_json::from_value(serde_json::json!({
            "port": r.port, "allow": { "read": true }
        }))
        .unwrap())
        .await
        .unwrap();
    assert_eq!(r2.port, r.port);
}

#[tokio::test]
async fn bad_params_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path()).await;
    // 非法 CIDR
    let e = api
        .listener_start(serde_json::from_value(serde_json::json!({
            "port": 0, "allow": { "read": true }, "allowlist": ["not-a-cidr"]
        }))
        .unwrap())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrCode::InvalidParams);
    // 根目录越界
    let e = api
        .listener_start(serde_json::from_value(serde_json::json!({
            "port": 0, "allow": { "read": true }, "root": "../escape"
        }))
        .unwrap())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrCode::SandboxViolation);
    // 两个 allow 都关
    let e = api
        .listener_start(serde_json::from_value(start_params(0, false, false)).unwrap())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrCode::InvalidParams);
}
