//! TFTP 引擎：包编解码 + 客户端/服务端会话 + RFC 2347 选项协商。
//! 仅支持 octet 模式；与文件系统解耦，数据源/汇为字节流。
//!
//! 模块在 M1 里程碑落地：`packet` / `options` / `client` / `server` / `error`。
