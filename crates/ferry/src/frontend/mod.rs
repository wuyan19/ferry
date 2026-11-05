//! 前端共享设施：stdout 单写者、stderr 日志初始化。

pub mod mcp;
pub mod ndjson;

use tokio::sync::mpsc;

/// stdout 单写者：所有协议消息经 mpsc 串行化，保证 NDJSON 行不交错。
/// writer 任务持有 stdout 锁直到通道关闭，随后退出（天然 flush 收尾）。
#[derive(Clone)]
pub struct LineWriter {
    tx: mpsc::Sender<String>,
}

impl LineWriter {
    pub async fn line(&self, line: String) {
        let _ = self.tx.send(line).await;
    }

    pub async fn json(&self, value: serde_json::Value) {
        self.line(value.to_string()).await;
    }
}

pub fn spawn_line_writer() -> (LineWriter, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<String>(256);
    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncWriteExt, BufWriter};
        let mut out = BufWriter::new(tokio::io::stdout());
        while let Some(line) = rx.recv().await {
            if out.write_all(line.as_bytes()).await.is_err()
                || out.write_all(b"\n").await.is_err()
                || out.flush().await.is_err()
            {
                break; // stdout 关闭：Agent 已退出
            }
        }
        let _ = out.flush().await;
    });
    (LineWriter { tx }, handle)
}

/// stderr 日志。ANSI 关闭（管道场景），级别非法时回退 info。
pub fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .init();
}
