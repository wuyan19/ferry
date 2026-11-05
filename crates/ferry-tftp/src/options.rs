//! RFC 2347 选项协商（blksize 2348 / tsize·timeout 2349 / windowsize 7440）。
//!
//! 协商规则：
//! - **客户端**（RRQ/WRQ 附带选项）：请求 blksize/tsize/timeout/windowsize；
//!   收到 OACK 时校验——必须是请求过的选项子集、值域合法、windowsize 不得大于请求值，
//!   违规回 `ERROR(8)`。
//! - **服务端**：逐项裁决——blksize 夹到 `[8, max_blksize]`，windowsize 夹到 `[1, 16]`，
//!   timeout 夹到 `[1, 60]`s，tsize 由会话层填（读=文件大小 / 写=回显声明值）。
//!   未接受的项不出现在 OACK 中（客户端按未接受处理）。

use crate::error::TftpError;
use crate::packet::Mode;

pub const MIN_BLKSIZE: u16 = 8;
pub const MAX_BLKSIZE: u16 = 65464;
pub const MAX_WINDOWSIZE: u16 = 16;
pub const MAX_TIMEOUT_SECS: u64 = 60;

/// 协商完成的会话参数。
#[derive(Debug, Clone)]
pub struct Options {
    pub blksize: u16,
    pub windowsize: u16,
    pub timeout_ms: u64,
    pub retries: u32,
    /// tsize（fetch = 对端声明的总大小；push = 本端声明的大小）。
    pub tsize: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            blksize: 512,
            windowsize: 1,
            timeout_ms: 5000,
            retries: 3,
            tsize: None,
        }
    }
}

pub type OptPairs = Vec<(String, String)>;

fn num(v: &str) -> Option<u64> {
    v.parse::<u64>().ok()
}

/// 客户端构造 RRQ/WRQ 携带的选项。
pub fn client_request(blksize: u16, windowsize: u16, timeout_ms: u64, tsize: Option<u64>) -> OptPairs {
    let mut pairs = vec![("blksize".to_string(), blksize.to_string())];
    if let Some(t) = tsize {
        pairs.push(("tsize".to_string(), t.to_string()));
    }
    pairs.push(("timeout".to_string(), (timeout_ms / 1000).max(1).to_string()));
    pairs.push(("windowsize".to_string(), windowsize.to_string()));
    pairs
}

/// 客户端采用 OACK 结果。非法时返回 Err（调用方应回 ERROR(8) 后终止）。
pub fn client_adopt(oack: OptPairs, requested: &OptPairs) -> Result<Options, TftpError> {
    let requested_keys: Vec<&str> = requested.iter().map(|(k, _)| k.as_str()).collect();
    let mut opts = Options::default();
    for (k, v) in &oack {
        if !requested_keys.contains(&k.as_str()) {
            return Err(TftpError::Illegal(format!("OACK contains unrequested option {k:?}")));
        }
        match k.as_str() {
            "blksize" => {
                let b = num(v).filter(|&b| (MIN_BLKSIZE as u64..=MAX_BLKSIZE as u64).contains(&b))
                    .ok_or_else(|| TftpError::Illegal(format!("OACK blksize out of range: {v}")))?;
                opts.blksize = b as u16;
            }
            "windowsize" => {
                let req = requested.iter().find(|(rk, _)| rk == "windowsize").and_then(|(_, rv)| num(rv)).unwrap_or(1);
                let w = num(v).filter(|&w| w >= 1 && w <= req.min(MAX_WINDOWSIZE as u64))
                    .ok_or_else(|| TftpError::Illegal(format!("OACK windowsize exceeds request: {v}")))?;
                opts.windowsize = w as u16;
            }
            "timeout" => {
                let secs = num(v).filter(|&s| (1..=MAX_TIMEOUT_SECS).contains(&s))
                    .ok_or_else(|| TftpError::Illegal(format!("OACK timeout out of range: {v}")))?;
                opts.timeout_ms = secs * 1000;
            }
            "tsize" => {
                opts.tsize = Some(num(v).ok_or_else(|| TftpError::Illegal(format!("OACK tsize invalid: {v}")))?);
            }
            other => return Err(TftpError::Illegal(format!("unknown option {other:?}"))),
        }
    }
    Ok(opts)
}

pub struct ServerLimits {
    pub max_blksize: u16,
    pub timeout_ms: u64,
    pub retries: u32,
}

impl Default for ServerLimits {
    fn default() -> Self {
        // 以太网 MTU 安全上限（1500 - 20 IP - 8 UDP）
        Self { max_blksize: 1468, timeout_ms: 5000, retries: 3 }
    }
}

/// 服务端裁决请求选项：返回 (会话参数, 将写进 OACK 的接受项)。
/// OACK 项为空 = 全部未接受（按无选项流程走，即默认参数）。
pub fn server_negotiate(
    requested: &OptPairs,
    limits: &ServerLimits,
    mode: ServerTsize,
) -> (Options, OptPairs) {
    let mut opts = Options {
        timeout_ms: limits.timeout_ms,
        retries: limits.retries,
        ..Default::default()
    };
    let mut accepted: OptPairs = Vec::new();
    let mut saw = std::collections::HashSet::new();
    for (k, v) in requested {
        if !saw.insert(k.clone()) {
            continue; // 重复项只认第一个
        }
        match k.as_str() {
            "blksize" => {
                if let Some(b) = num(v).filter(|&b| b >= MIN_BLKSIZE as u64) {
                    let b = b.min(limits.max_blksize as u64) as u16;
                    opts.blksize = b;
                    accepted.push((k.clone(), b.to_string()));
                }
            }
            "windowsize" => {
                if let Some(w) = num(v).filter(|&w| w >= 1) {
                    let w = w.min(MAX_WINDOWSIZE as u64) as u16;
                    opts.windowsize = w;
                    accepted.push((k.clone(), w.to_string()));
                }
            }
            "timeout" => {
                if let Some(s) = num(v).filter(|&s| s >= 1) {
                    let s = s.min(MAX_TIMEOUT_SECS);
                    opts.timeout_ms = s * 1000;
                    accepted.push((k.clone(), s.to_string()));
                }
            }
            "tsize" => {
                let echo = match &mode {
                    // 读请求：声明我方文件大小
                    ServerTsize::Read { file_len } => file_len.to_string(),
                    // 写请求：回显客户端声明（tsize 请求值本身可能非数字 → 回 0）
                    ServerTsize::Write => num(v).unwrap_or(0).to_string(),
                };
                opts.tsize = num(&echo);
                accepted.push(("tsize".to_string(), echo));
            }
            _ => {} // 未认识的服务端选项：忽略
        }
    }
    (opts, accepted)
}

/// 服务端视角 tsize 语义。
pub enum ServerTsize {
    Read { file_len: u64 },
    Write,
}

/// 请求模式是否可服务（仅 octet）。
pub fn mode_supported(mode: &Mode) -> bool {
    matches!(mode, Mode::Octet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_caps_blksize_and_window() {
        let limits = ServerLimits::default();
        let (o, acked) = server_negotiate(
            &vec![("blksize".into(), "65464".into()), ("windowsize".into(), "32".into())],
            &limits,
            ServerTsize::Write,
        );
        assert_eq!(o.blksize, 1468);
        assert_eq!(o.windowsize, 16);
        assert_eq!(acked[0].1, "1468");
        assert_eq!(acked[1].1, "16");
    }

    #[test]
    fn server_rejects_invalid_silently() {
        let (o, acked) = server_negotiate(
            &vec![("blksize".into(), "7".into()), ("junkopt".into(), "1".into())],
            &ServerLimits::default(),
            ServerTsize::Write,
        );
        assert_eq!(o.blksize, 512);
        assert!(acked.is_empty());
    }

    #[test]
    fn client_adopts_subset() {
        let req = client_request(1024, 4, 5000, Some(0));
        let o = client_adopt(vec![("blksize".into(), "1024".into()), ("tsize".into(), "4096".into())], &req).unwrap();
        assert_eq!(o.blksize, 1024);
        assert_eq!(o.windowsize, 1); // 未回 = 未接受
        assert_eq!(o.tsize, Some(4096));
    }

    #[test]
    fn client_rejects_unrequested_and_oversized() {
        let req = client_request(512, 1, 5000, None);
        assert!(client_adopt(vec![("blksize".into(), "512".into()), ("windowsize".into(), "4".into())], &req).is_err());
        assert!(client_adopt(vec![("weird".into(), "1".into())], &req).is_err());
    }
}
