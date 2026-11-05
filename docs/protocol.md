# TFTP 服务器控制面协议规范 v1.0

**传输层**：JSON-RPC 2.0 over stdio（NDJSON 前端 + MCP stdio 兼容前端，见 §9）
**角色**：Agent（客户端）↔ TFTP Server（服务器子进程）

---

## 1. 传输层约定

| 约定项 | 规则 |
| :--- | :--- |
| **分帧** | **NDJSON**：每条消息必须是**单行** JSON，以 `\n` 结尾。禁止跨行、禁止 BOM、禁止 JSON-RPC batch（一行恰好一个对象）。服务器容忍行尾 `\r\n`。 |
| **stdout** | **仅用于协议消息**。任何日志、调试信息一律走 `stderr`，防止污染协议流。 |
| **stderr** | 自由文本日志，Agent 可选择透传或丢弃。 |
| **编码** | UTF-8。 |
| **生命周期** | 服务器进程由 Agent 启动。**stdin EOF = Agent 要求优雅关闭**，服务器应在完成进行中的任务后退出。 |
| **就绪信号** | 服务器启动成功后，**第一条 stdout 消息必须是 `event.server_ready` 通知**（见 §5）。Agent 读取到该消息前不得发送任何请求。 |
| **启动失败** | 初始化失败（沙箱根不可创建、CLI 参数非法等）时：向 `stderr` 写明原因，以非零退出码退出，**不向 stdout 写任何字节**（Agent 不会收到 `event.server_ready`）。 |

> **为什么用 NDJSON 而不是 `Content-Length` 头（LSP 风格）**：单行 JSON 对 Agent 更友好——流式解析只需按 `\n` 切分，无需维护长度状态机，出错时也更容易定位坏消息。

---

## 2. 会话生命周期

```
Agent                                   Server
  │  (spawn 子进程)                        │
  │◄──────── event.server_ready ──────────│  ← 携带 capabilities，省掉 initialize 往返
  │                                       │
  │──────── request: file.write ─────────►│
  │◄──────── result: {bytes_written} ─────│
  │                                       │
  │──────── request: listener.start ─────►│
  │◄──────── result: {listener_id} ───────│
  │                                       │
  │◄──── event.inbound_request ───────────│  ← 设备发起 RRQ/WRQ
  │◄──── event.transfer_progress ─────────│
  │◄──── event.transfer_completed ────────│
  │                                       │
  │──────── request: server.shutdown ────►│
  │◄──────── result: {ok} ────────────────│
  │  (stdin close)                        │
```

**设计取舍**：省略 LSP 式的 `initialize` / `initialized` 往返。`event.server_ready` 直接携带完整 capabilities，Agent 一步即可获知服务器能力，节省一次 round-trip 的 token 和延迟。需要详细 schema 时再调 `server.describe`。本协议的 MCP stdio 前端见 §9——同一核心能力的另一套封装。

---

## 3. 消息类型总览

| 类别 | 命名空间 | 说明 |
| :--- | :--- | :--- |
| **请求** | `server.*` / `file.*` / `transfer.*` / `listener.*` / `task.*` | Agent → Server，有 `id`，必有 `result` 或 `error` |
| **通知** | `event.*` | Server → Agent，无 `id`，单向推送 |

所有请求方法名采用**点分小写命名**，语义为**意图导向**，而非协议原语（不暴露 RRQ/WRQ/ACK）。

MCP 模式（§9）下，同一组操作以 `tools/call` 工具形式暴露，方法名映射为下划线形式（如 `transfer.fetch` → `transfer_fetch`），参数 schema 完全一致。

---

## 4. 请求方法定义

### 4.1 `server.*` — 服务管理

#### `server.describe`
返回所有方法的 JSON Schema 与错误码清单。Agent 在启动时或需要自省时调用。

```json
// 请求
{"jsonrpc":"2.0","id":1,"method":"server.describe"}

// 结果（节选）
{
  "jsonrpc":"2.0","id":1,
  "result":{
    "protocol_version":"1.0",
    "server_version":"1.0.3",
    "methods":{
      "transfer.fetch":{
        "summary":"Pull a file from a remote TFTP server into the sandbox.",
        "params":{"type":"object","required":["host","remote_path","local_path"],"properties":{
          "host":{"type":"string","description":"IPv4 or hostname"},
          "remote_path":{"type":"string"},
          "local_path":{"type":"string","description":"Must be inside sandbox root"},
          "block_size":{"type":"integer","default":512},
          "timeout_ms":{"type":"integer","default":5000},
          "idempotency_key":{"type":"string"}
        }},
        "result":{"$ref":"#/definitions/TransferResult"}
      }
    },
    "error_codes":{ "-32002":"FILE_NOT_FOUND", "-32006":"REMOTE_TIMEOUT" }
  }
}
```

#### `server.status`
返回运行时状态：监听器列表、活动任务、沙箱用量。

```json
{"jsonrpc":"2.0","id":2,"method":"server.status"}

// 结果
{
  "jsonrpc":"2.0","id":2,
  "result":{
    "uptime_s":1423,
    "sandbox":{"root":"/var/lib/tftp-sandbox","used_bytes":10485760,"quota_bytes":5368709120},
    "listeners":[{"listener_id":"L-01","port":69,"mode":["read"],"root":"/firmware"}],
    "active_tasks":["T-7f3a","T-7f3b"]
  }
}
```

#### `server.shutdown`
优雅关闭。参数 `{"grace_ms": 5000}`。超时未完成的任务被标记为 `aborted`。

---

### 4.2 `file.*` — 沙箱文件操作

Agent 在传输前后管理沙箱内的文件。所有 `path` 参数都**相对于沙箱根目录**，含 `..` 的路径无条件拒绝（返回 `SANDBOX_VIOLATION`）。`file.write` 自动创建缺失的父目录。

| 方法 | 参数 | 结果 |
| :--- | :--- | :--- |
| `file.list` | `{path, recursive?:bool}` | `{entries:[{name,type,size,mtime}]}` |
| `file.stat` | `{path}` | `{name,type,size,mtime,sha256}` |
| `file.write` | `{path, content_base64, mode?}` | `{bytes_written, sha256}` |
| `file.read` | `{path, max_bytes?}` | `{content_base64, truncated}` |
| `file.delete` | `{path, recursive?:bool}` | `{deleted}` |

> **大文件建议**：`file.write` 适合 ≤ 1 MiB 的载荷。更大文件让 Agent 先生成到宿主机，再用 `transfer.push` 或把文件放进沙箱根目录后由服务器引用，避免 JSON 载荷膨胀。

---

### 4.3 `transfer.*` — 主动传输（服务器作为 TFTP 客户端）

服务器主动发起 RRQ / WRQ，适合 Agent 想从设备**拉取配置**或向设备**推送固件**。

#### `transfer.fetch`（RRQ：远端 → 本地沙箱）
#### `transfer.push`（WRQ：本地沙箱 → 远端）

**共享参数**：

```json
{
  "host":"192.168.1.10",
  "port":69,
  "remote_path":"config.bin",
  "local_path":"devices/192.168.1.10/config.bin",
  "block_size":512,
  "window_size":1,
  "timeout_ms":5000,
  "retries":3,
  "wait":false,
  "idempotency_key":"backup-192.168.1.10-20261105"
}
```

**`wait` 语义**：
- `wait: false`（默认）：立即返回 `{task_id, state:"pending"}`，进度与结果通过 `event.transfer_*` 推送。
- `wait: true`：阻塞直到进入终态（`success` / `failed` / `aborted`）或 `timeout_ms` 到期，返回完整 `TransferResult`。适合小文件、脚本化场景，Agent 一步拿到结果。MCP 模式下默认值为 `true`（§9.3）。

**`TransferResult`**：

```json
{
  "task_id":"T-7f3a",
  "state":"success",        // success | failed | aborted
  "bytes_transferred":524288,
  "duration_ms":1832,
  "sha256":"9f2c...",
  "error":null
}
```

`error` 为 `null`（成功 / 取消），或与 JSON-RPC `error.data` 同构的对象：`{"code":"REMOTE_TIMEOUT","retryable":true,"suggestion":"…","details":{…}}`。

> **传输失败不是 JSON-RPC error**：对端超时、网络不可达、收到 ERROR packet 等传输层失败，一律体现在 `TransferResult.state:"failed"` 与 `error` 字段（§6 错误分层）；JSON-RPC error 仅用于请求本身不合法（参数校验、沙箱越界等）。`wait:true` 的响应因此只有一种形状，Agent 无需双分支处理。

> **幂等性**：若提供 `idempotency_key`：
> - 同 key + 同参数、原任务**仍在进行**：直接返回原 `task_id` 与当前状态快照，**不发起第二次传输**（根治重试风暴）；
> - 原任务已终态且在 **10 分钟窗口**内：返回缓存结果（`"cached": true`），不再发起网络传输；
> - 同 key 但参数不同：返回 `IDEMPOTENCY_CONFLICT`；
> - 缓存对象是终态结果（含 `aborted`），需要强制重传时换新 key。
>
> 参数比对范围：`host / port / remote_path / local_path / block_size / window_size / timeout_ms / retries`（`wait` 与 `idempotency_key` 本身不参与比对）。

> **选项协商（RFC 2347）**：`transfer.*` 与监听器均支持 `blksize`（RFC 2348）、`tsize` / `timeout`（RFC 2349）、`windowsize`（RFC 7440）的双端协商。`event.transfer_progress` 的 `total_bytes` / `percent` 依赖对端提供 `tsize`，缺省为 `null`。
>
> **仅支持 octet 模式**：主动传输固定以 `octet` 发起；监听器收到 `netascii` 请求回 `ERROR(4, Illegal TFTP operation)`。

---

### 4.4 `listener.*` — 被动监听（服务器作为 TFTP 服务端）

Agent 预置文件并开放端口，等待设备主动来拉取或推送。这是固件部署、设备配置备份回传的核心场景。

#### `listener.start`

```json
{
  "jsonrpc":"2.0","id":10,"method":"listener.start",
  "params":{
    "port":69,
    "bind":"0.0.0.0",
    "root":"firmware/v2.3.1",
    "allow":{"read":true,"write":false},
    "max_concurrent":4,
    "allowlist":["192.168.1.0/24"]
  }
}
```

结果：`{"listener_id":"L-01","port":69,"state":"listening"}`

| 参数 | 说明 |
| :--- | :--- |
| `allow.read` | 允许设备 RRQ 下载 |
| `allow.write` | 允许设备 WRQ 上传（落到 `root/incoming/`） |
| `allowlist` | CIDR 列表，非白名单来源的请求在 TFTP 层直接拒绝（发送 `ERROR(2)` packet） |

> **路径安全**：设备请求的路径按沙箱规则归一化；含 `..`、绝对路径或其变体（URL 编码、反斜杠等）一律回 `ERROR(2, Access violation)` 并记入日志。
>
> **入站请求无交互式审批**：通过配置检查（allowlist / allow / 路径安全）即自动服务。TFTP 设备的重传超时通常只有数秒，等待 Agent 逐个确认会让传输饿死；需要审批语义时用更窄的 `allowlist` 表达。

#### `listener.stop` / `listener.status`
按 `listener_id` 停止或查询。`stop` 时若有进行中的传输，参数 `{"drain": true}` 表示等待完成再关闭。

---

### 4.5 `task.*` — 异步任务控制

| 方法 | 参数 | 说明 |
| :--- | :--- | :--- |
| `task.status` | `{task_id}` | 返回当前 `TransferResult` 快照 |
| `task.wait` | `{task_id, timeout_ms}` | 阻塞等待任务进入终态，超时返回 `state:"pending"` |
| `task.cancel` | `{task_id}` | 协作式取消。进行中的 TFTP 块传输在当前块完成后中止，返回 `state:"aborted"` |

> **`task.cancel` 是幂等的**：对已处于终态的任务调用，返回该终态而非报错（`"already_terminal": true`）。这消除了 Agent 在竞态下的重试负担。

---

## 5. 通知定义（Server → Agent）

所有通知遵循 `{"jsonrpc":"2.0","method":"event.xxx","params":{...}}`，**无 `id`**。

| 通知 | 触发时机 | 关键字段 |
| :--- | :--- | :--- |
| `event.server_ready` | 启动成功，**第一条消息** | `pid`, `protocol_version`, `server_version`, `capabilities` |
| `event.inbound_request` | 设备向监听器发起 RRQ/WRQ | `listener_id`, `peer`, `operation`(`read`/`write`), `path`, `task_id` |
| `event.transfer_progress` | 传输进度（每 N 块或 ≥1s 节流） | `task_id`, `bytes_transferred`, `total_bytes?`（需对端提供 `tsize`）, `percent?` |
| `event.transfer_completed` | 任务进入终态 | 完整 `TransferResult` |
| `event.listener_stopped` | 监听器因错误或 `stop` 停止 | `listener_id`, `reason` |
| `event.server_log` | 结构化日志（可选，`stderr` 之外） | `level`, `msg`, `ts` |

**`event.server_ready` 示例**（Agent 据此决定后续调用）：

```json
{
  "jsonrpc":"2.0","method":"event.server_ready",
  "params":{
    "pid":48213,
    "protocol_version":"1.0",
    "server_version":"1.0.3",
    "sandbox_root":"/var/lib/tftp-sandbox",
    "capabilities":{
      "transfer":["fetch","push"],
      "listener":{"read":true,"write":true,"allowlist":true},
      "tftp_options":["blksize","tsize","timeout","windowsize"],
      "idempotency":true,
      "max_concurrent_tasks":8
    }
  }
}
```

---

## 6. 错误码体系

JSON-RPC 标准码保留，业务错误用 `-32000 ~ -32099`。**每个 error 的 `data` 必须包含 `code`、`retryable`、`suggestion`**，让 Agent 能自主决定下一步。

错误分两个层级：**RPC 层**——请求本身不合法，走 JSON-RPC `error`；**任务层**——传输过程失败，走 `TransferResult.error`（结构与 `error.data` 同构）。任务层错误码的数字值仅作映射参考，不会以 JSON-RPC error 形式出现。

```json
{
  "jsonrpc":"2.0","id":10,
  "error":{
    "code":-32002,
    "message":"FILE_NOT_FOUND: config.bin not found in sandbox root 'firmware/v2.3.1'",
    "data":{
      "code":"FILE_NOT_FOUND",
      "retryable":false,
      "suggestion":"Call file.list on the listener root to verify the file name.",
      "details":{"path":"firmware/v2.3.1/config.bin"}
    }
  }
}
```

| code | 名称 | 层级 | retryable | 典型触发 |
| :--- | :--- | :--- | :---: | :--- |
| -32700 | PARSE_ERROR | RPC | 否 | 非法 JSON |
| -32600 | INVALID_REQUEST | RPC | 否 | 缺 `method` 或 `id` |
| -32601 | METHOD_NOT_FOUND | RPC | 否 | 未知方法 |
| -32602 | INVALID_PARAMS | RPC | 否 | schema 校验失败 |
| -32603 | INTERNAL_ERROR | RPC | 是 | 服务器内部异常 |
| -32001 | SANDBOX_VIOLATION | RPC | 否 | 路径越界、绝对路径 |
| -32002 | FILE_NOT_FOUND | RPC | 否 | 本地文件不存在（如 `file.stat`、`transfer.push` 源文件缺失） |
| -32003 | PERMISSION_DENIED | RPC | 否 | 监听器未开启 read/write |
| -32004 | TASK_NOT_FOUND | RPC | 否 | 未知 `task_id` |
| -32005 | QUOTA_EXCEEDED | RPC + 任务 | **是**（清理后） | 沙箱容量超限；fetch 中途超限同样使任务 `failed` |
| -32006 | REMOTE_TIMEOUT | 任务 | **是** | TFTP 对端无 ACK |
| -32007 | REMOTE_ERROR | 任务 | 视 `error_code` | 收到 TFTP ERROR packet（含 `tftp_error_code`） |
| -32008 | IDEMPOTENCY_CONFLICT | RPC | 否 | 同 key 不同参数 |
| -32009 | INVALID_STATE | RPC | 否 | 如对未启动监听器 `stop` |
| -32010 | PORT_IN_USE | RPC | **是**（换端口） | `listener.start` 绑定失败 |
| -32011 | NETWORK_UNREACHABLE | 任务 | **是** | DNS 解析失败、ICMP 不可达 / 端口拒绝 |

> **`REMOTE_ERROR` 透传 TFTP 错误码**：TFTP ERROR packet 的 `error_code`（0–8）原样放进 `details.tftp_error_code`，同时给出人类可读的 `details.tftp_error_name`（如 `DISK_FULL`、`FILE_EXISTS`），让 Agent 无需查阅 RFC 1350。
>
> **MCP 模式**：所有业务错误（含 RPC 层业务码）一律以 `isError:true` 的 `tools/call` 结果返回，JSON-RPC error 仅保留标准码（§9.3）。

---

## 7. 完整交互示例：固件推送 + 配置回传

**场景**：Agent 把 `firmware.bin` 放进沙箱，开监听器等待设备下载固件；设备下载完成后，Agent 主动从设备拉回 `config.bin` 做备份。

```jsonc
// 1. 服务器就绪
{"jsonrpc":"2.0","method":"event.server_ready","params":{
  "pid":48213,"protocol_version":"1.0","server_version":"1.0.3",
  "sandbox_root":"/srv/tftp","capabilities":{"listener":{"read":true,"write":true}}}}

// 2. Agent 写入固件（1 MiB 以内直接 base64）
{"jsonrpc":"2.0","id":1,"method":"file.write","params":{
  "path":"fw/v2.3.1/firmware.bin","content_base64":"<...>"}}
{"jsonrpc":"2.0","id":1,"result":{"bytes_written":786432,"sha256":"a1b2..."}}

// 3. 开启监听器
{"jsonrpc":"2.0","id":2,"method":"listener.start","params":{
  "port":69,"root":"fw/v2.3.1","allow":{"read":true,"write":false},
  "allowlist":["192.168.1.0/24"]}}
{"jsonrpc":"2.0","id":2,"result":{"listener_id":"L-01","state":"listening"}}

// 4. 设备来拉取固件
{"jsonrpc":"2.0","method":"event.inbound_request","params":{
  "listener_id":"L-01","peer":"192.168.1.10:49152",
  "operation":"read","path":"firmware.bin","task_id":"T-7f3a"}}
{"jsonrpc":"2.0","method":"event.transfer_progress","params":{
  "task_id":"T-7f3a","bytes_transferred":393216,"total_bytes":786432,"percent":50.0}}
{"jsonrpc":"2.0","method":"event.transfer_completed","params":{
  "task_id":"T-7f3a","state":"success","bytes_transferred":786432,"duration_ms":2140}}

// 5. Agent 主动拉回设备配置（同步等待结果）
{"jsonrpc":"2.0","id":3,"method":"transfer.fetch","params":{
  "host":"192.168.1.10","remote_path":"config.bin",
  "local_path":"backup/192.168.1.10/config.bin",
  "wait":true,"timeout_ms":10000,
  "idempotency_key":"backup-192.168.1.10-20261105"}}
{"jsonrpc":"2.0","id":3,"result":{
  "task_id":"T-7f3b","state":"success","bytes_transferred":4096,
  "duration_ms":620,"sha256":"c3d4...","cached":false}}

// 6. 拉取失败同样返回 result，不是 JSON-RPC error（错误分层见 §6）
{"jsonrpc":"2.0","id":4,"method":"transfer.fetch","params":{
  "host":"192.168.1.99","remote_path":"config.bin",
  "local_path":"backup/192.168.1.99/config.bin","wait":true,"timeout_ms":5000}}
{"jsonrpc":"2.0","id":4,"result":{
  "task_id":"T-7f4a","state":"failed","bytes_transferred":0,"duration_ms":15018,"sha256":null,
  "error":{"code":"REMOTE_TIMEOUT","retryable":true,
    "suggestion":"Verify the device TFTP service is up, then retry with the same idempotency_key.",
    "details":{"retries":3}}}}

// 7. 关闭
{"jsonrpc":"2.0","id":5,"method":"listener.stop","params":{"listener_id":"L-01","drain":true}}
{"jsonrpc":"2.0","id":5,"result":{"stopped":true}}
{"jsonrpc":"2.0","id":6,"method":"server.shutdown","params":{"grace_ms":5000}}
{"jsonrpc":"2.0","id":6,"result":{"ok":true}}
```

---

## 8. 设计决策速查

| 决策 | 选择 | 理由 |
| :--- | :--- | :--- |
| 分帧 | NDJSON 单行 | Agent 流式解析零状态、坏消息易定位 |
| 握手 | `event.server_ready` 自带 capabilities | 省一次往返，Agent 一步获知能力 |
| 任务模型 | 默认异步 + `wait:true` 同步捷径 | 大文件不阻塞，小文件一步到位 |
| 取消 | `task.cancel` 协作式，幂等 | 避免竞态下 Agent 的重试逻辑 |
| 错误 | 必有 `retryable` + `suggestion` | Agent 无需外部文档即可决策 |
| 错误分层 | 传输失败走 `result.error`，请求级错误走 JSON-RPC error | `wait:true` 响应单一形状，Agent 无双分支 |
| 错误透传 | `tftp_error_code` 原样保留 | 保留协议细节，同时给可读名 |
| 幂等 | `idempotency_key` + 在途去重 + 10 分钟结果缓存 | 网络抖动下 Agent 可安全重试 |
| TFTP 选项 | blksize / tsize / timeout / windowsize 协商 | 局域网吞吐量与进度可见性 |
| 传输模式 | 仅 octet | netascii 行结束转换是坑，设备场景几乎全是二进制 |
| 安全 | 沙箱路径强校验 + 监听器 allowlist | TFTP 无认证，边界必须在控制面强加 |
| 双前端 | 自定义 NDJSON + MCP stdio 共享同一核心（§9） | 一份实现，MCP 生态零成本接入 |
| wait 默认值 | 自定义协议 `false`，MCP 模式 `true` | 程序化调用方靠事件流，LLM 调用方一步拿结果 |

---

## 9. MCP 兼容模式（stdio）

同一二进制、同一核心（沙箱 / 传输 / 监听 / 任务），通过子命令选择协议前端：

| 子命令 | 前端 | 就绪信号 |
| :--- | :--- | :--- |
| `ferry serve`（默认） | 本协议（NDJSON JSON-RPC） | `event.server_ready` |
| `ferry mcp` | MCP stdio | `initialize` 响应（自带 capabilities） |

两种模式不在同一进程内混用。沙箱根、配额等通用选项由 CLI 参数传入，与前端无关。MCP 客户端配置示例：`{"command":"ferry","args":["mcp"]}`。

### 9.1 握手与能力

- 实现方法：`initialize`、`notifications/initialized`、`tools/list`、`tools/call`、`ping`、`logging/set_level`。
- `initialize` 响应：`protocolVersion`（客户端请求版本在支持列表内则原样回显，否则返回服务器支持的最新版本，最低基线 `2025-06-18`）；`capabilities: {"tools":{"listChanged":false},"logging":{}}`；`serverInfo: {"name":"ferry","version":"…"}`；`instructions` 简述沙箱模型与 TFTP 使用要点。
- MCP 模式下**不发送** `event.server_ready`。

### 9.2 工具映射

方法名 `.` → `_`，参数 schema 与本协议完全一致（`tools/list` 即自描述，`server.describe` 不再暴露）：

| 本协议方法 | MCP 工具 |
| :--- | :--- |
| `file.list` / `file.stat` / `file.read` / `file.write` / `file.delete` | `file_list` / `file_stat` / `file_read` / `file_write` / `file_delete` |
| `transfer.fetch` / `transfer.push` | `transfer_fetch` / `transfer_push` |
| `listener.start` / `listener.stop` / `listener.status` | `listener_start` / `listener_stop` / `listener_status` |
| `task.status` / `task.wait` / `task.cancel` | `task_status` / `task_wait` / `task_cancel` |
| `server.status` / `server.shutdown` | `server_status` / `server_shutdown` |

### 9.3 语义差异

- **`wait` 默认值**：MCP 模式下 `transfer_*` 默认 `wait: true`——LLM 调用方一步拿结果；可显式传 `false` 走异步 + `task_status` 轮询。大文件传输建议显式设置 `timeout_ms` 或改用异步，避免客户端工具调用超时。
- **结果**：`content[0].text` 恒为结果对象的 JSON 字符串；客户端 `protocolVersion` 支持结构化输出（≥ `2025-06-18`）时同时给出 `structuredContent`。
- **错误**：业务失败（§6 全部业务码，无论 RPC 层还是任务层）一律返回 `isError: true` 的 `tools/call` 结果，`text` / `structuredContent` 携带 `{code, retryable, suggestion, details}` 或含 `error` 的 `TransferResult`。JSON-RPC error 仅保留标准码（`-32700 ~ -32603`）。

### 9.4 通知映射

| 本协议事件 | MCP 通知 |
| :--- | :--- |
| `event.transfer_progress`（`wait:true` 且该次 `tools/call` 携带 `_meta.progressToken`） | `notifications/progress`（`progress` = 已传输字节，`total` = `total_bytes`，若有） |
| 其余 `event.transfer_progress` / `event.transfer_completed` | `notifications/message`（`level:"info"`，`data` 为原 `params`，同节流策略） |
| `event.inbound_request` / `event.listener_stopped` | `notifications/message`（`info` / `warning`） |
| `event.server_log` | `notifications/message`（对应 level） |

> 异步任务的完成通知依赖客户端日志通道：客户端未调用 `logging/set_level` 时可能丢弃 `notifications/message`。需要可靠获知异步终态的调用方，应以 `task_status` / `task_wait` 轮询为准。

### 9.5 生命周期

`ping` 返回空结果；`stdin EOF` = 优雅关闭，语义同 §1（MCP 客户端关闭 stdio 即触发）。