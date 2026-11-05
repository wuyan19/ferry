//! TFTP 包编解码（RFC 1350 + RFC 2347 OACK）。
//!
//! 仅 octet 模式：netascii 在此原样解码（[`Mode::Netascii`]），
//! 由会话层拒绝并回 `ERROR(4)`，便于测试覆盖该路径。

use bytes::{BufMut, Bytes, BytesMut};

pub const MAX_DATAGRAM: usize = 65535;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Octet,
    Netascii,
    Other(String),
}

impl Mode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "octet" => Self::Octet,
            "netascii" => Self::Netascii,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Octet => "octet",
            Self::Netascii => "netascii",
            Self::Other(s) => s.as_str(),
        }
    }
}

/// RRQ / WRQ 的公共载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub path: String,
    pub mode: Mode,
    /// RFC 2347 选项（保序，保留重复项由解析器去重——后者生效）。
    pub options: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    Rrq(Request),
    Wrq(Request),
    Data { block: u16, data: Bytes },
    Ack { block: u16 },
    Error { code: u16, message: String },
    Oack { options: Vec<(String, String)> },
}

fn read_cstr(buf: &[u8], pos: &mut usize) -> Result<String, String> {
    let start = *pos;
    while *pos < buf.len() && buf[*pos] != 0 {
        *pos += 1;
    }
    if *pos >= buf.len() {
        return Err("unterminated netascii string".into());
    }
    let s = std::str::from_utf8(&buf[start..*pos]).map_err(|e| e.to_string())?;
    *pos += 1; // NUL
    Ok(s.to_string())
}

fn put_cstr(buf: &mut BytesMut, s: &str) {
    buf.put_slice(s.as_bytes());
    buf.put_u8(0);
}

fn parse_options(buf: &[u8], pos: &mut usize) -> Result<Vec<(String, String)>, String> {
    let mut opts = Vec::new();
    while *pos < buf.len() {
        let k = read_cstr(buf, pos)?;
        if *pos >= buf.len() {
            return Err("option missing value".into());
        }
        let v = read_cstr(buf, pos)?;
        if k.is_empty() || v.is_empty() {
            return Err("empty option key/value".into());
        }
        opts.push((k.to_ascii_lowercase(), v));
    }
    Ok(opts)
}

impl Packet {
    pub fn decode(buf: &[u8]) -> Result<Self, String> {
        if buf.len() < 2 {
            return Err("packet too short".into());
        }
        let opcode = u16::from_be_bytes([buf[0], buf[1]]);
        let mut pos = 2;
        match opcode {
            1 | 2 => {
                let path = read_cstr(buf, &mut pos)?;
                let mode = Mode::parse(&read_cstr(buf, &mut pos)?);
                let options = if pos < buf.len() {
                    parse_options(buf, &mut pos)?
                } else {
                    Vec::new()
                };
                let req = Request { path, mode, options };
                Ok(if opcode == 1 { Self::Rrq(req) } else { Self::Wrq(req) })
            }
            3 => {
                if buf.len() < 4 {
                    return Err("DATA too short".into());
                }
                let block = u16::from_be_bytes([buf[2], buf[3]]);
                Ok(Self::Data {
                    block,
                    data: Bytes::copy_from_slice(&buf[4..]),
                })
            }
            4 => {
                if buf.len() != 4 {
                    return Err("ACK must be exactly 4 bytes".into());
                }
                Ok(Self::Ack {
                    block: u16::from_be_bytes([buf[2], buf[3]]),
                })
            }
            5 => {
                if buf.len() < 4 {
                    return Err("ERROR too short".into());
                }
                let code = u16::from_be_bytes([buf[2], buf[3]]);
                pos = 4;
                let message = if pos < buf.len() {
                    read_cstr(buf, &mut pos).unwrap_or_default()
                } else {
                    String::new()
                };
                Ok(Self::Error { code, message })
            }
            6 => {
                let options = parse_options(buf, &mut pos)?;
                Ok(Self::Oack { options })
            }
            op => Err(format!("unknown opcode {op}")),
        }
    }

    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::new();
        match self {
            Self::Rrq(req) => {
                buf.put_u16(1);
                put_cstr(&mut buf, &req.path);
                put_cstr(&mut buf, req.mode.as_str());
                for (k, v) in &req.options {
                    put_cstr(&mut buf, k);
                    put_cstr(&mut buf, v);
                }
            }
            Self::Wrq(req) => {
                buf.put_u16(2);
                put_cstr(&mut buf, &req.path);
                put_cstr(&mut buf, req.mode.as_str());
                for (k, v) in &req.options {
                    put_cstr(&mut buf, k);
                    put_cstr(&mut buf, v);
                }
            }
            Self::Data { block, data } => {
                buf.put_u16(3);
                buf.put_u16(*block);
                buf.put_slice(data);
            }
            Self::Ack { block } => {
                buf.put_u16(4);
                buf.put_u16(*block);
            }
            Self::Error { code, message } => {
                buf.put_u16(5);
                buf.put_u16(*code);
                put_cstr(&mut buf, message);
            }
            Self::Oack { options } => {
                buf.put_u16(6);
                for (k, v) in options {
                    put_cstr(&mut buf, k);
                    put_cstr(&mut buf, v);
                }
            }
        }
        buf.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrq_roundtrip_with_options() {
        let p = Packet::Rrq(Request {
            path: "fw/bin".into(),
            mode: Mode::Octet,
            options: vec![
                ("blksize".into(), "1468".into()),
                ("tsize".into(), "0".into()),
            ],
        });
        assert_eq!(Packet::decode(&p.encode()).unwrap(), p);
    }

    #[test]
    fn data_ack_error_roundtrip() {
        let cases = vec![
            Packet::Data { block: 65535, data: Bytes::from_static(b"\x00\x01") },
            Packet::Ack { block: 0 },
            Packet::Error { code: 4, message: "netascii not supported".into() },
            Packet::Oack { options: vec![("windowsize".into(), "4".into())] },
        ];
        for p in cases {
            assert_eq!(Packet::decode(&p.encode()).unwrap(), p);
        }
    }

    #[test]
    fn netascii_preserved_for_session_rejection() {
        let p = Packet::Rrq(Request {
            path: "cfg".into(),
            mode: Mode::Netascii,
            options: vec![],
        });
        assert_eq!(Packet::decode(&p.encode()).unwrap(), p);
    }

    #[test]
    fn rejects_malformed() {
        assert!(Packet::decode(b"").is_err());
        assert!(Packet::decode(b"\x00").is_err());
        assert!(Packet::decode(b"\x00\x01fw").is_err()); // 未终结的文件名
        assert!(Packet::decode(b"\x00\x03\x00").is_err()); // DATA 块号不完整
        assert!(Packet::decode(b"\x00\x04\x00\x01\x00").is_err()); // ACK 多余字节
        assert!(Packet::decode(b"\x00\x09").is_err()); // 未知 opcode
    }

    #[test]
    fn option_keys_lowercased() {
        let p = Packet::Rrq(Request {
            path: "f".into(),
            mode: Mode::Octet,
            options: vec![("BLKSIZE".into(), "512".into())],
        });
        match Packet::decode(&p.encode()).unwrap() {
            Packet::Rrq(r) => assert_eq!(r.options[0].0, "blksize"),
            _ => panic!(),
        }
    }
}
