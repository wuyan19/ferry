//! MCP stdio 前端（protocol.md §9）—— M4 里程碑实现。
//!
//! 当前为占位：保持 CLI 形状完整，进入即退出并说明原因（stderr）。

use ferry_core::Api;

pub async fn run(_api: Api) -> i32 {
    eprintln!("ferry: MCP frontend is not implemented yet (milestone M4)");
    2
}
