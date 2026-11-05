//! TFTP 引擎：包编解码 + 客户端/服务端会话 + RFC 2347 选项协商。
//!
//! - 仅支持 octet 模式（netascii 在会话层回 `ERROR(4)` 拒绝）；
//! - 与文件系统解耦：数据源为 `AsyncRead`、数据汇为 `AsyncWrite`；
//! - 选项协商：blksize（RFC 2348）、tsize/timeout（RFC 2349）、windowsize（RFC 7440）；
//! - 可靠性：per-packet 超时 + 有限重传（发送方重传未确认窗口，接收方重 ACK 引导）、
//!   16 位块号回绕、重复块去重（防巫师学徒综合征）。

pub mod client;
pub mod error;
pub mod options;
pub mod packet;
pub mod server;
mod session;

pub use client::{fetch_to_writer, push_from_reader, ClientConfig, TransferReport};
pub use error::{error_name, TftpError};
pub use options::ServerLimits;
pub use packet::{Mode, Packet, Request};
pub use server::{serve_read, serve_write, send_error};
