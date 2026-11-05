//! TaskManager：异步任务状态机 + 幂等缓存（protocol.md §4.3/§4.5）。
//!
//! - 状态经 `watch` 广播：`task.wait` 订阅等待终态，`task.status` 取快照；
//! - `task.cancel` 幂等：终态任务直接回终态（`already_terminal: true`），
//!   进行中的任务取消并等待其到达终态（协作式，当前块完成后）；
//! - 幂等键：在途 → 复用 task_id；终态 10 分钟窗口内 → 缓存结果；
//! - 定期清扫过期终态任务与幂等条目。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{broadcast, watch, Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::error::{ErrCode, FerryError};
use crate::event::Event;
use crate::params::TransferResult;

/// 终态任务与幂等结果的缓存窗口（protocol.md §4.3：10 分钟）。
pub const TERMINAL_TTL: Duration = Duration::from_secs(600);

pub(crate) struct Task {
    pub id: String,
    /// "fetch" | "push"（M3 起还有 "inbound-read"/"inbound-write"）
    #[allow(dead_code)]
    pub kind: &'static str,
    pub cancel: CancellationToken,
    #[allow(dead_code)] // M3 listener 任务用于任务-监听器关联
    pub idem_key: Option<String>,
    terminal_at: Mutex<Option<Instant>>,
    snapshot_tx: watch::Sender<TransferResult>,
}

impl Task {
    pub fn snapshot(&self) -> TransferResult {
        self.snapshot_tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<TransferResult> {
        self.snapshot_tx.subscribe()
    }

    pub(crate) fn publish(&self, snap: TransferResult) {
        self.snapshot_tx.send_replace(snap);
    }

    async fn mark_terminal(&self) {
        *self.terminal_at.lock().await = Some(Instant::now());
    }
}

#[derive(Clone)]
pub(crate) struct IdemEntry {
    pub task_id: String,
    pub canonical: Value,
}

pub struct TaskManager {
    tasks: Mutex<HashMap<String, Arc<Task>>>,
    idem: Mutex<HashMap<String, IdemEntry>>,
    seq: AtomicU32,
    semaphore: Arc<Semaphore>,
    events: broadcast::Sender<Event>,
}

impl TaskManager {
    pub fn new(max_concurrent: usize, events: broadcast::Sender<Event>) -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            idem: Mutex::new(HashMap::new()),
            seq: AtomicU32::new(0),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            events,
        }
    }

    pub fn semaphore(&self) -> &Arc<Semaphore> {
        &self.semaphore
    }

    pub(crate) async fn create(
        &self,
        kind: &'static str,
        idem_key: Option<String>,
    ) -> Arc<Task> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("T-{seq:04x}");
        let (tx, _rx) = watch::channel(TransferResult::pending(&id));
        let task = Arc::new(Task {
            id: id.clone(),
            kind,
            cancel: CancellationToken::new(),
            idem_key,
            terminal_at: Mutex::new(None),
            snapshot_tx: tx,
        });
        self.tasks.lock().await.insert(id, task.clone());
        task
    }

    pub(crate) async fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.lock().await.get(id).cloned()
    }

    pub async fn active_ids(&self) -> Vec<String> {
        self.tasks
            .lock()
            .await
            .iter()
            .filter(|(_, t)| !t.snapshot().state.is_terminal())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 任务体统一收尾：终态快照 + TransferCompleted 事件。
    pub(crate) async fn finish(&self, task: &Task, mut snap: TransferResult) {
        snap.cached = None; // 事件推送不带 cached（§4.3）
        task.publish(snap.clone());
        task.mark_terminal().await;
        let _ = self.events.send(Event::TransferCompleted { result: snap });
    }

    // ---------- 幂等 ----------

    pub(crate) async fn lookup_idem(&self, key: &str) -> Option<(String, Value)> {
        let entry = self.idem.lock().await.get(key).cloned()?;
        Some((entry.task_id, entry.canonical))
    }

    pub(crate) async fn register_idem(&self, key: String, task_id: String, canonical: Value) {
        self.idem.lock().await.insert(
            key,
            IdemEntry { task_id, canonical },
        );
    }

    // ---------- 清扫 ----------

    pub(crate) async fn sweep(&self) {
        let mut expired = Vec::new();
        {
            let tasks = self.tasks.lock().await;
            for (id, t) in tasks.iter() {
                let at = t.terminal_at.lock().await;
                if let Some(at) = *at
                    && at.elapsed() > TERMINAL_TTL {
                        expired.push(id.clone());
                    }
            }
        }
        if expired.is_empty() {
            return;
        }
        let mut tasks = self.tasks.lock().await;
        let mut idem = self.idem.lock().await;
        for id in &expired {
            tasks.remove(id);
            idem.retain(|_, e| &e.task_id != id);
        }
    }

    /// 等待任务终态；`timeout` 为 None 表示无限等待，超时返回当前快照。
    pub(crate) async fn wait_terminal(
        task: &Arc<Task>,
        timeout: Option<Duration>,
    ) -> TransferResult {
        if task.snapshot().state.is_terminal() {
            return task.snapshot();
        }
        let mut rx = task.subscribe();
        let fut = async {
            let _ = rx.wait_for(|s| s.state.is_terminal()).await;
        };
        match timeout {
            Some(t) => {
                let _ = tokio::time::timeout(t, fut).await;
            }
            None => fut.await,
        }
        task.snapshot()
    }

    /// task.cancel：幂等 + 等待取消生效。
    pub(crate) async fn cancel(&self, id: &str) -> Result<(TransferResult, bool), FerryError> {
        let task = self
            .get(id)
            .await
            .ok_or_else(|| FerryError::task_not_found(id))?;
        if task.snapshot().state.is_terminal() {
            return Ok((task.snapshot(), true));
        }
        task.cancel.cancel();
        // 协作式取消在当前块完成后生效；封顶等待 10s 后返回当前快照
        let mut rx = task.subscribe();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            rx.wait_for(|s| s.state.is_terminal()),
        )
        .await;
        Ok((task.snapshot(), false))
    }
}

/// 幂等冲突错误。
pub(crate) fn idempotency_conflict(key: &str) -> FerryError {
    FerryError::new(
        ErrCode::IdempotencyConflict,
        format!("idempotency_key {key:?} already used with different params"),
    )
    .with_suggestion("Reuse the exact same params, or pick a new key to force a new transfer.")
}
