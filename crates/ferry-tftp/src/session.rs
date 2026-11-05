//! 会话原语：UDP 会话 socket + 共享的数据收发循环。
//!
//! - 发送方（客户端 push / 服务端 read）：`send_data` —— 窗口化发送 + 超时重传未确认窗口；
//! - 接收方（客户端 fetch / 服务端 write）：`recv_data` —— 按窗口节流 ACK、
//!   重复块重 ACK 不重写（巫师学徒综合征防护）、16 位块号回绕。
//!
//! 对端锁定：客户端首个响应确定数据通道地址（TFTP 服务端从新端口应答），
//! 此后忽略其它来源；服务端会话自创建起锁定发起方。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use crate::error::TftpError;
use crate::options::Options;
use crate::packet::Packet;

pub struct Session {
    sock: UdpSocket,
    peer: Option<SocketAddr>,
    buf: Vec<u8>,
}

impl Session {
    /// 绑定与 `family_of` 同族的临时端口。
    pub async fn bind_ephemeral(family_of: SocketAddr) -> std::io::Result<Self> {
        let bind = if family_of.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
        Ok(Self {
            sock: UdpSocket::bind(bind).await?,
            peer: None,
            buf: vec![0u8; crate::packet::MAX_DATAGRAM],
        })
    }

    /// 锁定对端（数据通道地址）。之后 `recv` 只接受该地址。
    pub fn lock_peer(&mut self, peer: SocketAddr) {
        self.peer = Some(peer);
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer.expect("peer locked")
    }

    pub async fn send_to(&self, pkt: &Packet, to: SocketAddr) -> Result<(), TftpError> {
        self.sock.send_to(&pkt.encode(), to).await?;
        Ok(())
    }

    pub async fn send(&self, pkt: &Packet) -> Result<(), TftpError> {
        self.send_to(pkt, self.peer()).await
    }

    /// 收包（带单次超时预算），返回 (包, 来源地址)。
    /// 返回 `Ok(None)` = 超时；已锁对端时静默丢弃其它来源/坏包；
    /// Windows UDP 的 ConnectionReset（ICMP 不可达回弹）按超时处理。
    pub async fn recv(&mut self, timeout_ms: u64) -> Result<Option<(Packet, SocketAddr)>, TftpError> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match tokio::time::timeout(remaining, self.sock.recv_from(&mut self.buf)).await {
                Err(_) => return Ok(None),
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Ok(Err(e)) => return Err(e.into()),
                Ok(Ok((n, from))) => {
                    if let Some(p) = self.peer
                        && p != from {
                            continue; // 延迟的旧会话包
                        }
                    match Packet::decode(&self.buf[..n]) {
                        Ok(pkt) => return Ok(Some((pkt, from))),
                        Err(_) => continue, // 坏包：等待下一个
                    }
                }
            }
        }
    }
}

/// 从 reader 精读至多 `size` 字节（EOF 提前截断）。
async fn read_chunk(reader: &mut (impl AsyncRead + Unpin), size: usize) -> std::io::Result<Bytes> {
    let mut buf = vec![0u8; size];
    let mut filled = 0usize;
    while filled < size {
        let n = reader.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(Bytes::from(buf))
}

/// 发送方循环：读 source → 窗口化发 DATA → 收 ACK 推进。
/// 末块短于 blksize 标记结束；文件恰为整数块或为空时补一个 0 字节末块。
pub(crate) async fn send_data(
    sess: &mut Session,
    opts: &Options,
    total_hint: Option<u64>,
    reader: &mut (impl AsyncRead + Unpin),
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<u64, TftpError> {
    let blksize = opts.blksize as usize;
    let window = opts.windowsize.max(1) as usize;
    let mut inflight: VecDeque<(u16, Bytes)> = VecDeque::new();
    let mut last_acked: u16 = 0;
    let mut next_block: u16 = 1;
    let mut total: u64 = 0;
    let mut eof = false;
    let mut attempts = opts.retries.max(1);

    loop {
        if cancel.is_cancelled() {
            return Err(TftpError::Cancelled);
        }
        // 填窗口
        while !eof && inflight.len() < window {
            let chunk = read_chunk(reader, blksize).await?;
            if chunk.len() < blksize {
                eof = true;
                if chunk.is_empty() {
                    // EOF：若此前最后一块是整块（或空文件）→ 补 0 字节末块
                    let need_empty = inflight.back().is_none_or(|(_, b)| b.len() == blksize);
                    if need_empty {
                        sess.send(&Packet::Data { block: next_block, data: Bytes::new() }).await?;
                        inflight.push_back((next_block, Bytes::new()));
                        next_block = next_block.wrapping_add(1);
                    }
                    break;
                }
            }
            sess.send(&Packet::Data { block: next_block, data: chunk.clone() }).await?;
            total += chunk.len() as u64;
            progress(total, total_hint);
            inflight.push_back((next_block, chunk));
            next_block = next_block.wrapping_add(1);
        }
        if eof && inflight.is_empty() {
            return Ok(total);
        }
        // 等 ACK
        match sess.recv(opts.timeout_ms).await? {
            Some((Packet::Ack { block }, _)) => {
                let dist = block.wrapping_sub(last_acked) as usize;
                if dist == 0 {
                    continue; // 重复 ACK：忽略（防巫师学徒）
                }
                if dist <= inflight.len() {
                    for _ in 0..dist {
                        inflight.pop_front();
                    }
                    last_acked = block;
                    attempts = opts.retries.max(1);
                }
                // 超前 ACK：协议违例，忽略等待重传
            }
            Some((Packet::Error { code, message }, _)) => {
                return Err(TftpError::Remote {
                    code,
                    name: crate::error::error_name(code),
                    message,
                });
            }
            Some(_) => {}
            None => {
                attempts -= 1;
                if attempts == 0 {
                    return Err(TftpError::Timeout { retries: opts.retries });
                }
                // 重传整个未确认窗口
                for (block, data) in &inflight {
                    sess.send(&Packet::Data { block: *block, data: data.clone() }).await?;
                }
            }
        }
    }
}

/// 接收方循环：收 DATA → 写 sink → 按窗口节流 ACK。
/// `pending` 为握手期间已收到的首包（DATA(1)，无协商场景）。
pub(crate) async fn recv_data(
    sess: &mut Session,
    opts: &Options,
    mut pending: Option<Packet>,
    sink: &mut (impl AsyncWrite + Unpin),
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<u64, TftpError> {
    let blksize = opts.blksize as usize;
    let window = opts.windowsize.max(1) as usize;
    let total_hint = opts.tsize; // fetch: OACK tsize；serve_write: WRQ 声明回显
    let mut expected: u16 = 1;
    let mut last_acked: u16 = 0;
    let mut since_ack = 0usize;
    let mut total: u64 = 0;
    let mut attempts = opts.retries.max(1);

    loop {
        if cancel.is_cancelled() {
            return Err(TftpError::Cancelled);
        }
        let pkt = match pending.take() {
            Some(p) => p,
            None => match sess.recv(opts.timeout_ms).await? {
                Some((p, _)) => p,
                None => {
                    attempts -= 1;
                    if attempts == 0 {
                        return Err(TftpError::Timeout { retries: opts.retries });
                    }
                    // 重发最后 ACK 催促对端
                    sess.send(&Packet::Ack { block: last_acked }).await?;
                    continue;
                }
            },
        };
        match pkt {
            Packet::Data { block, data } => {
                if data.len() > blksize {
                    return Err(TftpError::Illegal(format!(
                        "DATA block {block} is {} bytes > blksize {blksize}",
                        data.len()
                    )));
                }
                if block == expected {
                    let is_final = data.len() < blksize;
                    sink.write_all(&data).await?;
                    total += data.len() as u64;
                    progress(total, total_hint);
                    last_acked = block;
                    expected = block.wrapping_add(1);
                    attempts = opts.retries.max(1);
                    since_ack += 1;
                    if is_final {
                        sess.send(&Packet::Ack { block }).await?;
                        sink.flush().await?;
                        return Ok(total);
                    }
                    if since_ack >= window {
                        sess.send(&Packet::Ack { block: last_acked }).await?;
                        since_ack = 0;
                    }
                } else {
                    // 重复/陈旧/乱序：重 ACK 最后成功块，引导对端重传正确块
                    sess.send(&Packet::Ack { block: last_acked }).await?;
                }
            }
            Packet::Error { code, message } => {
                return Err(TftpError::Remote {
                    code,
                    name: crate::error::error_name(code),
                    message,
                });
            }
            _ => {}
        }
    }
}
