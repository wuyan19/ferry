//! core 级传输集成测试：Api ↔ TFTP 引擎 loopback。
//! 覆盖 fetch/push 成功、失败分层（result 而非 RPC error）、取消、
//! 幂等（在途去重/终态缓存/冲突）、配额中断、临时文件清理。

use ferry_core::api::{Api, ApiConfig};
use ferry_core::params::{TaskState, TransferParams};
use ferry_core::{ErrCode, Event};
use ferry_tftp::{serve_read, serve_write, Packet, ServerLimits};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u64 * 31 + 7) as u8).collect()
}

async fn test_api(dir: &std::path::Path, quota: u64) -> Api {
    Api::new(ApiConfig {
        sandbox_root: dir.to_path_buf(),
        quota_bytes: quota,
        max_concurrent_tasks: 4,
    })
    .unwrap()
}

fn fetch_params(addr: std::net::SocketAddr, local: &str) -> TransferParams {
    serde_json::from_value(serde_json::json!({
        "host": addr.ip().to_string(),
        "port": addr.port(),
        "remote_path": "fw.bin",
        "local_path": local,
        "timeout_ms": 1500,
        "retries": 2,
    }))
    .unwrap()
}

/// 引擎作为"设备"读服务（单请求）。返回 (addr, 句柄)。
async fn device_read(data: Vec<u8>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        let (n, from) = sock.recv_from(&mut buf).await.unwrap();
        let req = match Packet::decode(&buf[..n]).unwrap() {
            Packet::Rrq(r) => r,
            other => panic!("{other:?}"),
        };
        let len = data.len() as u64;
        let mut src = std::io::Cursor::new(data);
        // 对端中途退出（如配额中断）会让会话超时——设备侧不视为错误
        let _ = serve_read(from, req, &mut src, len, &ServerLimits::default(), &cancel, &mut |_, _| {}).await;
    });
    (addr, handle)
}

/// 挂起服务端：OACK + DATA(1) 后停住（任务永远在途）。统计 RRQ 数。
async fn stalling_device() -> (std::net::SocketAddr, Arc<AtomicU32>) {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = sock.local_addr().unwrap();
    let rrq_count = Arc::new(AtomicU32::new(0));
    let cnt = rrq_count.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        let (_n, from) = sock.recv_from(&mut buf).await.unwrap();
        cnt.fetch_add(1, Ordering::SeqCst);
        let oack = Packet::Oack {
            options: vec![
                ("blksize".into(), "512".into()),
                ("tsize".into(), "4096".into()),
                ("timeout".into(), "1".into()),
                ("windowsize".into(), "1".into()),
            ],
        };
        sock.send_to(&oack.encode(), from).await.unwrap();
        let _ = sock.recv_from(&mut buf).await.unwrap(); // ACK(0)
        let d = Packet::Data { block: 1, data: vec![0u8; 512].into() };
        sock.send_to(&d.encode(), from).await.unwrap();
        std::future::pending::<()>().await;
    });
    (addr, rrq_count)
}

async fn wait_terminal(api: &Api, task_id: &str) -> ferry_core::params::TransferResult {
    api.task_wait_raw(task_id, Some(Duration::from_secs(10))).await.unwrap()
}

#[tokio::test]
async fn fetch_success_lands_file_and_emits_events() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path(), 1 << 20).await;
    let data = pattern(1200);
    let (addr, device) = device_read(data.clone()).await;
    let mut rx = api.events();

    let start = api
        .transfer_fetch(fetch_params(addr, "devices/fw.bin"))
        .await
        .unwrap();
    assert_eq!(start.snapshot.state, TaskState::Pending);
    let result = wait_terminal(&api, &start.task_id).await;

    assert_eq!(result.state, TaskState::Success);
    assert_eq!(result.bytes_transferred, 1200);
    let content = std::fs::read(tmp.path().join("devices/fw.bin")).unwrap();
    assert_eq!(content, data);
    use sha2::{Digest, Sha256};
    let expected: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(result.sha256.as_deref(), Some(expected.as_str()));

    // 事件：TransferCompleted 到达
    let mut got_completed = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, Event::TransferCompleted { .. }) {
            got_completed = true;
        }
    }
    assert!(got_completed);
    device.await.unwrap();
}

#[tokio::test]
async fn push_success_hashes_content() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path(), 1 << 20).await;
    let data = pattern(700);
    std::fs::write(tmp.path().join("out.bin"), &data).unwrap();

    // 设备 = serve_write
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let device = tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        let (n, from) = sock.recv_from(&mut buf).await.unwrap();
        let req = match Packet::decode(&buf[..n]).unwrap() {
            Packet::Wrq(r) => r,
            other => panic!("{other:?}"),
        };
        let mut sink = Vec::new();
        serve_write(from, req, &mut sink, &ServerLimits::default(), &cancel, &mut |_, _| {})
            .await
            .unwrap();
        sink
    });

    let p = serde_json::from_value(serde_json::json!({
        "host": addr.ip().to_string(), "port": addr.port(),
        "remote_path": "cfg.bin", "local_path": "out.bin",
        "timeout_ms": 1500, "retries": 2,
    }))
    .unwrap();
    let start = api.transfer_push(p).await.unwrap();
    let result = wait_terminal(&api, &start.task_id).await;
    assert_eq!(result.state, TaskState::Success);
    assert_eq!(result.bytes_transferred, 700);
    let sink = device.await.unwrap();
    assert_eq!(sink, data);
}

#[tokio::test]
async fn transfer_failure_is_task_level_not_rpc() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path(), 1 << 20).await;
    // 死端口：握手超时
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dead = probe.local_addr().unwrap();
    drop(probe);

    let mut p = fetch_params(dead, "x.bin");
    p.timeout_ms = Some(300);
    p.retries = Some(1);
    let start = api.transfer_fetch(p).await.unwrap(); // RPC 层成功
    let result = wait_terminal(&api, &start.task_id).await;
    assert_eq!(result.state, TaskState::Failed);
    let err = result.error.unwrap();
    assert_eq!(err.code, "REMOTE_TIMEOUT");
    assert!(err.retryable);
}

#[tokio::test]
async fn bad_host_maps_to_network_unreachable() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path(), 1 << 20).await;
    let p = serde_json::from_value(serde_json::json!({
        "host": "999.999.999.999", "remote_path": "a", "local_path": "a.bin",
        "timeout_ms": 500, "retries": 1,
    }))
    .unwrap();
    let start = api.transfer_fetch(p).await.unwrap();
    let result = wait_terminal(&api, &start.task_id).await;
    assert_eq!(result.state, TaskState::Failed);
    assert_eq!(result.error.unwrap().code, "NETWORK_UNREACHABLE");
}

#[tokio::test]
async fn cancel_aborts_and_cleans_temp() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path(), 1 << 20).await;
    let (addr, _cnt) = stalling_device().await;

    let start = api
        .transfer_fetch(fetch_params(addr, "half.bin"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cancel = api
        .task_cancel(serde_json::from_value(serde_json::json!({ "task_id": start.task_id })).unwrap())
        .await
        .unwrap();
    assert!(!cancel.already_terminal);
    let result = cancel.result;
    assert_eq!(result.state, TaskState::Aborted);
    assert!(result.error.is_none());

    // 无 .part 残留、无半截文件
    let entries = std::fs::read_dir(tmp.path()).unwrap().count();
    assert_eq!(entries, 0);

    // 幂等取消：再次 cancel 返回终态
    let again = api
        .task_cancel(serde_json::from_value(serde_json::json!({ "task_id": start.task_id })).unwrap())
        .await
        .unwrap();
    assert!(again.already_terminal);
}

#[tokio::test]
async fn idempotency_inflight_dedup_and_cached_result() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path(), 1 << 20).await;

    // 在途去重
    let (addr, rrq) = stalling_device().await;
    let mut p = fetch_params(addr, "a.bin");
    p.idempotency_key = Some("k1".into());
    let s1 = api.transfer_fetch(p.clone()).await.unwrap();
    let s2 = api.transfer_fetch(p.clone()).await.unwrap();
    assert_eq!(s1.task_id, s2.task_id);
    assert!(!s2.snapshot.state.is_terminal());
    // 等首个 RRQ 到达设备端，再确认没有第二个
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(rrq.load(Ordering::SeqCst), 1); // 无第二次 RRQ
    let _ = api
        .task_cancel(serde_json::from_value(serde_json::json!({ "task_id": s1.task_id })).unwrap())
        .await;

    // 终态缓存
    let data = pattern(900);
    let (addr2, device) = device_read(data).await;
    let mut p2 = fetch_params(addr2, "b.bin");
    p2.idempotency_key = Some("k2".into());
    let s3 = api.transfer_fetch(p2.clone()).await.unwrap();
    let r1 = wait_terminal(&api, &s3.task_id).await;
    assert_eq!(r1.state, TaskState::Success);
    let s4 = api.transfer_fetch(p2.clone()).await.unwrap();
    assert_eq!(s4.task_id, s3.task_id);
    assert_eq!(s4.snapshot.cached, Some(true));
    assert_eq!(s4.snapshot.state, TaskState::Success);
    device.await.unwrap();

    // 同 key 不同参数 → 冲突（RPC 层错误）
    let mut p3 = p2.clone();
    p3.remote_path = "other.bin".into();
    let err = api.transfer_fetch(p3).await.unwrap_err();
    assert_eq!(err.code, ErrCode::IdempotencyConflict);
}

#[tokio::test]
async fn quota_exceeded_fails_task_and_leaves_no_file() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path(), 100).await; // 配额 100B
    let data = pattern(500);
    let (addr, device) = device_read(data).await;
    let start = api
        .transfer_fetch(fetch_params(addr, "big.bin"))
        .await
        .unwrap();
    let result = wait_terminal(&api, &start.task_id).await;
    assert_eq!(result.state, TaskState::Failed);
    assert_eq!(result.error.unwrap().code, "QUOTA_EXCEEDED");
    assert!(!tmp.path().join("big.bin").exists());
    let entries = std::fs::read_dir(tmp.path()).unwrap().count();
    assert_eq!(entries, 0);
    device.await.unwrap();
}

#[tokio::test]
async fn push_missing_source_is_rpc_error() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path(), 1 << 20).await;
    let p = serde_json::from_value(serde_json::json!({
        "host": "127.0.0.1", "port": 1, "remote_path": "r", "local_path": "missing.bin",
    }))
    .unwrap();
    let err = api.transfer_push(p).await.unwrap_err();
    assert_eq!(err.code, ErrCode::FileNotFoundError);
}

#[tokio::test]
async fn task_status_and_wait_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let api = test_api(tmp.path(), 1 << 20).await;
    let data = pattern(300);
    let (addr, device) = device_read(data).await;
    let start = api
        .transfer_fetch(fetch_params(addr, "c.bin"))
        .await
        .unwrap();

    // task.wait 带超时（小文件很快完成）
    let r = api
        .task_wait(serde_json::from_value(serde_json::json!({
            "task_id": start.task_id, "timeout_ms": 5000
        })).unwrap())
        .await
        .unwrap();
    assert_eq!(r.state, TaskState::Success);

    // task.status 快照
    let s = api
        .task_status(serde_json::from_value(serde_json::json!({ "task_id": start.task_id })).unwrap())
        .await
        .unwrap();
    assert_eq!(s.state, TaskState::Success);

    // 未知任务
    let e = api
        .task_status(serde_json::from_value(serde_json::json!({ "task_id": "T-dead" })).unwrap())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrCode::TaskNotFound);
    device.await.unwrap();
}
