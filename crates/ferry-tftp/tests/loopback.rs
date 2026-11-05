//! 引擎级 loopback 集成测试：真实 UDP socket（127.0.0.1 随机端口），
//! 覆盖 自有 client↔server 双端、无协商裸客户端、对抗性服务端（重复块）、
//! 错误路径、空文件/整块边界与 16 位块号回绕。

use ferry_tftp::client::{fetch_to_writer, push_from_reader, ClientConfig};
use ferry_tftp::server::{serve_read, serve_write};
use ferry_tftp::{Packet, Request, ServerLimits, TftpError};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u64 * 7 + 13) as u8).collect()
}

fn fast_cfg(block_size: u16, window_size: u16) -> ClientConfig {
    ClientConfig { block_size, window_size, timeout_ms: 1500, retries: 3 }
}

/// 模拟监听器主 socket：收首个 RRQ/WRQ 后交给引擎会话。
async fn recv_request(sock: &UdpSocket) -> (Request, std::net::SocketAddr) {
    let mut buf = vec![0u8; 65535];
    let (n, from) = sock.recv_from(&mut buf).await.unwrap();
    match Packet::decode(&buf[..n]).unwrap() {
        Packet::Rrq(r) | Packet::Wrq(r) => (r, from),
        other => panic!("expected RRQ/WRQ, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_via_own_server_with_options() {
    let data = pattern(10 * 1024 + 137);
    let main = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = main.local_addr().unwrap();
    let cancel = CancellationToken::new();

    let server = tokio::spawn({
        let data = data.clone();
        let cancel = cancel.clone();
        async move {
            let (req, from) = recv_request(&main).await;
            let mut src = std::io::Cursor::new(data.clone());
            serve_read(from, req, &mut src, data.len() as u64, &ServerLimits::default(), &cancel, &mut |_, _| {})
                .await
        }
    });

    let mut sink = Vec::new();
    let report = fetch_to_writer(addr, "fw.bin", fast_cfg(512, 4), &mut sink, &cancel, &mut |_, _| {}).await.unwrap();
    assert_eq!(sink, data);
    assert_eq!(report.bytes, data.len() as u64);
    assert_eq!(report.total_bytes, Some(data.len() as u64)); // tsize 协商生效
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn push_via_own_server_with_options() {
    let data = pattern(3000); // blksize 1024 → 2 整块 + 1 短块
    let main = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = main.local_addr().unwrap();
    let cancel = CancellationToken::new();

    let server = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let (req, from) = recv_request(&main).await;
            let mut sink = Vec::new();
            let report = serve_write(from, req, &mut sink, &ServerLimits::default(), &cancel, &mut |_, _| {})
                .await
                .unwrap();
            (sink, report)
        }
    });

    let mut src = std::io::Cursor::new(data.clone());
    let report = push_from_reader(addr, "cfg.bin", fast_cfg(1024, 2), &mut src, data.len() as u64, &cancel, &mut |_, _| {})
        .await
        .unwrap();
    assert_eq!(report.bytes, data.len() as u64);
    let (sink, server_report) = server.await.unwrap();
    assert_eq!(sink, data);
    assert_eq!(server_report.bytes, data.len() as u64);
    assert_eq!(server_report.total_bytes, Some(data.len() as u64)); // WRQ tsize 回显
}

/// 无选项裸客户端（模拟哑设备）：服务端跳过 OACK 直接发 DATA。
#[tokio::test]
async fn plain_rrq_no_options() {
    let data = pattern(700); // 512 + 188
    let cancel = CancellationToken::new();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer = client.local_addr().unwrap();

    let req = Request { path: "f".into(), mode: ferry_tftp::Mode::Octet, options: vec![] };
    let server = tokio::spawn({
        let data = data.clone();
        let cancel = cancel.clone();
        async move {
            let mut src = std::io::Cursor::new(data);
            serve_read(peer, req, &mut src, 700, &ServerLimits::default(), &cancel, &mut |_, _| {}).await
        }
    });

    // 裸客户端：收 DATA、回 ACK
    let mut sink = Vec::new();
    let mut server_data_port = None;
    for expected_block in 1..=2u16 {
        let (pkt, _from) = loop {
            let mut buf = vec![0u8; 65535];
            let (n, from) = client.recv_from(&mut buf).await.unwrap();
            if server_data_port.is_none() {
                server_data_port = Some(from);
            }
            if server_data_port != Some(from) {
                continue; // 旧会话串扰
            }
            break (Packet::decode(&buf[..n]).unwrap(), from);
        };
        match pkt {
            Packet::Data { block, data: bytes } => {
                assert_eq!(block, expected_block, "首包必须是 DATA(1)（无 OACK）");
                sink.extend_from_slice(&bytes);
                client.send_to(&Packet::Ack { block }.encode(), server_data_port.unwrap()).await.unwrap();
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(sink, data);
    server.await.unwrap().unwrap();
}

/// 对抗性服务端：重复发同一块，客户端必须去重不重写。
#[tokio::test]
async fn duplicate_data_is_deduped() {
    let cancel = CancellationToken::new();
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let saddr = server.local_addr().unwrap();

    let evil = tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        // 收 RRQ
        let (n, from) = server.recv_from(&mut buf).await.unwrap();
        assert!(matches!(Packet::decode(&buf[..n]).unwrap(), Packet::Rrq(_)));
        // OACK（客户端请求了 blksize=512/tsize=0/timeout/windowsize=1）
        let oack = Packet::Oack {
            options: vec![
                ("blksize".into(), "512".into()),
                ("tsize".into(), "513".into()),
                ("timeout".into(), "1".into()),
                ("windowsize".into(), "1".into()),
            ],
        };
        server.send_to(&oack.encode(), from).await.unwrap();
        // 等 ACK(0)
        let (n, _) = server.recv_from(&mut buf).await.unwrap();
        assert!(matches!(Packet::decode(&buf[..n]).unwrap(), Packet::Ack { block: 0 }));
        // DATA(1) × 2（重复），DATA(2) 短块收尾；ACK 全忽略
        let d1 = Packet::Data { block: 1, data: vec![b'A'; 512].into() };
        server.send_to(&d1.encode(), from).await.unwrap();
        server.send_to(&d1.encode(), from).await.unwrap();
        let d2 = Packet::Data { block: 2, data: vec![b'B'].into() };
        server.send_to(&d2.encode(), from).await.unwrap();
        // 排空 ACK（客户端会 ACK(1) 两次 + ACK(2)）
        for _ in 0..3 {
            let _ = tokio::time::timeout(Duration::from_millis(300), server.recv_from(&mut buf)).await;
        }
    });

    let mut sink = Vec::new();
    let report = fetch_to_writer(saddr, "x", fast_cfg(512, 1), &mut sink, &cancel, &mut |_, _| {}).await.unwrap();
    assert_eq!(sink, vec![b'A'; 512].iter().chain(b"B".iter()).copied().collect::<Vec<_>>());
    assert_eq!(report.bytes, 513);
    evil.await.unwrap();
}

/// 远端回 ERROR(1)：客户端必须透传 TFTP 错误码。
#[tokio::test]
async fn remote_error_surfaces() {
    let cancel = CancellationToken::new();
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let saddr = server.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        let (n, from) = server.recv_from(&mut buf).await.unwrap();
        assert!(matches!(Packet::decode(&buf[..n]).unwrap(), Packet::Rrq(_)));
        let err = Packet::Error { code: 1, message: "nope".into() };
        server.send_to(&err.encode(), from).await.unwrap();
    });
    let mut sink = Vec::new();
    let e = fetch_to_writer(saddr, "missing", fast_cfg(512, 1), &mut sink, &cancel, &mut |_, _| {}).await.unwrap_err();
    match e {
        TftpError::Remote { code, name, .. } => {
            assert_eq!(code, 1);
            assert_eq!(name, "FILE_NOT_FOUND");
        }
        other => panic!("expected Remote, got {other:?}"),
    }
}

/// netascii 请求：服务端回 ERROR(4) 并拒绝。
#[tokio::test]
async fn netascii_rejected_with_error_4() {
    let cancel = CancellationToken::new();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer = client.local_addr().unwrap();

    let req = Request { path: "cfg".into(), mode: ferry_tftp::Mode::Netascii, options: vec![] };
    let server = tokio::spawn(async move {
        let mut src = std::io::Cursor::new(vec![1u8, 2, 3]);
        serve_read(peer, req, &mut src, 3, &ServerLimits::default(), &cancel, &mut |_, _| {}).await
    });

    let mut buf = vec![0u8; 65535];
    let (n, _) = client.recv_from(&mut buf).await.unwrap();
    match Packet::decode(&buf[..n]).unwrap() {
        Packet::Error { code, message } => {
            assert_eq!(code, 4);
            assert!(message.contains("netascii"));
        }
        other => panic!("expected ERROR(4), got {other:?}"),
    }
    assert!(matches!(server.await.unwrap(), Err(TftpError::Illegal(_))));
}

/// 空文件 fetch：单个 0 字节 DATA 收尾。
#[tokio::test]
async fn empty_file_fetch() {
    let cancel = CancellationToken::new();
    let main = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = main.local_addr().unwrap();
    let server = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let (req, from) = recv_request(&main).await;
            let mut src = std::io::Cursor::new(Vec::new());
            serve_read(from, req, &mut src, 0, &ServerLimits::default(), &cancel, &mut |_, _| {}).await
        }
    });
    let mut sink = Vec::new();
    let report = fetch_to_writer(addr, "empty", fast_cfg(512, 1), &mut sink, &cancel, &mut |_, _| {}).await.unwrap();
    assert!(sink.is_empty());
    assert_eq!(report.bytes, 0);
    server.await.unwrap().unwrap();
}

/// 文件恰为整数块：push 需补 0 字节末块；服务端 sink 长度精确。
#[tokio::test]
async fn exact_multiple_push_gets_empty_final_block() {
    let data = pattern(1024); // blksize 512 → 恰 2 整块
    let main = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = main.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let server = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let (req, from) = recv_request(&main).await;
            let mut sink = Vec::new();
            let report = serve_write(from, req, &mut sink, &ServerLimits::default(), &cancel, &mut |_, _| {})
                .await
                .unwrap();
            (sink, report)
        }
    });
    let mut src = std::io::Cursor::new(data.clone());
    push_from_reader(addr, "exact", fast_cfg(512, 1), &mut src, 1024, &cancel, &mut |_, _| {}).await.unwrap();
    let (sink, report) = server.await.unwrap();
    assert_eq!(sink, data);
    assert_eq!(report.bytes, 1024);
}

/// 16 位块号回绕：blksize=8、65538 块 → 覆盖 65535→0→1 回绕点。
#[tokio::test]
async fn block_number_wraps_at_65535() {
    let blocks: usize = 65538;
    let data = pattern(blocks * 8);
    let cancel = CancellationToken::new();
    let main = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = main.local_addr().unwrap();
    let server = tokio::spawn({
        let data = data.clone();
        let cancel = cancel.clone();
        async move {
            let (req, from) = recv_request(&main).await;
            let mut src = std::io::Cursor::new(data);
            serve_read(from, req, &mut src, (blocks * 8) as u64, &ServerLimits::default(), &cancel, &mut |_, _| {}).await
        }
    });
    let mut sink = Vec::new();
    let report = fetch_to_writer(addr, "wrap", fast_cfg(8, 16), &mut sink, &cancel, &mut |_, _| {}).await.unwrap();
    assert_eq!(sink.len(), blocks * 8);
    assert_eq!(sink, data);
    assert_eq!(report.bytes, (blocks * 8) as u64);
    server.await.unwrap().unwrap();
}

/// 不可达端口：握手重试耗尽 → Timeout（快速超时配置）。
#[tokio::test]
async fn dead_peer_times_out() {
    let cancel = CancellationToken::new();
    // 绑定后立刻丢弃的端口（绑定即释放，几乎必然无主）
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let mut sink = Vec::new();
    let cfg = ClientConfig { block_size: 512, window_size: 1, timeout_ms: 100, retries: 2 };
    let e = fetch_to_writer(addr, "x", cfg, &mut sink, &cancel, &mut |_, _| {}).await.unwrap_err();
    assert!(matches!(e, TftpError::Timeout { .. }));
}
