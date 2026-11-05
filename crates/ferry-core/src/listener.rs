//! listener.*：被动监听（ferry 作为 TFTP 服务端，protocol.md §4.4）。
//!
//! - 主 socket 只收首个 RRQ/WRQ 并做配置裁决：allowlist / allow / 设备路径安全 /
//!   并发闸，拒绝就地回 ERROR（2/0），不产生任务（协议：入站无交互式审批，
//!   过配置检查即自动服务）；
//! - 通过裁决的请求创建任务（inbound-read / inbound-write）、发 `InboundRequest`
//!   事件，随后派生引擎会话（新临时端口）；
//! - 上传落 `root/incoming/`，走 `.part` 临时文件 + rename，配额实时检查；
//! - `stop` 支持 drain（等待在途会话）或取消（协作式取消所有 inbound 任务）。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ipnet::IpNet;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use ferry_tftp::{serve_read, serve_write, send_error, Packet, Request, ServerLimits};

use crate::error::{ErrCode, FerryError};
use crate::event::Event;
use crate::params::{
    ListenerAllow, ListenerInfo, ListenerStartParams, ListenerStartResult, ListenerStatusParams,
    ListenerStopParams, ListenerStopResult, TaskState, TransferResult,
};
use crate::transfer::{map_engine_error, HashingSink, Throttle};
use crate::Api;

fn invalid_state(msg: impl Into<String>) -> FerryError {
    FerryError::new(ErrCode::InvalidState, msg.into())
        .with_suggestion("Call server.status to inspect live listeners.")
}

pub struct ListenerManager {
    listeners: Mutex<HashMap<String, Arc<Listener>>>,
    seq: AtomicU32,
}

impl ListenerManager {
    pub fn new() -> Self {
        Self { listeners: Mutex::new(HashMap::new()), seq: AtomicU32::new(0) }
    }

    fn next_id(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    async fn register(&self, id: String, l: Arc<Listener>) {
        self.listeners.lock().await.insert(id, l);
    }

    async fn unregister(&self, id: &str) {
        self.listeners.lock().await.remove(id);
    }

    async fn get(&self, id: &str) -> Option<Arc<Listener>> {
        self.listeners.lock().await.get(id).cloned()
    }

    pub async fn infos(&self) -> Vec<ListenerInfo> {
        let mut infos = Vec::new();
        for l in self.listeners.lock().await.values() {
            infos.push(l.info().await);
        }
        infos.sort_by(|a, b| a.listener_id.cmp(&b.listener_id));
        infos
    }
}

impl Default for ListenerManager {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct Listener {
    pub id: String,
    pub port: u16,
    /// 沙箱内的绝对路径（canonical）。
    pub root: PathBuf,
    pub root_rel: String,
    pub allow: ListenerAllow,
    pub allowlist: Vec<IpNet>,
    state: Mutex<&'static str>,
    /// accept 循环停止信号。
    pub cancel: CancellationToken,
    /// 在途 inbound 任务 id 集合。
    pub active: Mutex<HashSet<String>>,
    active_sem: Arc<Semaphore>,
}

impl Listener {
    async fn info(&self) -> ListenerInfo {
        let mut mode = Vec::new();
        if self.allow.read {
            mode.push("read".to_string());
        }
        if self.allow.write {
            mode.push("write".to_string());
        }
        ListenerInfo {
            listener_id: self.id.clone(),
            port: self.port,
            mode,
            root: self.root_rel.clone(),
            state: (*self.state.lock().await).to_string(),
            active_transfers: self.active.lock().await.len(),
        }
    }
}

/// 设备请求路径 → 相对 root 的规范化路径组件（拒 ..、绝对路径、反斜杠、盘符）。
fn device_path_rel(path: &str) -> Result<PathBuf, ()> {
    if path.is_empty() || path.contains('\0') || path.starts_with('/') {
        return Err(());
    }
    let mut rel = PathBuf::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => continue,
            ".." => return Err(()),
            c if c.contains('\\') || c.contains(':') => return Err(()),
            c => rel.push(c),
        }
    }
    if rel.as_os_str().is_empty() {
        return Err(());
    }
    Ok(rel)
}

impl Api {
    pub async fn listener_start(&self, p: ListenerStartParams) -> Result<ListenerStartResult, FerryError> {
        if !p.allow.read && !p.allow.write {
            return Err(FerryError::invalid_params("allow.read and allow.write are both false"));
        }
        let root_rel = p.root.clone().unwrap_or_default();
        let root = self.sandbox().resolve(&root_rel)?;
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|e| FerryError::io("listener create root", e))?;
        let allowlist = parse_allowlist(p.allowlist.as_deref().unwrap_or(&[]))?;
        let bind_ip: IpAddr = p
            .bind
            .as_deref()
            .unwrap_or("0.0.0.0")
            .parse()
            .map_err(|_| FerryError::invalid_params(format!("bind must be an IP address, got {:?}", p.bind)))?;
        let sock = UdpSocket::bind(SocketAddr::new(bind_ip, p.port)).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                FerryError::new(
                    ErrCode::PortInUse,
                    format!("cannot bind {}:{}: address already in use", bind_ip, p.port),
                )
                .with_suggestion("Pick another port, or stop the process occupying it.")
                .with_details(serde_json::json!({ "port": p.port }))
            } else {
                FerryError::io("listener bind", e)
            }
        })?;
        let port = sock.local_addr().map_err(|e| FerryError::io("listener local_addr", e))?.port();
        let seq = self.listeners().next_id();
        let id = format!("L-{seq:02}");
        let listener = Arc::new(Listener {
            id: id.clone(),
            port,
            root,
            root_rel: if root_rel.is_empty() { ".".to_string() } else { root_rel },
            allow: p.allow,
            allowlist,
            state: Mutex::new("listening"),
            cancel: CancellationToken::new(),
            active: Mutex::new(HashSet::new()),
            active_sem: Arc::new(Semaphore::new(p.max_concurrent.unwrap_or(4).max(1))),
        });
        self.listeners().register(id.clone(), listener.clone()).await;
        tokio::spawn(accept_loop(self.clone(), listener, Arc::new(sock)));
        Ok(ListenerStartResult { listener_id: id, port, state: "listening".into() })
    }

    pub async fn listener_stop(&self, p: ListenerStopParams) -> Result<ListenerStopResult, FerryError> {
        let l = self
            .listeners()
            .get(&p.listener_id)
            .await
            .ok_or_else(|| invalid_state(format!("listener not found: {}", p.listener_id)))?;
        if *l.state.lock().await != "listening" {
            return Err(invalid_state(format!("listener already stopped: {}", p.listener_id)));
        }
        l.cancel.cancel(); // 停止接收新请求
        if p.drain.unwrap_or(false) {
            // 等待在途会话结束（封顶 60s）
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            while tokio::time::Instant::now() < deadline {
                if l.active.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        // 取消仍在途的任务（drain 超时或非 drain 模式）
        let active: Vec<String> = l.active.lock().await.iter().cloned().collect();
        for tid in active {
            if let Some(t) = self.tasks().get(&tid).await {
                t.cancel.cancel();
            }
        }
        // 给取消留一点生效时间
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline && !l.active.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        *l.state.lock().await = "stopped";
        self.listeners().unregister(&p.listener_id).await;
        self.emit(Event::ListenerStopped {
            listener_id: p.listener_id.clone(),
            reason: "stopped".into(),
        });
        Ok(ListenerStopResult { stopped: true })
    }

    pub async fn listener_status(&self, p: ListenerStatusParams) -> Result<ListenerInfo, FerryError> {
        Ok(self
            .listeners()
            .get(&p.listener_id)
            .await
            .ok_or_else(|| invalid_state(format!("listener not found: {}", p.listener_id)))?
            .info()
            .await)
    }
}

fn parse_allowlist(list: &[String]) -> Result<Vec<IpNet>, FerryError> {
    list.iter()
        .map(|s| {
            s.parse::<IpNet>().map_err(|e| {
                FerryError::invalid_params(format!("bad CIDR {s:?}: {e}"))
            })
        })
        .collect()
}

async fn accept_loop(api: Api, l: Arc<Listener>, sock: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 65535];
    loop {
        tokio::select! {
            _ = l.cancel.cancelled() => break,
            res = sock.recv_from(&mut buf) => match res {
                Ok((n, from)) => {
                    if let Err(reason) = handle_inbound(&api, &l, &sock, &buf[..n], from).await {
                        tracing::warn!(listener = %l.id, %from, reason, "inbound request rejected");
                    }
                }
                Err(e) => {
                    tracing::error!(listener = %l.id, error = %e, "accept loop error");
                    break;
                }
            },
        }
    }
    *l.state.lock().await = "stopped";
    api.emit(Event::ListenerStopped {
        listener_id: l.id.clone(),
        reason: "accept loop exited".into(),
    });
}

/// 单个入站请求的裁决与会话派生。Err = 拒绝原因（已回 ERROR）。
async fn handle_inbound(
    api: &Api,
    l: &Arc<Listener>,
    sock: &Arc<UdpSocket>,
    buf: &[u8],
    from: SocketAddr,
) -> Result<(), String> {
    let pkt = Packet::decode(buf).map_err(|e| format!("bad packet: {e}"))?;
    let (req, op) = match pkt {
        Packet::Rrq(r) if l.allow.read => (r, "read"),
        Packet::Wrq(r) if l.allow.write => (r, "write"),
        Packet::Rrq(_) | Packet::Wrq(_) => {
            send_error(sock, from, 2, "operation not allowed").await;
            return Err("operation not allowed by listener".into());
        }
        _ => return Err("not a request packet".into()),
    };
    if !l.allowlist.is_empty() && !l.allowlist.iter().any(|n| n.contains(&from.ip())) {
        send_error(sock, from, 2, "source not in allowlist").await;
        return Err(format!("source {} not in allowlist", from.ip()));
    }
    let permit = match Arc::clone(&l.active_sem).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            send_error(sock, from, 0, "too many concurrent transfers").await;
            return Err("listener busy".into());
        }
    };
    let rel = match device_path_rel(&req.path) {
        Ok(r) => r,
        Err(()) => {
            drop(permit);
            send_error(sock, from, 2, "access violation").await;
            return Err(format!("unsafe path {:?}", req.path));
        }
    };
    // 仅 octet：netascii 在任何文件检查前拒绝（ERROR 4）
    if !matches!(req.mode, ferry_tftp::Mode::Octet) {
        drop(permit);
        send_error(sock, from, 4, "netascii not supported").await;
        return Err(format!("unsupported mode {}", req.mode.as_str()));
    }

    let kind: &'static str = if op == "read" { "inbound-read" } else { "inbound-write" };
    let task = api.tasks().create(kind, None).await;
    api.emit(Event::InboundRequest {
        listener_id: l.id.clone(),
        peer: from.to_string(),
        operation: op.to_string(),
        path: req.path.clone(),
        task_id: task.id.clone(),
    });
    l.active.lock().await.insert(task.id.clone());

    let api = api.clone();
    let l = l.clone();
    let sock = sock.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let _server_gate = api.tasks().semaphore().clone().acquire_owned().await;
        let mut snap = task.snapshot();
        snap.state = TaskState::Running;
        task.publish(snap);
        let started = std::time::Instant::now();

        let outcome = run_inbound(
            &api,
            &l,
            &task,
            InboundJob { sock, from, req, rel, is_read: op == "read" },
        )
        .await;
        let snap = match outcome {
            Ok((bytes, sha)) => TransferResult {
                task_id: task.id.clone(),
                state: TaskState::Success,
                bytes_transferred: bytes,
                duration_ms: started.elapsed().as_millis() as u64,
                sha256: sha,
                error: None,
                cached: None,
            },
            Err((state, ferr)) => {
                let error = ferr.map(|e| crate::params::TaskErrorInfo {
                    code: e.code.name().to_string(),
                    retryable: e.retryable,
                    suggestion: e.suggestion.clone(),
                    details: e.details.clone(),
                });
                TransferResult {
                    task_id: task.id.clone(),
                    state,
                    bytes_transferred: 0,
                    duration_ms: started.elapsed().as_millis() as u64,
                    sha256: None,
                    error,
                    cached: None,
                }
            }
        };
        l.active.lock().await.remove(&task.id);
        api.tasks().finish(&task, snap).await;
    });
    Ok(())
}

/// 会话执行所需的一次性上下文。
struct InboundJob {
    sock: Arc<UdpSocket>,
    from: SocketAddr,
    req: Request,
    rel: PathBuf,
    is_read: bool,
}

async fn run_inbound(
    api: &Api,
    l: &Arc<Listener>,
    task: &Arc<crate::task::Task>,
    job: InboundJob,
) -> Result<(u64, Option<String>), (TaskState, Option<FerryError>)> {
    let InboundJob { sock, from, req, rel, is_read } = job;
    let mut throttle = Throttle::new(api.clone(), &task.id);
    let cancel = task.cancel.clone();
    let limits = ServerLimits::default();

    if is_read {
        let target = l.root.join(rel);
        let md = match tokio::fs::metadata(&target).await {
            Ok(m) if m.is_file() => m,
            Ok(_) => {
                send_error(&sock, from, 1, "not a regular file").await;
                return Err((TaskState::Failed, Some(FerryError::file_not_found(&req.path))));
            }
            Err(_) => {
                send_error(&sock, from, 1, "file not found").await;
                return Err((TaskState::Failed, Some(FerryError::file_not_found(&req.path))));
            }
        };
        let mut file = match tokio::fs::File::open(&target).await {
            Ok(f) => f,
            Err(e) => {
                send_error(&sock, from, 2, "cannot open file").await;
                return Err((TaskState::Failed, Some(FerryError::io("listener open", e))));
            }
        };
        let mut progress = |b: u64, t: Option<u64>| throttle.on_progress(b, t);
        match serve_read(from, req, &mut file, md.len(), &limits, &cancel, &mut progress).await {
            Ok(r) => Ok((r.bytes, None)),
            Err(e) => Err(map_engine_error(e)),
        }
    } else {
        // 上传：root/incoming/<设备路径>，.part 临时文件 + rename
        let incoming = l.root.join("incoming");
        if let Err(e) = tokio::fs::create_dir_all(&incoming).await {
            return Err((TaskState::Failed, Some(FerryError::io("listener create incoming", e))));
        }
        let final_path = incoming.join(rel);
        if let Some(parent) = final_path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await {
                return Err((TaskState::Failed, Some(FerryError::io("listener create dirs", e))));
            }
        let old_len = tokio::fs::metadata(&final_path).await.map(|m| m.len()).unwrap_or(0);
        let mut temp = final_path.clone();
        let mut name = final_path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(format!(".{}.part", task.id));
        temp.set_file_name(name);
        let file = match tokio::fs::File::create(&temp).await {
            Ok(f) => f,
            Err(e) => return Err((TaskState::Failed, Some(FerryError::io("listener create temp", e)))),
        };
        let remaining = api
            .sandbox()
            .quota_bytes()
            .saturating_sub(api.sandbox().used_bytes())
            .saturating_add(old_len);
        let mut sink = HashingSink::new(file, remaining);
        let mut progress = |b: u64, t: Option<u64>| throttle.on_progress(b, t);
        match serve_write(from, req, &mut sink, &limits, &cancel, &mut progress).await {
            Ok(report) => {
                if let Err(e) = sink.sync().await {
                    let _ = tokio::fs::remove_file(&temp).await;
                    return Err((TaskState::Failed, Some(FerryError::io("listener sync", e))));
                }
                let sha = sink.sha256_hex();
                api.sandbox().add_used(report.bytes as i64 - old_len as i64);
                if let Err(e) = tokio::fs::rename(&temp, &final_path).await {
                    let _ = tokio::fs::remove_file(&temp).await;
                    api.sandbox().add_used(-(report.bytes as i64));
                    return Err((TaskState::Failed, Some(FerryError::io("listener rename", e))));
                }
                Ok((report.bytes, Some(sha)))
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&temp).await;
                Err(map_engine_error(e))
            }
        }
    }
}
