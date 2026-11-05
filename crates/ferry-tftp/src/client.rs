//! 主动传输（ferry 作为 TFTP 客户端）：fetch = RRQ，push = WRQ。
//!
//! 握手统一走选项协商：RRQ/WRQ 附带 blksize/tsize/timeout/windowsize，
//! 对端回 OACK 则采用（回 ACK(0) 确认）后开始数据；对端直接回 DATA(1)/ACK(0)
//! 视为未协商，退回默认参数（512 / 窗口 1）。
//!
//! 数据通道地址：TFTP 服务端从新的临时端口应答，客户端首个响应确定该地址
//! 并锁定（后续忽略其它来源，防旧会话串扰）。

use std::net::SocketAddr;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::error::TftpError;
use crate::options::{self, Options};
use crate::packet::{Mode, Packet, Request};
use crate::session::{recv_data, send_data, Session};

/// 客户端传输参数（协议 transfer.fetch/push 的引擎投影）。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub block_size: u16,
    pub window_size: u16,
    pub timeout_ms: u64,
    pub retries: u32,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self { block_size: 512, window_size: 1, timeout_ms: 5000, retries: 3 }
    }
}

#[derive(Debug, Clone)]
pub struct TransferReport {
    pub bytes: u64,
    /// 对端通过 tsize 声明的总大小（fetch）。
    pub total_bytes: Option<u64>,
}

/// 发起首个请求包并完成握手。返回 (会话, 会话参数, 接收循环的 pending 首包)。
async fn handshake(
    mut sess: Session,
    server: SocketAddr,
    first: Packet,
    requested: options::OptPairs,
    cfg: &ClientConfig,
) -> Result<(Session, Options, Option<Packet>), TftpError> {
    let mut attempts = cfg.retries.max(1);
    let (resp, from) = loop {
        sess.send_to(&first, server).await?;
        match sess.recv(cfg.timeout_ms).await? {
            Some((pkt, from)) => break (pkt, from),
            None => {
                attempts -= 1;
                if attempts == 0 {
                    return Err(TftpError::Timeout { retries: cfg.retries });
                }
            }
        }
    };
    sess.lock_peer(from);

    match resp {
        Packet::Oack { options: oack } => {
            let opts = match options::client_adopt(oack, &requested) {
                Ok(o) => o,
                Err(e) => {
                    sess.send(&Packet::Error { code: 8, message: e.to_string() }).await?;
                    return Err(e);
                }
            };
            // RRQ/WRQ + OACK：客户端 ACK(0) 确认后进入数据阶段（RFC 7440 Figure 1）
            sess.send(&Packet::Ack { block: 0 }).await?;
            Ok((sess, opts, None))
        }
        // 对端不协商：RRQ → DATA(1) 直入接收；WRQ → ACK(0) 直入发送
        Packet::Data { block: 1, data } => Ok((sess, Options::default(), Some(Packet::Data { block: 1, data }))),
        Packet::Ack { block: 0 } => Ok((sess, Options::default(), None)),
        Packet::Error { code, message } => Err(TftpError::Remote {
            code,
            name: crate::error::error_name(code),
            message,
        }),
        other => Err(TftpError::Illegal(format!("unexpected handshake packet: {other:?}"))),
    }
}

/// RRQ：远端 → 本地 sink。
pub async fn fetch_to_writer(
    server: SocketAddr,
    remote_path: &str,
    cfg: ClientConfig,
    sink: &mut (impl AsyncWrite + Unpin),
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<TransferReport, TftpError> {
    let sess = Session::bind_ephemeral(server).await?;
    let requested = options::client_request(cfg.block_size, cfg.window_size, cfg.timeout_ms, Some(0));
    let req = Request { path: remote_path.to_string(), mode: Mode::Octet, options: requested.clone() };
    let (mut sess, mut opts, pending) = handshake(sess, server, Packet::Rrq(req), requested, &cfg).await?;
    let total_bytes = opts.tsize;
    opts.retries = cfg.retries;
    let bytes = recv_data(&mut sess, &opts, pending, sink, cancel, progress).await?;
    sink.flush().await?;
    Ok(TransferReport { bytes, total_bytes })
}

/// WRQ：本地 source（`len` 字节）→ 远端。
pub async fn push_from_reader(
    server: SocketAddr,
    remote_path: &str,
    cfg: ClientConfig,
    source: &mut (impl AsyncRead + Unpin),
    len: u64,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<TransferReport, TftpError> {
    let sess = Session::bind_ephemeral(server).await?;
    let requested = options::client_request(cfg.block_size, cfg.window_size, cfg.timeout_ms, Some(len));
    let req = Request { path: remote_path.to_string(), mode: Mode::Octet, options: requested.clone() };
    let (mut sess, mut opts, _pending) = handshake(sess, server, Packet::Wrq(req), requested, &cfg).await?;
    let total_bytes = opts.tsize;
    opts.retries = cfg.retries;
    let bytes = send_data(&mut sess, &opts, source, cancel, progress).await?;
    Ok(TransferReport { bytes, total_bytes })
}
