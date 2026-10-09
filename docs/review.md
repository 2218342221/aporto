# 发布检查与已知边界

本页列出发布和部署前需要核对的行为，不作为某个版本已通过全部验收的证明。执行入口见 [验证指南](validation.md)，实现边界见 [架构](architecture.md)。

## 发布检查

| 范围 | 检查内容 |
| --- | --- |
| 构建与配置 | 所有示例可离线 build / inspect；重复构建摘要一致；Agentfile、部署和协议 Schema 与 Rust 类型一致 |
| 依赖与交付 | 锁文件已提交；CI 使用声明的 Rust / Node 版本；正常 release 构建不包含测试 fixture |
| PTC 与模型 | 模型仅看到 exec / wait；目标模型支持 Responses custom tools；不支持参数或不完整响应明确失败 |
| 发布契约 | activate 固定每个候选镜像和工具目录；指针替换前失败保留原 active 指针；旧会话仍加载原 release |
| 会话与实例 | 新建与复用、工作目录、共享实例串行执行、独立实例并行、取消和重启恢复符合协议 |
| Runtime | 在目标 Docker / AgentENV 上验证命令、文件、MCP、暂停/停止、恢复、失败清理及资源归属 |
| Client UI | 发送、中断、历史分页、过程展开、SSE 重连、移动布局和 Markdown 安全通过浏览器验证 |
| 公开内容 | README、文档、示例、截图和 Git 历史无凭据、私有服务地址或业务数据 |

每次发布应记录被测 commit、执行的命令、目标 runtime 和结果。真实模型、真实 Docker、AgentENV 协议 fixture 与真实 microVM 的结果分别报告；CI 成功不能替代目标部署验收。

## 当前部署边界

- **单租户、单 Core。** 一个 Server token 可访问该 state directory 的全部会话。没有多用户 RBAC、OIDC、HA 调度或多副本共享 SQLite。
- **PTC 在 Core 进程内运行。** QuickJS 有内存、CPU 和能力限制，但没有独立的 OS 进程隔离。面向不可信租户部署前，需要独立 worker 与凭据隔离。
- **构建输入需要受信任的快照。** 构建拒绝 symlink 和特殊文件，但不能抵御另一个进程在检查与打开之间恶意替换祖先目录。构建时应使用不可变或只读的输入。
- **持久资源没有自动 GC。** 当前没有会话删除/归档、实例销毁或 release 回收 API。部署者负责数据库、release、容器 rootfs 和 VM 存储的保留周期及配额。
- **Docker 没有 lease / TTL。** 强制退出或 daemon 请求结果未知时，容器可能仍在运行。必须按持久 ID 与 owner 核对资源归属后清理。
- **AgentENV 需要独立验收。** 仓库中的 local / remote 自动化测试使用 HTTP / Connect fixture；真实模板、envd、Firecracker、权限与 pause / connect 需要在实际服务上验证。协议 fixture 不证明真实 microVM 可用。
- **取消不回滚副作用。** 已写入的文件和外部 MCP 操作不会自动撤销。AgentENV 进程取消针对直接进程，不能保证清理全部后台子孙进程。
- **模型上下文有界。** 当前不自动压缩 history；达到预算时明确失败，需要新建会话。部分或中断的模型输出不会触发新的工具调用。
- **秘密识别有范围。** 公开事件会遮盖已知部署凭据和常见敏感字段，但不能识别任务内容里的任意业务秘密。会话、私有 history 和备份均需按任务数据保护。

## 保留的 TODO

共享实例的任务已经串行执行，但每个 turn 结束仍会 pause / stop，下一个 turn 再 connect / start。

**TODO：** 当同一实例已有等待任务时，由实例调度器把活动 runtime 交给下一轮，减少重复暂停/恢复。实现前必须保持实例互斥、上一轮 MCP / PTC 清理、取消与故障恢复的边界。对应代码位于 [`crates/core/src/executor.rs`](../crates/core/src/executor.rs)。

其他后续能力包括独立 PTC worker、审计导出、artifact / volume 管理、上下文压缩和签名分发；它们均不属于当前发布的接口承诺。
