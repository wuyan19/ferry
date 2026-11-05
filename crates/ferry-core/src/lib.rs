//! ferry 核心：沙箱、文件操作、任务管理、监听器与统一错误模型。
//! 前端无关——NDJSON 与 MCP 前端都只调用 [`api::Api`]。

pub mod api;
pub mod error;
pub mod event;
pub mod file_ops;
pub mod params;
pub mod sandbox;
pub mod task;
pub mod transfer;

pub use api::{Api, ApiConfig};
pub use error::{ErrCode, FerryError};
pub use event::Event;
pub use params::*;
