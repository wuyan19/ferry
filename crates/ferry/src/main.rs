//! ferry：面向 Agent 的文件传输工具。
//!
//! `ferry serve`（默认）= NDJSON JSON-RPC 前端（protocol.md）
//! `ferry mcp`          = MCP stdio 前端（protocol.md §9）
//!
//! stdout 只走协议消息，日志一律 stderr；启动失败 = stderr + 非零退出、不写 stdout。

mod frontend;
mod schema;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

use ferry_core::api::{Api, ApiConfig};

#[derive(Parser)]
#[command(name = "ferry", version, about = "Agent-oriented file transfer tool (TFTP first)")]
struct Cli {
    #[command(subcommand)]
    command: Option<Frontend>,

    #[command(flatten)]
    args: SharedArgs,
}

#[derive(Subcommand)]
enum Frontend {
    /// NDJSON JSON-RPC 2.0 frontend over stdio (protocol.md)
    Serve,
    /// MCP stdio frontend (protocol.md §9)
    Mcp,
}

#[derive(clap::Args)]
struct SharedArgs {
    /// Sandbox root directory (created if missing).
    #[arg(long, global = true, default_value_os_t = default_sandbox_root())]
    sandbox_root: PathBuf,

    /// Sandbox quota in bytes.
    #[arg(long, global = true, default_value_t = 5 * 1024 * 1024 * 1024)]
    quota_bytes: u64,

    /// Max concurrent transfer tasks.
    #[arg(long, global = true, default_value_t = 8)]
    max_concurrent_tasks: usize,

    /// Log level for stderr: error|warn|info|debug|trace.
    #[arg(long, global = true, default_value = "info")]
    log_level: String,
}

fn default_sandbox_root() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".ferry").join("sandbox")
}

fn main() {
    let cli = Cli::parse();
    frontend::init_tracing(&cli.args.log_level);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let code = rt.block_on(run(cli));
    std::process::exit(code);
}

async fn run(cli: Cli) -> i32 {
    let cfg = ApiConfig {
        sandbox_root: cli.args.sandbox_root.clone(),
        quota_bytes: cli.args.quota_bytes,
        max_concurrent_tasks: cli.args.max_concurrent_tasks,
    };
    let api = match Api::new(cfg) {
        Ok(api) => api,
        Err(e) => {
            // 启动失败：只写 stderr，不污染 stdout 协议流（§1）
            eprintln!(
                "ferry: failed to initialize sandbox at {}: {e}",
                cli.args.sandbox_root.display()
            );
            return 2;
        }
    };
    match cli.command.unwrap_or(Frontend::Serve) {
        Frontend::Serve => frontend::ndjson::run(api).await,
        Frontend::Mcp => frontend::mcp::run(api).await,
    }
}
