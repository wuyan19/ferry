//! 入站会话（ferry 监听器作为 TFTP 服务端）。
//!
//! 监听器主 socket 收到 RRQ/WRQ 后，为每个请求派生独立会话：
//! 绑定新临时端口、锁定发起方、协商选项（OACK）、进入共享收发循环。
//! 模式检查（仅 octet）在此层拒绝并回 `ERROR(4)`。

use std::net::SocketAddr;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use crate::client::TransferReport;
use crate::error::TftpError;
use crate::options::{self, Options, ServerLimits, ServerTsize};
use crate::packet::{Mode, Packet, Request};
use crate::session::{recv_data, send_data, Session};

/// 向对端发送 ERROR packet（监听器拒绝请求时用，不经会话）。
pub async fn send_error(sock: &UdpSocket, to: SocketAddr, code: u16, message: &str) {
    let pkt = Packet::Error { code, message: message.to_string() };
    let _ = sock.send_to(&pkt.encode(), to).await;
}

/// 服务端握手：
/// - 有接受的选项 → OACK → 等 ACK(0)（超时重传 OACK）；
/// - 选项全拒 → 写方向直接回 ACK(0) 准许上传；读方向直接开始发数据。
async fn server_handshake(
    sess: &mut Session,
    req: &Request,
    limits: &ServerLimits,
    tsize: ServerTsize,
    is_write: bool,
) -> Result<Options, TftpError> {
    let (mut opts, accepted) = options::server_negotiate(&req.options, limits, tsize);
    opts.retries = limits.retries;
    if accepted.is_empty() {
        if is_write {
            sess.send(&Packet::Ack { block: 0 }).await?;
        }
        return Ok(opts);
    }
    sess.send(&Packet::Oack { options: accepted.clone() }).await?;
    let mut attempts = opts.retries.max(1);
    loop {
        match sess.recv(opts.timeout_ms).await? {
            Some((Packet::Ack { block: 0 }, _)) => return Ok(opts),
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
                sess.send(&Packet::Oack { options: accepted.clone() }).await?;
            }
        }
    }
}

async fn check_mode(sess: &mut Session, req: &Request) -> Result<(), TftpError> {
    if matches!(req.mode, Mode::Octet) {
        Ok(())
    } else {
        let _ = sess.send(&Packet::Error { code: 4, message: "netascii not supported".into() }).await;
        Err(TftpError::Illegal(format!("unsupported mode: {}", req.mode.as_str())))
    }
}

/// 设备 RRQ（读）：把 `source`（`file_len` 字节）发给 `peer`。
pub async fn serve_read(
    peer: SocketAddr,
    req: Request,
    source: &mut (impl AsyncRead + Unpin),
    file_len: u64,
    limits: &ServerLimits,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<TransferReport, TftpError> {
    let mut sess = Session::bind_ephemeral(peer).await?;
    sess.lock_peer(peer);
    check_mode(&mut sess, &req).await?;
    let opts = server_handshake(&mut sess, &req, limits, ServerTsize::Read { file_len }, false).await?;
    let bytes = send_data(&mut sess, &opts, source, cancel, progress).await?;
    Ok(TransferReport { bytes, total_bytes: Some(file_len) })
}

/// 设备 WRQ（写）：把设备上传的数据收进 `sink`。
/// 返回值 `total_bytes` 为 WRQ 声明的 tsize（调用方据此校验配额）。
pub async fn serve_write(
    peer: SocketAddr,
    req: Request,
    sink: &mut (impl AsyncWrite + Unpin),
    limits: &ServerLimits,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<TransferReport, TftpError> {
    let mut sess = Session::bind_ephemeral(peer).await?;
    sess.lock_peer(peer);
    check_mode(&mut sess, &req).await?;
    let declared = req
        .options
        .iter()
        .find(|(k, _)| k == "tsize")
        .and_then(|(_, v)| v.parse::<u64>().ok());
    let opts = server_handshake(&mut sess, &req, limits, ServerTsize::Write, true).await?;
    let bytes = recv_data(&mut sess, &opts, None, sink, cancel, progress).await?;
    Ok(TransferReport { bytes, total_bytes: declared })
}
