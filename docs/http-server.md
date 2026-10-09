# HTTP Server 与 Core 进程协议

Server 是 HTTP 适配层。它通过 `aporto-core-client` 启动一个独立的 `aporto-core` 子进程，用 stdin/stdout JSON-RPC 交换请求和响应。Server 不依赖执行引擎，不读取 SQLite，也不接收浏览器提交的 bundle 路径、模型 endpoint、运行时 endpoint 或凭据。

配置、bundle、环境变量中的密钥以及 state directory 由部署者管理。当前 bearer token 对应一个共享操作身份，拥有该 Server 下所有聊天的权限；这不是按用户隔离的多租户授权系统。

## 启动

```bash
cargo build --release --workspace --locked

# 由终端或服务管理器设置 APORTO_SERVER_TOKEN，至少 32 字节。
# Core 配置需要的模型、AgentENV 与 MCP 密钥同样由环境变量注入。
# 先 build bundle，并通过 activate --config FILE --agent ID 激活。
./target/release/aporto-server \
  --core-bin ./target/release/aporto-core \
  --core-config ./examples/server/deployment-agentenv-local.toml \
  --state-dir ./var/aporto \
  --listen 127.0.0.1:8080 \
  --token-env APORTO_SERVER_TOKEN \
  --allow-origin http://localhost:5173
```

如果 activation 使用自定义发布目录，Server 也需用 `--releases-dir DIR` 指向同一目录；默认使用部署文件旁的 `releases/`。重新激活后受控重启 Server/Core，已有线程继续加载原 release。

`--allow-origin` 可重复指定。默认不允许任何跨 Origin 浏览器请求；无 Origin 的服务端请求仍可使用 bearer 认证。必须指定完整 Origin，例如 `https://aporto.example.com`，不能使用通配符、路径或结尾 `/`。

Core 启动参数使用独立 argv 传递，路径中的空格不会被 shell 展开。普通 release 构建不包含测试 fixture binary；协议和 HTTP 测试使用 `cargo test --workspace --all-features` 显式开启 `test-fixtures`。

## 选择 AgentENV 或 Docker

运行环境由 Core 部署配置与已验证 bundle 共同决定，浏览器/API 请求只可从 Agentfile 允许的镜像/模板中选择，不能改变 provider 或 daemon socket。Agentfile `[runtime]` 固定 provider、默认和可选镜像/模板及默认工作路径，部署 TOML 只提供连接及资源配置，不重复 provider。AgentENV local 与 remote 分别有独立部署示例。

Docker 部署使用 [`deployment-docker.toml`](../examples/server/deployment-docker.toml)，只需模型凭据和 Server token，无需 AgentENV key 或 KVM。先显式拉取镜像并构建 Docker bundle：

```bash
docker --host unix:///var/run/docker.sock pull python:3.12-slim
./target/release/aporto build examples/docker-reviewer \
  -o examples/server/docker-reviewer.agent.json
./target/release/aporto activate \
  --config examples/server/deployment-docker.toml --agent docker-reviewer
./target/release/aporto-server \
  --core-bin ./target/release/aporto-core \
  --core-config examples/server/deployment-docker.toml \
  --state-dir .aporto/docker-state --listen 127.0.0.1:8080 \
  --token-env APORTO_SERVER_TOKEN --allow-origin http://localhost:5173
```

Docker 的 Core `runtime` 配置示例：

```toml
[agents.runtime]
host = "unix:///var/run/docker.sock"
binary = "docker"
memory_mb = 512
cpus = 2
pids_limit = 128
network = "none"
```

可选镜像来自 bundle 的 `[runtime].image` 和 `images`，activation 逐个解析为不可变 image ID，并固定各自的工具目录。Docker 配置不接受 AgentENV 的 mode、endpoint、key 或 lease 字段。Docker 当前只支持本机 Unix socket，镜像必须预先存在，不会自动 pull。

HTTP 合约和 UI 无需另开 Docker 接口。协议字段 `sandbox_id` 在 Docker 下保存 container ID；轮末 stop 保留文件，下一轮 start 同一个容器，Core 重启后仍使用持久化 ID。缺失容器或 owner/image 不匹配时不创建替代工作区；bundle / deployment 更新需重新激活，已有线程仍绑定原 release。

默认 Docker 网络为 `none`。容器共享宿主内核，没有 VM 隔离或 TTL 自动停止；异常退出后需按记录 ID 检查残留计算资源。完整配置、导出与清理说明见 [Docker runtime](docker-runtime.md)。

## HTTP 合约

除两个健康检查外，以下 API 都要求 `Authorization: Bearer <token>`。token 不从 URL、cookie 或请求 JSON 读取。请求和响应字段使用 snake_case，完整数据类型由 `aporto-protocol` 导出。

| 方法和路径 | 输入 | 返回 |
| --- | --- | --- |
| `GET /healthz` | 无认证 | Server 存活，`200 {"status":"ok"}` |
| `GET /readinessz` | 无认证 | Core 可用且 Server 未关闭时 200，否则 503 |
| `GET /v1/agents` | 无 | `AgentListResult {agents}` |
| `GET /v1/agents/{id}/instances` | query `limit`, `cursor` | `RuntimeInstanceListResult {instances,next_cursor}` |
| `GET /v1/threads` | query `limit`, `cursor` | `ThreadListResult {threads,next_cursor}` |
| `POST /v1/threads` | `{agent_id,title?,runtime?,workdir?}` | 201，`Thread` |
| `GET /v1/threads/{id}` | query `limit`, `before` | `ThreadReadResult {thread,turns,next_cursor}` |
| `POST /v1/threads/{id}/turns` | `{input,idempotency_key}` | 202，`Turn` |
| `POST /v1/threads/{id}/turns/{turn}/interrupt` | 无 | 200，更新后的 `Turn` |
| `GET /v1/threads/{id}/events/page` | query `after`, `limit`；可选 `Last-Event-ID` | `EventListResult {events,next_cursor,has_more}` |
| `GET /v1/threads/{id}/turns/{turn_id}/items` | query `after`, `limit` | `ItemListResult {items,next_cursor,has_more}` |
| `GET /v1/threads/{id}/events` | 相同 cursor 参数 | 持续 SSE |

### 新建会话与复用实例

`GET /v1/agents` 的每个 Agent 包含 `runtime: {provider,images,default_image,default_workdir}`；`images` 包含默认项，AgentENV 下值为模板 ID。客户端只展示这些选项。

```json
{"agent_id":"reviewer","runtime":{"mode":"new","image":"python:3.12-slim"},"workdir":"/projects/demo"}
```

省略 `runtime` 等同 `{"mode":"new"}`，省略 `image` 和 `workdir` 使用 Agent 默认值。创建会话先持久化实例记录，首轮才真正分配容器；空会话不占用容器。

```json
{"agent_id":"reviewer","runtime":{"mode":"reuse","instance_id":"<instances 返回的 id>"},"workdir":"/projects/demo"}
```

复用仅接受该 Agent 已创建、具有实际 `sandbox_id` 且原 release 可加载的实例。原 release、镜像和 bundle 均沿用实例记录；请求不能再指定 image。省略工作路径时使用实例创建时的默认目录，填写时仅影响新会话。实例列表分页返回 `id,agent_id,release_id,bundle_digest,provider,image,sandbox_id,workdir,busy,created_at,updated_at`。列表是注册记录，不主动探测或接管外部容器；容器被外部删除时执行会明确失败。

`Thread` 返回 `runtime_instance_id,runtime_provider,runtime_image,workdir`；其 `sandbox_id` 反映共享实例最新持久 ID。复用从空对话历史开始，共享实例文件。同实例所有会话的 turn 串行执行，锁持有到 PTC/MCP、runtime 清理和结果提交完成；其他实例可继续并行。`busy` 表示已有 queued/running/cancelling turn，可选择该实例并排队。

工作路径必须是非根绝对容器路径，默认 `/workspace`，支持 `/projects/demo` 等自定义目录；拒绝 `..`、控制字符和 runtime 保留目录。命令 cwd、文件工具及每轮模型上下文使用所选路径；目录不存在时创建。MCP 安装目录仍为 `/opt/agent`，工作区配置不会自动加载。

创建 turn 的 `idempotency_key` 必填，最长 128 字节。同一次提交重试时必须保留 key 和 input，避免网络超时后重复执行。Core 决定幂等与冲突语义，Server 不自动重试有副作用的 RPC。未知 JSON 字段会被拒绝。

聊天详情默认返回最近 20 个 turn，页内按时间升序。使用返回的 `next_cursor` 作为下一次请求的 `before` 读取更早记录。事件接口使用独立的递增数字 sequence，默认每页 100，HTTP 接口支持 1–200。

## SSE 与恢复

浏览器应使用带 Authorization header 的 `fetch()` 读取流；原生 `EventSource` 不能直接设置该 header。不要把 token 放进 SSE URL。

```text
event: agent_event
id: 42
data: {"sequence":42,"thread_id":"...","turn_id":"...","kind":"turn.completed","data":{},"created_at":...}

```

每条 `agent_event` 的 data 是完整 `Event`。客户端保存已处理的 sequence，重连时发送 `?after=42` 或 `Last-Event-ID: 42`。两者同时存在时，query `after` 优先。`after` 是排他边界，已处理事件不会再次返回。

`item.started`、`item.updated`、`item.completed` 的 `data.item` 是完整过程记录，包含累计文本与工具详情。首次打开或刷新时按 turn 读取 items；SSE 按 `(turn_id,id)` 合并，只有更大的 `sequence` 可以替换已有记录。items 分页的 `after` 使用首次创建的 `ordinal`，与 SSE 的更新 sequence 不同。详见 [对话过程协议](conversation.md)。

Server 调用 Core 的 `event/list` 做持久化事件重放。发送前校验整页的会话归属、递增 sequence、页大小，以及 `next_cursor` 恰好等于最后交付的 sequence；空页不能声称还有下一页。存在下一页时立即续取，否则每 250 ms 查询一次；空闲连接每 15 秒发送 SSE keep-alive comment。SSE 断连只停止该订阅，不中断 turn。用户明确中断时才调用 interrupt API。

建立流前的 Core 错误返回普通 HTTP 错误。流建立后遇到 Core 故障，会发送 `server_error` 事件并关闭流，data 使用下面的 error envelope。客户端保留已处理 cursor，按错误 code 决定恢复：例如 `-32010` 暂时不可用和 `-32029` 过载适合退避重连，资源不存在或认证错误需要先处理原因。`server_error` 不推进事件 cursor。

## 错误和资源上限

Server 错误使用统一 JSON：

```json
{"error":{"code":-32004,"message":"configured agent not found"}}
```

| HTTP 状态 | 情况 |
| --- | --- |
| 400 | JSON、query、资源 ID 或参数无效 |
| 401 | bearer 缺失或无效，响应包含 `WWW-Authenticate: Bearer` |
| 403 | Origin 不在部署者 allowlist 中 |
| 404 | 路由或 Core 资源不存在 |
| 408 | HTTP 请求超过处理 deadline，包括慢速 body 上传 |
| 409 | 幂等 key/input、bundle 或其他 Core 状态冲突 |
| 413 | JSON body 超过 1 MiB |
| 429 | Server 请求、SSE 订阅或 Core 请求/队列达到上限 |
| 502 | Core 返回内部或协议错误 |
| 503 | Core 进程不可用或 Server 正在关闭 |
| 504 | Core RPC 超时；操作是否已提交可能未知 |

默认最多 128 个普通 HTTP 请求和 64 个 SSE 订阅。SSE permit 持有到整个流结束，不占普通请求 permit。HTTP 处理 deadline 为 30 秒，包含读取 body；SSE 只限制握手阶段，不限制整个连接时长。

Core client 限制 128 个 pending RPC、128 个待写 frame，每个 JSONL frame 连同换行最多 2 MiB，默认 RPC deadline 为 30 秒。独立写任务保证 HTTP/SSE 调用者取消时不会留下半个 JSONL frame。迟到响应被丢弃；子进程退出或协议破坏会使所有 pending RPC 失败，不会自动重新启动 Core 或重放请求。

bearer token 要求 32–4096 个可见 ASCII 字符，不包含空白；比较使用固定长度 SHA-256 摘要和常量时间比较。认证 scheme 大小写不敏感，重复 Authorization header 会被拒绝。认证响应和 API 响应设置 `Cache-Control: no-store`。Server 不在 HTTP 错误中转发未知 Core 内部错误详情。

## TLS、代理和 systemd

Server 自身提供 HTTP，默认只监听 loopback。远程部署应由反向代理终止 TLS，并避免记录 Authorization header。SSE 需要关闭响应 buffering，并设置足够长的读超时。例如 nginx 的 API location：

```nginx
location /v1/ {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Authorization $http_authorization;
    proxy_buffering off;
    proxy_read_timeout 3600s;
}
```

Server 同时处理 Ctrl-C 和 Unix SIGTERM：停止接受连接、拒绝新的 API 请求、readiness 变为 503，并停止 SSE poll；最多等待 HTTP drain 10 秒，然后通过 shutdown RPC 请求 Core 清理。Core client 等待子进程真正退出，默认 cleanup 等待窗口 45 秒，超过窗口才强制结束子进程。Core 的默认执行清理窗口是 30 秒。

Server 也持续等待 Core 子进程的退出通知。未收到正常关闭信号时，Core 的任何退出（包括退出码 0）都会触发相同的 admission/SSE 停止及最多 10 秒 HTTP drain，随后 Server 以非零状态退出，使 `Restart=on-failure` 能重新启动整套服务。正常关闭信号优先于同时到达的 Core 退出通知，正常 shutdown 成功时 Server 返回 0。Server 不在进程内重启 Core，也不重放已提交的请求；重启后的持久化恢复由 Core 负责。

```ini
[Unit]
Description=Aporto Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=aporto
WorkingDirectory=/opt/aporto
EnvironmentFile=/etc/aporto/secrets.env
ExecStart=/opt/aporto/bin/aporto-server --core-bin /opt/aporto/bin/aporto-core --core-config /etc/aporto/deployment.toml --state-dir /var/lib/aporto --listen 127.0.0.1:8080 --allow-origin https://aporto.example.com
Restart=on-failure
TimeoutStopSec=120
KillMode=control-group
UMask=0077

[Install]
WantedBy=multi-user.target
```

部署者需提前创建专用用户、配置文件和可写 state directory，并限制 secrets 文件权限。健康检查只报告 liveness/readiness，不暴露 agent 清单、配置、文件路径或密钥。Core 崩溃后的重启由服务管理器负责；持久化恢复与异常 turn 的处理由 Core 完成。

Docker 部署还需给这个服务用户配置所选 Unix socket 的访问权限，并提前加载镜像；不要将 daemon socket 暴露给 Client 或任务容器。systemd 重启 Server/Core 不等于清理全部 Docker 容器，服务工作区会按持久 ID stop/start；无法恢复的残留容器应由部署者核对归属后处理。AgentENV 部署继续使用其 lease/autoPause 机制。
