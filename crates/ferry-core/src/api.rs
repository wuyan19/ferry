//! Api 门面：两前端共用的类型化异步入口。
//!
//! 前端只做协议翻译（方法名/JSON ↔ 类型化调用 + 事件翻译），
//! 全部业务语义在此层。`Api` 是廉价 `Clone` 句柄（`Arc` 内部结构）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::FerryError;
use crate::event::Event;
use crate::params::*;
use crate::sandbox::{Sandbox, SharedSandbox};
use crate::task::TaskManager;

pub struct ApiConfig {
    pub sandbox_root: PathBuf,
    pub quota_bytes: u64,
    pub max_concurrent_tasks: usize,
}

pub(crate) struct ApiInner {
    sandbox: SharedSandbox,
    events: broadcast::Sender<Event>,
    shutdown: CancellationToken,
    started: Instant,
    pub(crate) max_concurrent_tasks: usize,
    pub(crate) tasks: TaskManager,
    pub(crate) listeners: crate::listener::ListenerManager,
}

#[derive(Clone)]
pub struct Api(Arc<ApiInner>);

impl Api {
    /// 初始化失败（沙箱根不可创建等）返回 io::Error，main 走启动失败路径：
    /// stderr + 非零退出、不写 stdout（protocol.md §1）。
    /// 必须在 tokio 运行时内调用（内部启动清扫任务）。
    pub fn new(cfg: ApiConfig) -> std::io::Result<Self> {
        let sandbox = Arc::new(Sandbox::new(cfg.sandbox_root, cfg.quota_bytes)?);
        let (events, _) = broadcast::channel(256);
        let tasks = TaskManager::new(cfg.max_concurrent_tasks, events.clone());
        let api = Self(Arc::new(ApiInner {
            sandbox,
            events,
            shutdown: CancellationToken::new(),
            started: Instant::now(),
            max_concurrent_tasks: cfg.max_concurrent_tasks,
            tasks,
            listeners: crate::listener::ListenerManager::new(),
        }));
        api.start_sweeper();
        Ok(api)
    }

    /// 周期清扫过期终态任务与幂等条目（TERMINAL_TTL，60s 周期）。
    fn start_sweeper(&self) {
        let api = self.clone();
        let shutdown = self.0.shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => api.0.tasks.sweep().await,
                }
            }
        });
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.0.sandbox
    }

    pub(crate) fn tasks(&self) -> &TaskManager {
        &self.0.tasks
    }

    pub(crate) fn listeners(&self) -> &crate::listener::ListenerManager {
        &self.0.listeners
    }

    pub fn max_concurrent_tasks(&self) -> usize {
        self.0.max_concurrent_tasks
    }

    /// 前端订阅事件流（ndjson → `event.*`；MCP → `notifications/*`）。
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.0.events.subscribe()
    }

    #[allow(dead_code)] // M2（transfer）/M3（listener）开始使用
    pub(crate) fn emit(&self, ev: Event) {
        // 无订阅者时丢弃即可（前端总在订阅）
        let _ = self.0.events.send(ev);
    }

    /// server.shutdown 与 stdin EOF 共用的关停信号。
    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.0.shutdown
    }

    // ---------- file.* ----------

    pub async fn file_list(&self, p: FileListParams) -> Result<FileListResult, FerryError> {
        crate::file_ops::list(&self.0.sandbox, p)
    }

    pub async fn file_stat(&self, p: FileStatParams) -> Result<FileStatResult, FerryError> {
        crate::file_ops::stat(&self.0.sandbox, p)
    }

    pub async fn file_write(&self, p: FileWriteParams) -> Result<FileWriteResult, FerryError> {
        crate::file_ops::write(&self.0.sandbox, p)
    }

    pub async fn file_read(&self, p: FileReadParams) -> Result<FileReadResult, FerryError> {
        crate::file_ops::read(&self.0.sandbox, p)
    }

    pub async fn file_delete(&self, p: FileDeleteParams) -> Result<FileDeleteResult, FerryError> {
        crate::file_ops::delete(&self.0.sandbox, p)
    }

    // ---------- transfer.* / task.* ----------

    pub async fn transfer_fetch(&self, p: TransferParams) -> Result<crate::transfer::TransferStart, FerryError> {
        self.transfer_start(crate::transfer::Direction::Fetch, p).await
    }

    pub async fn transfer_push(&self, p: TransferParams) -> Result<crate::transfer::TransferStart, FerryError> {
        self.transfer_start(crate::transfer::Direction::Push, p).await
    }

    /// 等待任务终态（timeout None = 无限等待），超时返回当前快照。
    pub async fn task_wait_raw(
        &self,
        task_id: &str,
        timeout: Option<std::time::Duration>,
    ) -> Result<TransferResult, FerryError> {
        let task = self
            .0
            .tasks
            .get(task_id)
            .await
            .ok_or_else(|| FerryError::task_not_found(task_id))?;
        Ok(TaskManager::wait_terminal(&task, timeout).await)
    }

    pub async fn task_status(&self, p: TaskStatusParams) -> Result<TransferResult, FerryError> {
        let task = self
            .0
            .tasks
            .get(&p.task_id)
            .await
            .ok_or_else(|| FerryError::task_not_found(&p.task_id))?;
        Ok(task.snapshot())
    }

    pub async fn task_wait(&self, p: TaskWaitParams) -> Result<TransferResult, FerryError> {
        let timeout = match p.timeout_ms {
            None | Some(0) => None,
            Some(ms) => Some(std::time::Duration::from_millis(ms)),
        };
        self.task_wait_raw(&p.task_id, timeout).await
    }

    pub async fn task_cancel(
        &self,
        p: TaskCancelParams,
    ) -> Result<crate::params::CancelResult, FerryError> {
        let (result, already_terminal) = self.0.tasks.cancel(&p.task_id).await?;
        Ok(crate::params::CancelResult { result, already_terminal })
    }

    // ---------- server.* ----------

    pub async fn server_status(&self) -> Result<ServerStatusResult, FerryError> {
        Ok(ServerStatusResult {
            uptime_s: self.0.started.elapsed().as_secs(),
            sandbox: SandboxInfo {
                root: self.0.sandbox.display_root().display().to_string(),
                used_bytes: self.0.sandbox.used_bytes(),
                quota_bytes: self.0.sandbox.quota_bytes(),
            },
            listeners: self.0.listeners.infos().await,
            active_tasks: self.0.tasks.active_ids().await,
        })
    }

    pub async fn server_shutdown(&self, _p: ShutdownParams) -> Result<ShutdownResult, FerryError> {
        self.0.shutdown.cancel();
        Ok(ShutdownResult { ok: true })
    }
}
