# Docker runtime

Docker 是本机容器 backend。Agentfile 固定 `provider = "docker"`、默认 image、可选 images 和默认 workdir；Core 的部署配置固定 Docker CLI、Unix socket 和资源限额。它不需要 AgentENV 服务、API key 或 `/dev/kvm`。AgentENV local / remote 使用模板与服务协议，两种 provider 不会在运行失败时相互回退。

## 镜像与前置条件

需要可访问本机 daemon 的 Docker CLI。仅接受绝对路径的 `unix://` socket；默认 `unix:///var/run/docker.sock`。不读取 Docker context 来切换目标，不支持 TCP/SSH 远端 daemon。

镜像必须已存在于选定 daemon，并提供 Python 3、POSIX `sh`、可写的 `/workspace` 和 `/opt/agent`。镜像不能声明 `VOLUME`，以免 Docker 隐式创建匿名卷；运行器在创建前拒绝这类镜像，并检查启动条件，不自动 pull、安装依赖或降级执行宿主 Shell。先显式准备示例镜像：

```bash
docker --host unix:///var/run/docker.sock pull python:3.12-slim
```

Agentfile 示例：

```toml
# All resource paths are relative to this build context.
[agent]
name = "docker-reviewer"

[model]
name = "YOUR_MODEL_ID"
connection = "primary"

[runtime]
provider = "docker"
image = "python:3.12-slim"
# images = ["python:3.11-slim"] # 额外选项；需提前 pull
workdir = "/workspace"

[[prompts]]
source = "prompts/reviewer.md"

[[skills]]
source = "skills/code-review"

[[mcp]]
name = "policy"
source = "mcp/policy"
command = ["python3", "server.py"]
```

完整资源位于 [examples/docker-reviewer](../examples/docker-reviewer/README.md)。`YOUR_MODEL_ID` 需替换成服务实际允许、支持 Responses custom tools 的模型 ID。image 支持 tag、`repository@sha256:<digest>` 和 `sha256:<image-id>`；tag 可以变化，正式运行宜固定实际 digest。Dockerfile 用于另外构建解释器和系统依赖镜像，Agentfile 用于编译配置与资源 bundle；两者的产物不能混用。

## 构建、激活和运行

```bash
cargo build --release --workspace --locked
./target/release/aporto build examples/docker-reviewer \
  -o examples/server/docker-reviewer.agent.json

# 配置 OPENAI_API_KEY 与 deployment 中的完整 Responses endpoint。
./target/release/aporto activate \
  --config examples/server/deployment-docker.toml --agent docker-reviewer
./target/release/aporto run \
  --config examples/server/deployment-docker.toml --agent docker-reviewer \
  --workspace /path/to/project --task '审查项目并将结论写入 review.md' \
  --export review.md=./review.md
```

Docker 连接和资源设置来自部署 TOML 的 `[agents.runtime]`，模型/API 参数不通过 run flags 覆盖。使用 rootless Docker 时设置 `host = "unix:///run/user/1000/docker.sock"`，并向同一个 daemon 提前准备镜像。

| 部署字段 | 默认值 | 作用 |
| --- | --- | --- |
| `host` | `unix:///var/run/docker.sock` | 本机 daemon socket |
| `binary` | `docker` | CLI 程序路径 |
| `memory_mb` | `512` | 容器内存 MiB，范围 64–1048576 |
| `cpus` | `2` | CPU 上限，范围 1–1024 |
| `pids_limit` | `128` | PID 上限，范围 16–65536 |
| `network` | `none` | 仅支持 none 或 bridge |

runtime 字段不包含 provider/image，也不接受 AgentENV 的 api_key、api_url、lease_ms 或 mode。构建器只打包资源，activation 逐个解析允许的镜像 ID，创建临时容器安装 bundle、初始化 MCP 并分别锁定工具契约，全部删除成功后才发布 release。服务启动不自动完成这些动作。

`--workspace` 先完成有界本地快照再上传，不 bind-mount 宿主目录。`/opt/agent` 安装经过校验的 bundle；MCP 只取得声明的环境变量和秘密，模型 key 和宿主完整环境不会传入容器。

## Core / Server 与持久工作区

完成上面的 activation 后使用同一部署配置启动：

```bash
# 另需设置至少 32 字节随机值的 APORTO_SERVER_TOKEN。
./target/release/aporto-server \
  --core-bin ./target/release/aporto-core \
  --core-config examples/server/deployment-docker.toml \
  --state-dir .aporto/docker-state \
  --listen 127.0.0.1:8080 --allow-origin http://localhost:5173
```

新会话可新建实例或选择该 Agent 已创建的实例；新实例在首轮分配容器。复用保留原 release 和镜像，共享文件但不复制对话历史。同实例的会话串行执行，包括轮末清理；不同实例可并行。

工作路径由 Agentfile 的 `workdir` 提供默认值（省略为 `/workspace`），新会话可指定非根绝对容器路径。复用默认沿用实例的初始目录，也可为新会话指定其他目录。目录影响命令 cwd、文件工具和模型会话上下文，不影响打包 MCP 的 `/opt/agent`。CLI 可用 `--image` / `--workdir` 覆盖默认选项。

Core 在每轮创建或恢复相同 container ID，轮末关闭 PTC/MCP 后执行 Docker stop；下轮 start 保留文件。进程内存、MCP 连接和 PTC store 不跨轮保留。HTTP/SQLite 的 `sandbox_id` 在 Docker 下保存 container ID。

线程固定 release ID，恢复时使用该 release 的原始 bundle、runtime 配置和 image ID，并核对容器受管标签及 owner。容器缺失或归属/镜像不匹配时失败，不新建替代工作区。更新 bundle / deployment 后重新激活并受控重启服务，新线程使用新 release；已有线程和可变 image tag 后续变化互不影响。

## 隔离、网络与清理

默认 `network=none`，无 host bind mount，丢弃全部 Linux capabilities，并设置 `no-new-privileges`、内存、CPU 和 PID 限额。容器内 `exec_command` 和 stdio MCP 的联网受容器网络限制；模型 HTTP 和已声明 HTTP MCP 由宿主上的引擎连接，不因容器 `network=none` 而断开。

Docker 共享宿主内核，不提供 AgentENV microVM 的隔离边界，也没有租约/TTL 自动停止机制。Docker daemon 的访问权限由部署者控制，不能交给浏览器或工作区内容选择。运行器不把 daemon socket 挂入任务容器。

单任务 CLI 正常完成后删除容器；服务模式 stop 后保留 rootfs。导出失败时 CLI 会尝试 stop、保留 container ID 并报错，停止失败也不自动删除。Ctrl-C 会走显式清理；如果进程被强杀、宿主崩溃或 Docker daemon 不可用，需要运维按记录的 ID 检查残留容器，不能依赖自动过期。

停止的容器可直接取回文件，无需重新运行任务：

```bash
# 将 container_id 设置为 CLI 错误或该聊天 sandbox_id 中记录的准确 ID。
docker --host unix:///var/run/docker.sock inspect "$container_id"
docker --host unix:///var/run/docker.sock cp "$container_id:/workspace/review.md" ./review.md
```

确认已保存产物且不再继续该聊天后，可对这个准确 ID 执行 `docker stop` / `docker rm`。删除后既有聊天无法恢复原工作区；不要批量删除仅凭通用 managed 标签筛选出的其他任务容器。

## 验证入口

真实容器测试默认 ignored，须显式启用并预拉镜像；不需要模型 key：

```bash
docker --host unix:///var/run/docker.sock pull python:3.12-slim
APORTO_DOCKER_TESTS=1 cargo test --locked --test docker_runtime -- --ignored --nocapture
cargo build --workspace --locked
python3 scripts/e2e_stack.py --runtime docker
```

前者覆盖文件、命令、stdio、取消、资源限额、ownership 和 stop/start；后者运行实际 Server/Core、PTC、打包 skill 与 stdio MCP，并验证跨轮文件和重启后恢复。默认模型是 fixture，只有另设 `APORTO_LIVE_*` 并加 `--live` 才调用真实模型。

CI 配置包含预拉镜像和验证命令。完整入口与覆盖范围见 [验证指南](validation.md)；发布时应分别记录真实 Docker、真实模型和 AgentENV wire fixture 的结果。
