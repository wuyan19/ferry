//! transfer.fetch / transfer.push 编排（protocol.md §4.3）。
//!
//! - RPC 级前置校验（参数、沙箱路径、push 源存在性）在任务创建前完成；
//! - 网络失败是**任务层**错误：resolve/DNS/超时/ERROR packet 都发生在任务体内，
//!   折叠进 `TransferResult.error`，绝不作为 JSON-RPC error 返回；
//! - fetch 落盘走 `.part` 临时文件 + rename，配额在写入流中实时检查；
//! - push/fetch 的 sha256 在 IO 包装器中流式计算。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

use ferry_tftp::{fetch_to_writer, push_from_reader, ClientConfig, TftpError};

use crate::error::{ErrCode, FerryError};
use crate::event::Event;
use crate::params::{TaskErrorInfo, TaskState, TransferParams, TransferResult};
use crate::Api;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Fetch,
    Push,
}

impl Direction {
    fn kind(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Push => "push",
        }
    }
}

/// start_transfer 的返回：task_id + 当前快照（可能是终态缓存命中）。
#[derive(Debug)]
pub struct TransferStart {
    pub task_id: String,
    pub snapshot: TransferResult,
}

const PROGRESS_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const PROGRESS_MIN_DELTA: u64 = 256 * 1024;

/// 幂等参数比对键（§4.3：wait / idempotency_key 不参与）。
pub(crate) fn canonical_key(p: &TransferParams) -> Value {
    json!({
        "host": p.host,
        "port": p.port.unwrap_or(69),
        "remote_path": p.remote_path,
        "local_path": p.local_path,
        "block_size": p.block_size.unwrap_or(512),
        "window_size": p.window_size.unwrap_or(1),
        "timeout_ms": p.timeout_ms.unwrap_or(5000),
        "retries": p.retries.unwrap_or(3),
    })
}

fn engine_config(p: &TransferParams) -> Result<ClientConfig, FerryError> {
    let block_size = p.block_size.unwrap_or(512);
    if !(8..=65464).contains(&block_size) {
        return Err(FerryError::invalid_params(format!("block_size must be 8..=65464, got {block_size}")));
    }
    let window_size = p.window_size.unwrap_or(1);
    if !(1..=16).contains(&window_size) {
        return Err(FerryError::invalid_params(format!("window_size must be 1..=16, got {window_size}")));
    }
    let timeout_ms = p.timeout_ms.unwrap_or(5000).max(100);
    let retries = p.retries.unwrap_or(3).clamp(1, 10);
    Ok(ClientConfig { block_size, window_size, timeout_ms, retries })
}

impl Api {
    /// 创建（或幂等复用）传输任务。RPC 级错误在此返回；
    /// 网络级失败发生在任务体内（任务层错误）。
    pub async fn transfer_start(
        &self,
        dir: Direction,
        p: TransferParams,
    ) -> Result<TransferStart, FerryError> {
        if p.host.trim().is_empty() {
            return Err(FerryError::invalid_params("host must not be empty"));
        }
        if p.remote_path.is_empty() {
            return Err(FerryError::invalid_params("remote_path must not be empty"));
        }
        if p.local_path.is_empty() {
            return Err(FerryError::invalid_params("local_path must not be empty"));
        }
        let _cfg = engine_config(&p)?;
        let dest = self.sandbox().resolve(&p.local_path)?;
        // push：源必须已存在于沙箱
        if dir == Direction::Push {
            let md = tokio::fs::metadata(&dest)
                .await
                .map_err(|_| FerryError::file_not_found(&p.local_path))?;
            if !md.is_file() {
                return Err(FerryError::invalid_params(format!(
                    "local_path is not a file: {:?}",
                    p.local_path
                )));
            }
        }

        // 幂等：在途去重 / 终态缓存 / 参数冲突
        let canonical = canonical_key(&p);
        if let Some(key) = &p.idempotency_key
            && let Some((task_id, prev)) = self.tasks().lookup_idem(key).await {
                if prev != canonical {
                    return Err(crate::task::idempotency_conflict(key));
                }
                if let Some(task) = self.tasks().get(&task_id).await {
                    let mut snap = task.snapshot();
                    if snap.state.is_terminal() {
                        snap.cached = Some(true);
                    } else {
                        snap.cached = Some(false); // 在途去重
                    }
                    return Ok(TransferStart { task_id, snapshot: snap });
                }
                // 任务已被清扫：按新任务处理，键重新登记
            }

        let task = self
            .tasks()
            .create(dir.kind(), p.idempotency_key.clone())
            .await;
        if let Some(key) = &p.idempotency_key {
            self.tasks()
                .register_idem(key.clone(), task.id.clone(), canonical)
                .await;
        }
        let task_id = task.id.clone();
        spawn_transfer_body(self.clone(), task, dir, p);
        Ok(TransferStart {
            snapshot: TransferResult::pending(&task_id), // cached: Some(false)
            task_id,
        })
    }
}

/// 任务体：并发闸 → Running → 执行 → 终态收尾。
fn spawn_transfer_body(api: Api, task: Arc<crate::task::Task>, dir: Direction, p: TransferParams) {
    tokio::spawn(async move {
        let _permit = api.tasks().semaphore().clone().acquire_owned().await;
        let mut snap = task.snapshot();
        snap.state = TaskState::Running;
        task.publish(snap);
        let started = Instant::now();

        let outcome = run_transfer(&api, &task, dir, &p).await;
        let mut snap = match outcome {
            Ok((bytes, sha256)) => TransferResult {
                task_id: task.id.clone(),
                state: TaskState::Success,
                bytes_transferred: bytes,
                duration_ms: started.elapsed().as_millis() as u64,
                sha256,
                error: None,
                cached: None,
            },
            Err((state, ferr)) => TransferResult {
                task_id: task.id.clone(),
                state,
                bytes_transferred: 0,
                duration_ms: started.elapsed().as_millis() as u64,
                sha256: None,
                error: ferr.map(|e| TaskErrorInfo {
                    code: e.code.name().to_string(),
                    retryable: e.retryable,
                    suggestion: e.suggestion.clone(),
                    details: e.details.clone(),
                }),
                cached: None,
            },
        };
        snap.duration_ms = started.elapsed().as_millis() as u64;
        api.tasks().finish(&task, snap).await;
    });
}

/// Ok((bytes, sha256)) | Err((state, error))。Aborted 时 error 为 None。
async fn run_transfer(
    api: &Api,
    task: &Arc<crate::task::Task>,
    dir: Direction,
    p: &TransferParams,
) -> Result<(u64, Option<String>), (TaskState, Option<FerryError>)> {
    let cfg = engine_config(p).expect("validated in transfer_start");
    let port = p.port.unwrap_or(69);

    // DNS 解析（任务层：失败 = NETWORK_UNREACHABLE）
    let addr: SocketAddr = match tokio::net::lookup_host((p.host.clone(), port)).await {
        Ok(addrs) => {
            let mut picked: Option<SocketAddr> = None;
            for a in addrs {
                if picked.is_none() {
                    picked = Some(a);
                }
                if a.is_ipv4() {
                    picked = Some(a);
                    break;
                }
            }
            match picked {
                Some(a) => a,
                None => return Err((TaskState::Failed, Some(network_unreachable(&p.host, "no addresses resolved")))),
            }
        }
        Err(e) => return Err((TaskState::Failed, Some(network_unreachable(&p.host, &e.to_string())))),
    };

    match dir {
        Direction::Fetch => run_fetch(api, task, p, cfg, addr).await,
        Direction::Push => run_push(api, task, p, cfg, addr).await,
    }
}

fn network_unreachable(host: &str, reason: &str) -> FerryError {
    FerryError::new(
        ErrCode::NetworkUnreachable,
        format!("cannot reach {host:?}: {reason}"),
    )
    .with_suggestion("Check the host name/address and network connectivity, then retry.")
    .with_details(json!({ "host": host }))
}

/// 进度节流状态（≥1s 或 ≥256KiB 才发事件）。
pub(crate) struct Throttle {
    api: Api,
    task_id: String,
    last_at: Instant,
    last_bytes: u64,
}

impl Throttle {
    pub(crate) fn new(api: Api, task_id: &str) -> Self {
        Self {
            api,
            task_id: task_id.to_string(),
            last_at: Instant::now() - PROGRESS_MIN_INTERVAL, // 首块即发
            last_bytes: 0,
        }
    }

    pub(crate) fn on_progress(&mut self, bytes: u64, total: Option<u64>) {
        let now = Instant::now();
        if now - self.last_at < PROGRESS_MIN_INTERVAL && bytes - self.last_bytes < PROGRESS_MIN_DELTA
        {
            return;
        }
        self.last_at = now;
        self.last_bytes = bytes;
        let percent = total.filter(|&t| t > 0).map(|t| (bytes as f64 * 100.0 / t as f64 * 10.0).round() / 10.0);
        self.api.emit(Event::TransferProgress {
            task_id: self.task_id.clone(),
            bytes_transferred: bytes,
            total_bytes: total,
            percent,
        });
    }
}

async fn run_fetch(
    api: &Api,
    task: &Arc<crate::task::Task>,
    p: &TransferParams,
    cfg: ClientConfig,
    addr: SocketAddr,
) -> Result<(u64, Option<String>), (TaskState, Option<FerryError>)> {
    let dest = match api.sandbox().resolve(&p.local_path) {
        Ok(d) => d,
        Err(e) => return Err((TaskState::Failed, Some(e))),
    };
    let old_len = tokio::fs::metadata(&dest).await.map(|m| m.len()).unwrap_or(0);
    if let Some(parent) = dest.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await {
            return Err((TaskState::Failed, Some(FerryError::io("fetch create parent", e))));
        }
    // 临时文件 + 成功后 rename，避免半截文件可见
    let mut temp: PathBuf = dest.clone();
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{}.part", task.id));
    temp.set_file_name(name);

    let file = match tokio::fs::File::create(&temp).await {
        Ok(f) => f,
        Err(e) => return Err((TaskState::Failed, Some(FerryError::io("fetch create temp", e)))),
    };
    let remaining = api
        .sandbox()
        .quota_bytes()
        .saturating_sub(api.sandbox().used_bytes())
        .saturating_add(old_len); // 覆盖已有文件时抵扣旧占用
    let mut sink = HashingSink::new(file, remaining);
    let mut throttle = Throttle::new(api.clone(), &task.id);
    let cancel: CancellationToken = task.cancel.clone();
    let mut progress = |bytes: u64, total: Option<u64>| throttle.on_progress(bytes, total);

    let result =
        fetch_to_writer(addr, &p.remote_path, cfg, &mut sink, &cancel, &mut progress).await;
    match result {
        Ok(report) => {
            if let Err(e) = sink.inner.sync_all().await {
                let _ = tokio::fs::remove_file(&temp).await;
                return Err((TaskState::Failed, Some(FerryError::io("fetch sync", e))));
            }
            let sha = sink.sha256_hex();
            api.sandbox().add_used(report.bytes as i64 - old_len as i64);
            if let Err(e) = tokio::fs::rename(&temp, &dest).await {
                let _ = tokio::fs::remove_file(&temp).await;
                api.sandbox().add_used(-(report.bytes as i64));
                return Err((TaskState::Failed, Some(FerryError::io("fetch rename", e))));
            }
            Ok((report.bytes, Some(sha)))
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp).await;
            Err(map_engine_error(e))
        }
    }
}

async fn run_push(
    api: &Api,
    task: &Arc<crate::task::Task>,
    p: &TransferParams,
    cfg: ClientConfig,
    addr: SocketAddr,
) -> Result<(u64, Option<String>), (TaskState, Option<FerryError>)> {
    let src = match api.sandbox().resolve(&p.local_path) {
        Ok(s) => s,
        Err(e) => return Err((TaskState::Failed, Some(e))),
    };
    let len = match tokio::fs::metadata(&src).await {
        Ok(m) => m.len(),
        Err(_) => return Err((TaskState::Failed, Some(FerryError::file_not_found(&p.local_path)))),
    };
    let file = match tokio::fs::File::open(&src).await {
        Ok(f) => f,
        Err(e) => return Err((TaskState::Failed, Some(FerryError::io("push open", e)))),
    };
    let mut reader = HashingReader::new(file);
    let mut throttle = Throttle::new(api.clone(), &task.id);
    let cancel: CancellationToken = task.cancel.clone();
    let mut progress = |bytes: u64, total: Option<u64>| throttle.on_progress(bytes, total);

    let result =
        push_from_reader(addr, &p.remote_path, cfg, &mut reader, len, &cancel, &mut progress).await;
    match result {
        Ok(report) => Ok((report.bytes, Some(reader.sha256_hex()))),
        Err(e) => Err(map_engine_error(e)),
    }
}

/// 引擎错误 → (终态, 任务层错误)。Cancelled → Aborted/None。
pub(crate) fn map_engine_error(e: TftpError) -> (TaskState, Option<FerryError>) {
    let ferr = match e {
        TftpError::Cancelled => return (TaskState::Aborted, None),
        TftpError::Timeout { retries } => FerryError::new(
            ErrCode::RemoteTimeout,
            format!("remote stopped responding after {retries} retries"),
        )
        .with_suggestion("Verify the device TFTP service is up; safe to retry with the same idempotency_key.")
        .with_details(json!({ "retries": retries })),
        TftpError::Remote { code, name, message } => FerryError::new(
            ErrCode::RemoteError,
            format!("remote replied ERROR({code} {name}): {message}"),
        )
        .with_suggestion("Inspect data.tftp_error_name; codes 1/6 mean the remote path is wrong or exists.")
        .with_details(json!({
            "tftp_error_code": code,
            "tftp_error_name": name,
            "message": message,
        })),
        TftpError::Network(s) => network_unreachable("", &s),
        TftpError::Io(e) if e.kind() == std::io::ErrorKind::StorageFull || e.to_string().contains("quota") => {
            FerryError::new(ErrCode::QuotaExceeded, format!("sandbox quota exceeded during write: {e}"))
                .with_suggestion("Delete unneeded files via file.delete, then retry with a new idempotency_key.")
        }
        TftpError::Io(e) => FerryError::io("transfer io", e),
        TftpError::Illegal(s) => FerryError::new(
            ErrCode::RemoteError,
            format!("peer violated the protocol: {s}"),
        )
        .with_suggestion("The remote TFTP implementation is misbehaving; not retryable as-is.")
        .with_details(json!({ "reason": s })),
    };
    (TaskState::Failed, Some(ferr))
}

// ---------- IO 包装器：流式 sha256 + 配额实时检查 ----------

/// 写入侧：更新 sha256、超配额即报 StorageFull（临时文件占用不计入 used，
/// 成功 rename 后才入账，因此超限只需中断写入）。
pub(crate) struct HashingSink {
    inner: tokio::fs::File,
    hasher: Sha256,
    written: u64,
    remaining: u64,
}

impl HashingSink {
    pub(crate) fn new(inner: tokio::fs::File, remaining: u64) -> Self {
        Self { inner, hasher: Sha256::new(), written: 0, remaining }
    }

    pub(crate) fn sha256_hex(&mut self) -> String {
        self.hasher.clone().finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 落盘同步（rename 前调用）。
    pub(crate) async fn sync(&self) -> std::io::Result<()> {
        self.inner.sync_all().await
    }
}

impl AsyncWrite for HashingSink {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.written + buf.len() as u64 > this.remaining {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "sandbox quota exceeded",
            )));
        }
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                this.hasher.update(&buf[..n]);
                this.written += n as u64;
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// 读取侧：推送时对流经内容计算 sha256。
pub(crate) struct HashingReader {
    inner: tokio::fs::File,
    hasher: Sha256,
    #[allow(dead_code)]
    read: u64,
}

impl HashingReader {
    fn new(inner: tokio::fs::File) -> Self {
        Self { inner, hasher: Sha256::new(), read: 0 }
    }

    fn sha256_hex(&mut self) -> String {
        self.hasher.clone().finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl AsyncRead for HashingReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let filled_before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let new = &buf.filled()[filled_before..];
                this.hasher.update(new);
                this.read += new.len() as u64;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}
