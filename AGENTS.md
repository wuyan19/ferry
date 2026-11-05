# ferry

面向 Agent 的文件传输工具（当前 TFTP）。一份核心、两个 stdio 前端：`ferry serve`（JSON-RPC/NDJSON）与 `ferry mcp`。协议规范在 [docs/protocol.md](docs/protocol.md)——**行为语义问题先查它**，暂缓/候选功能在 [docs/roadmap.md](docs/roadmap.md)。

## 布局与边界

| 目录 | 职责 | 硬边界 |
| :--- | :--- | :--- |
| `crates/ferry-tftp` | TFTP 引擎：包编解码、RFC 2347/2348/2349/7440 选项协商、客户端/服务端会话 | 文件系统无关：源 `AsyncRead`、汇 `AsyncWrite` |
| `crates/ferry-core` | 沙箱、任务管理、transfer/listener 编排、错误模型、事件总线 | 不得依赖任何前端；前端只经 `Api` 门面调用 |
| `crates/ferry` | clap CLI + `frontend/ndjson.rs` + `frontend/mcp.rs` | 只做协议翻译，业务语义一律在 core |

## 常用命令

```bash
cargo build --release
cargo test --workspace              # 44 测试：单测 + loopback + e2e
cargo clippy --workspace --all-targets   # 零警告是硬标准
# 聚焦：cargo test -p ferry-tftp --test loopback | -p ferry-core --test transfer|listener | -p ferry --test ndjson_e2e|mcp_e2e
# 冒烟：echo '{"jsonrpc":"2.0","id":1,"method":"server.status","params":{}}' | target/release/ferry.exe serve --sandbox-root <dir>
```

## 改动时的硬规则

- **错误分层**（protocol.md §6）：传输失败是任务层错误，进 `TransferResult.error`，绝不作 JSON-RPC error；RPC error 只用于请求本身不合法。
- **双前端同步**：新增 Api 方法需要四处一致——core 实现、`schema.rs` 的 METHODS（`server.describe` 与 MCP `tools/list` 的单一来源）、`ndjson.rs` dispatch、`mcp.rs` tool_call。方法名 `.` ↔ MCP 工具名 `_`。
- **schemars**：嵌套参数结构必须 `#[schemars(inline)]`——部分 MCP 客户端不解析 `$defs`/`$ref`（ZCode 宿主就会把字段退化成 string）。
- **stdout 只有协议消息**（经共享 `LineWriter` 单写者）；日志一律 stderr tracing。请求按行并发处理，响应顺序 ≠ 请求顺序，客户端按 id 匹配——不要引入顺序假设。
- **沙箱路径**：拒 `..`/绝对路径/反斜杠/盘符 + 物理包含检查；`file.write` 自动建父目录；配额增量记账。
- **TFTP**：仅 octet（netascii 回 `ERROR(4)`）；16 位块号回绕与重复块去重在 `session.rs`，改收发循环要跑全量 loopback 测试。
- release profile 有意**不开** `panic = "abort"`（长驻服务靠任务系统兜 panic）。

## 环境相关的坑

- 本机 UDP:69 被 Tftpd64 占用——监听器测试用 1069 或随机端口。
- Windows 本地交叉 musl 需 `--config 'target.<t>.linker="rust-lld"'`（无 cc）；CI 的 ubuntu runner 不需要。
- 真机测试台架：COM3 串口接 OpenWrt（aarch64，192.168.21.46），PC 为 192.168.21.72，经 serial-studio MCP 驱动；设备 tftp 客户端支持 `--option "blksize N"`。
- 发布流程：推 `v*` tag（必须等于 workspace version），工作流跑测试闸门→版本守卫→5 目标构建（含 musl 静态）→草稿 release 待人工核对。
