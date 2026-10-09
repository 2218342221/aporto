# 对话过程

对话由 thread、turn 和 item 组成。一次用户输入产生一个 turn；一个 turn 可包含多轮模型请求、助手中间说明、PTC 与多个工具调用，最终回答仍保存在 `Turn.output`。参考 Codex app-server 的消息 phase、item 生命周期与稳定 ID，公开过程记录与私有 Responses history 分开。

## 内容与状态

| kind | 内容 | 展示 |
| --- | --- | --- |
| `assistant_message` | `text`；`phase=commentary/final_answer` | 中间说明或最终回答，支持生成中更新 |
| `ptc_call` | `name=exec/wait`、`input/output/error` | 可展开的 JavaScript 源码或 wait 参数、执行结果 |
| `tool_call` | 内置或 MCP 工具的 `name/input/output/error` | 可展开的参数、命令和结果 |

所有 item 都包含 `id`、`turn_id`、`status`、`ordinal`、`sequence`、`created_at`、`updated_at`、`elapsed_ms` 和 `truncated`；不适用的可选字段输出 `null`。时间戳和耗时单位均为毫秒。ID 在一个 turn 内唯一，并发工具具有各自的记录。

`status` 为 `in_progress/completed/failed/interrupted`。非零命令退出码和 MCP `isError=true` 会显示为失败，即使工具传输成功。turn 失败、中断或 Core 重启会结束仍在执行中的 item；已经完成的步骤保留原状态。暂停/刷新浏览器不取消服务端执行。

exec/wait 的记录描述一次 PTC 观察调用。exec 返回 `running` 表示 cell 继续运行，本次观察调用已返回；后续 wait 是另一条记录。工具输出在调用返回后提供，当前不逐字节推送 shell stdout。

## HTTP 与 SSE

`GET /v1/threads/{thread_id}/turns/{turn_id}/items?after=0&limit=100` 对应 Core `item/list`。返回 `{items,next_cursor,has_more}`。按 `ordinal` 升序返回，`after` 是排他边界；空页保持请求游标。页大小还有序列化字节预算，不应假设返回条数等于 limit。

SSE 的 `item.started/updated/completed` 事件在 `data.item` 中携带完整累计快照。item 与事件在一个 SQLite 事务中提交，`item.sequence` 等于当前事件序号；`ordinal` 固定为首次创建时的事件序号。

客户端恢复流程：

1. 读取 thread/turn 快照，为当前显示的 turn 分页加载 items。
2. 同时接收持久事件，以 `(turn_id,id)` 为键合并；较旧或相同 sequence 的记录不能覆盖新记录。
3. 按 ordinal 排列，更新同一条记录的累计文本。禁止将快照文本再次拼接。
4. 重连使用最后收到的事件 sequence；重新打开页面可从 items 直接恢复，无需重放整个会话的所有 token 更新。

完成的 turn 优先渲染权威 `Turn.output`，即使 final item 尚未加载、被截断或包含多条最终消息，也只显示一份完整回答。执行中以及失败/中断时保留 final item 的累计或部分文本。

## 对话界面

用户消息靠右展示。运行初期以“正在思考”表示等待公开内容，不展示私有 reasoning；已有活动时显示处理耗时、中间说明和简洁操作摘要。成功完成后过程默认收起为“用时…”入口，最终回答保留在过程之外；可再次展开查看历史。耗时结束后冻结，刷新按持久化时间恢复。

相邻的 PTC 与工具记录组成可展开的操作组，摘要显示实际命令、文件或工具数量。展开后仍可查看按 ordinal 保留的 PTC 源码、输出及工具详情；该分组仅用于呈现，不推断协议中不存在的父子 ID。单独的 PTC 调用、错误和输出也可查看。正文和工具输出以安全文本渲染，原有 Markdown 安全边界不变。

已确认成功的文件写入可生成结果卡片，显示实际路径与字节数；“查看内容”展示该次写入的公开快照，可能带脱敏/截断标记，不代表重新读取当前容器文件。协议目前没有旧文件快照、diff 或撤销操作，因此不推导行数增减，也不显示无效操作按钮。

最终回答可复制；本地 HTTP 使用用户主动触发的浏览器复制回退。右侧原始进度面板默认收起，需要时可手动展开。活动分页、SSE 重放、刷新恢复和会话切换仍沿用前述协议。

## 模型传输与边界

模型请求使用 `stream=true`。增量 decoder 处理分片 UTF-8、SSE 换行与多行 data，读取公开 `output_text/refusal`，在 completed 后使用权威输出校正同一条记录。仍接受 Responses JSON 响应。部分、失败或截断响应不能启动工具；只在验证完整输出列表、工具类型、状态和 call ID 后执行 exec/wait。

助手文本采用累计快照和更新预算：单消息最多 32 次中间更新，单 turn 最多 300 次中间更新，完成快照始终发送。每个公开内容字段最多 32 KiB，截断通过 `truncated` 明示；工具和完整模型历史继续使用原有执行预算。item 独立分页，避免无限增加 thread/read 的响应大小。

工具结果超过输出预算时保留有界的 `exit_code`、`isError` 和 `status` 字段，预览与状态字段合计受同一字节预算限制。截断不会把命令失败或 MCP 错误改记为成功。

模型 key、显式 provider header、部署 MCP secret、AgentENV key 在公开内容持久化前按已知值脱敏。累计流中的凭据前缀暂不公开，避免跨事件拆分泄漏；结构化工具参数和结果中的常见凭据字段同样遮盖。这不能识别任务文件里的任意未知业务秘密；有权读取会话的操作者能够查看工具处理的任务内容。私有 history 保持模型协议所需的原始数据，按任务数据库保护。

所有部署凭据合并到同一个公开输出边界，按原文本中的重叠范围统一遮盖，避免顺序执行多个过滤器时先破坏另一凭据的完整匹配。Rust 调用者可通过 `ModelConfig.redacted_values` 提供额外值；CLI / ProductionExecutor 自动加入显式部署的 MCP / runtime 凭据，此字段不进入模型请求。

system/developer prompt、原始 reasoning、reasoning summary 和 encrypted_content 不属于公开过程协议。UI 的执行状态表示实际事件，不生成虚假的思考内容或子 Agent 活动。
