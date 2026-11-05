# Roadmap

已定但暂缓或候选的功能，完成一项后从本表删除。

| 功能 | 动机 | 形态要点 | 状态 |
| :--- | :--- | :--- | :--- |
| MCP Streamable HTTP 前端 | 多 Agent 共享同一实例；Agent 与 ferry 不同机 | 见下节 | 暂缓（2026-11-05 决定） |
| `file.import` | 大文件入沙箱免拷贝 | 从宿主机路径导入，同卷用硬链接实现零拷贝；跨卷回退为复制 | 候选 |
| 进度通知配置 | 部分 MCP 客户端把 `notifications/message` 注入上下文，长传输事件多 | listener/transfer 增加通知间隔或开关参数 | 候选 |
| SFTP / SCP 传输方式 | 覆盖需要认证与加密的场景 | 在 ferry-core 增加并列的传输编排，复用任务/沙箱/事件模型 | 候选 |
| crate 发布与 CI | 供其他项目复用 `ferry-tftp` 引擎 | 发布到 crates.io；GitHub Actions 跑 test + clippy | 候选 |

## MCP Streamable HTTP 前端

当前双前端（NDJSON / MCP stdio）的消息处理已与传输层解耦，HTTP 化只需新增前端模块：

- HTTP 服务器（axum）+ `Mcp-Session-Id` 会话管理，事件经 SSE 流推送
- 启用 MCP 规范的鉴权（HTTP 模式标配）
- `ferry-core` 不动；预计 300~500 行

单机单 Agent 场景继续用 stdio，两种前端并存。
