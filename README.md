# ferry

面向 AI Agent 的文件传输工具，当前支持 TFTP。一份核心、两个 stdio 前端：

| 子命令 | 前端 | 适用场景 |
| :--- | :--- | :--- |
| `ferry serve`（默认） | JSON-RPC 2.0 over NDJSON（[protocol.md](protocol.md)） | 程序化 Agent 宿主，事件流 + 异步任务 |
| `ferry mcp` | MCP stdio（protocol.md §9） | 任何 MCP 客户端（Claude Desktop、Cursor 等） |

## 能力

- **主动传输** `transfer.fetch` / `transfer.push`：ferry 作为 TFTP 客户端从设备拉取 / 向设备推送，支持 `wait:true` 同步等待与幂等键（在途去重 + 10 分钟结果缓存）
- **被动监听** `listener.start`：ferry 作为 TFTP 服务端等设备下载固件或回传配置；allowlist（CIDR）、读写开关、设备上传落 `root/incoming/`
- **沙箱文件操作** `file.*`：读写删列 + sha256/配额，路径强校验（拒 `..` / 绝对路径 / 符号链接逃逸）
- **任务系统** `task.*`：状态查询 / 等待 / 幂等取消
- **TFTP 选项协商**：blksize（RFC 2348）、tsize / timeout（RFC 2349）、windowsize（RFC 7440）；仅 octet 模式

## 构建与测试

```bash
cargo build --release   # 产物 target/release/ferry.exe
cargo test --workspace  # 单测 + loopback + e2e
cargo clippy --workspace --all-targets
```

## MCP 接入

```json
{
  "mcpServers": {
    "ferry": { "command": "C:/path/to/ferry.exe", "args": ["mcp"] }
  }
}
```

MCP 模式下工具名与方法名一一对应（`transfer.fetch` → `transfer_fetch`），`transfer_*` 默认等待完成；业务失败返回 `isError:true` 结果（携带 `retryable` / `suggestion`）。

## NDJSON 协议速览

Spawn `ferry serve` 后首条 stdout 消息为 `event.server_ready`（自带 capabilities），此后逐行 JSON-RPC：

```
→ {"jsonrpc":"2.0","id":1,"method":"file.write","params":{"path":"fw/v1.bin","content_base64":"..."}}
← {"jsonrpc":"2.0","id":1,"result":{"bytes_written":786432,"sha256":"a1b2..."}}
→ {"jsonrpc":"2.0","id":2,"method":"transfer.fetch","params":{"host":"192.168.1.10","remote_path":"config.bin","local_path":"backup/cfg.bin","wait":true}}
← {"jsonrpc":"2.0","id":2,"result":{"task_id":"T-0001","state":"success",...}}
```

传输失败不是 JSON-RPC error：`state:"failed"` 与 `error`（`{code, retryable, suggestion, details}`）出现在结果中（protocol.md §6 错误分层）。

## 常用参数

```
ferry serve|mcp [--sandbox-root <dir>] [--quota-bytes <n>] [--max-concurrent-tasks <n>] [--log-level <level>]
```

- 沙箱根默认 `~/.ferry/sandbox`（自动创建），配额默认 5 GiB
- 日志走 stderr，stdout 仅协议消息
- 监听器绑定 69 端口在 Linux 需要 CAP_NET_BIND_SERVICE（Windows 无限制）
- `stdin EOF` 即优雅关闭；进行中的任务有 5s 收尾窗口

## 布局

```
crates/ferry-tftp   TFTP 引擎（包编解码 + 会话 + 选项协商，文件系统无关）
crates/ferry-core   沙箱 / 任务 / 监听 / 错误模型 / 事件（前端无关）
crates/ferry        CLI + NDJSON / MCP 双前端
```
