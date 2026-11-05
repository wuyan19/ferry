//! TFTP 层错误。core 负责映射为协议错误码（REMOTE_TIMEOUT / REMOTE_ERROR / ...）。

/// RFC 1350 / RFC 2347 错误码的可读名。
pub const fn error_name(code: u16) -> &'static str {
    match code {
        0 => "NOT_DEFINED",
        1 => "FILE_NOT_FOUND",
        2 => "ACCESS_VIOLATION",
        3 => "DISK_FULL",
        4 => "ILLEGAL_OPERATION",
        5 => "UNKNOWN_TRANSFER_ID",
        6 => "FILE_EXISTS",
        7 => "NO_SUCH_USER",
        8 => "OPTION_NEGOTIATION_FAILED",
        _ => "UNKNOWN",
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TftpError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// 包解码失败 / 会话内协议违例。
    #[error("protocol violation: {0}")]
    Illegal(String),

    /// 对端无响应，重试耗尽（core → REMOTE_TIMEOUT）。
    #[error("peer timed out after {retries} retries")]
    Timeout { retries: u32 },

    /// 收到 TFTP ERROR packet（core → REMOTE_ERROR，透传 error_code）。
    #[error("remote error {code} ({name}): {message}")]
    Remote {
        code: u16,
        name: &'static str,
        message: String,
    },

    /// 网络层失败：DNS 解析失败 / 不可达（core → NETWORK_UNREACHABLE）。
    #[error("network error: {0}")]
    Network(String),

    /// 协作式取消。
    #[error("transfer cancelled")]
    Cancelled,
}
