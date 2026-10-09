# Agentfile 完整示例

本目录包含标准 TOML Agentfile、可离线打包的资源，以及 Docker / AgentENV 部署绑定。

| 文件 | 用途 |
| --- | --- |
| [Agentfile](Agentfile) | 单模型、Docker runtime、Prompt、Skill、stdio MCP、资产与限额 |
| [Agentfile.multiple-images](Agentfile.multiple-images) | 两个可选 Docker 镜像，以及容器内工作目录 `/work/review` |
| [deployment-docker.toml](deployment-docker.toml) | Docker daemon 和 Responses 连接 |
| [deployment-agentenv-local.toml](deployment-agentenv-local.toml) | 本地 AgentENV 连接 |
| [deployment-agentenv-remote.toml](deployment-agentenv-remote.toml) | 远端 AgentENV API / sandbox 连接 |
| [resources/](resources/) | reviewer prompt、code-review skill、无第三方 Python 依赖的 policy MCP、参考文档 |

## 构建与发布

先把 `[model].name` 的 `YOUR_MODEL_ID` 换成所用服务实际可用、支持 Responses custom tools 的模型 ID，修改 deployment 的完整 Responses endpoint，并设置 `OPENAI_API_KEY`。从仓库根目录执行：

```bash
cargo build --release --workspace --locked
mkdir -p examples/agentfile/dist
docker --host unix:///var/run/docker.sock pull python:3.12-slim
./target/release/aporto build examples/agentfile \
  -o examples/agentfile/dist/reviewer-docker.agent.json
./target/release/aporto activate --config examples/agentfile/deployment-docker.toml --agent reviewer
./target/release/aporto run --config examples/agentfile/deployment-docker.toml --agent reviewer \
  --workspace /absolute/path/to/project --task '审查代码'
```

`[model]` 进入 bundle，connection 引用 deployment 的 `[connections.primary]`。可选 reasoning/text 参数原样发送，目标模型必须支持；不需要时删除对应字段。connection 只提供完整 Responses endpoint、认证、超时和 headers。

多镜像示例需要先准备两个镜像，再使用同一个 Docker deployment：

```bash
docker --host unix:///var/run/docker.sock pull python:3.12-slim
docker --host unix:///var/run/docker.sock pull python:3.13-slim
./target/release/aporto build examples/agentfile -f Agentfile.multiple-images \
  -o examples/agentfile/dist/reviewer-docker.agent.json
./target/release/aporto activate --config examples/agentfile/deployment-docker.toml --agent reviewer
./target/release/aporto run --config examples/agentfile/deployment-docker.toml --agent reviewer \
  --image python:3.13-slim --workdir /work/review --task '检查运行环境'
```

`image` 是默认项，`images` 是额外允许项；有效候选项最多 16 个。activation 会分别启动和验证两个环境的 MCP，逐项清理后才发布。同一 release 为每个镜像保留独立的固定身份和工具目录。`workdir` 是容器内目录，默认 `/workspace`，不会挂载宿主路径。

`[runtime]` 固定 provider 与 image/template。deployment 不重复这些字段；bundle 路径相对 deployment 所在目录。为 AgentENV 构建 bundle 时，将整个 `[runtime]` 改为：

```toml
[runtime]
provider = "agentenv"
template = "aporto-python"
# templates = ["aporto-python-extra"]
# workdir = "/work/review"
```

然后构建为 `examples/agentfile/dist/reviewer-agentenv.agent.json`，使用 local 或 remote deployment 激活。模板需在所选 AgentENV 服务中存在，设置 `AENV_API_KEY`；remote 示例 URL 必须替换成实际服务。不能仅改 deployment 就把 Docker bundle 当作 AgentENV bundle。

## 显式资源和 MCP

所有 `source` 相对本目录。Prompt、Skill、MCP 目录与 assets 只从显式路径打包，不扫描 workspace/HOME 配置。Prompt 默认 developer；Skill 目录保留相对结构，其正文通过 `tools.read_skill` 按需读取。

policy MCP 的 `command` 为 executable 和 argv，cwd 固定为包内目录 `/opt/agent/mcp/policy`。runtime 提供 Python 3，不自动安装依赖。未填写 `include_tools` 时，activation 发现并固定此 MCP 的全部工具；需要收窄时填写非空精确名称列表。

要接入 HTTP MCP，可在 Agentfile 添加下面的声明，并替换占位 URL：

```toml
[[mcp]]
name = "docs"
url = "https://mcp.example.com/mcp"
include_tools = ["search", "fetch"]
headers = { Authorization = { secret = "DOCS_AUTH" } }
```

同时在相应 deployment 添加：

```toml
[agents.secrets]
DOCS_AUTH = { env = "MCP_DOCS_AUTH" }
```

环境变量内容是完整 Authorization 值（例如 `Bearer ...`）。HTTP MCP 由 Core 发起连接，不受 Docker guest 的 `network = "none"` 限制。stdio MCP 的 `env` 可包含普通字符串和结构化 secret 引用，HTTP headers 只允许 secret 引用。没有通用字符串插值，也不会继承宿主的完整环境。

`[limits.turn]` 控制一轮的模型请求数、工具调用数和并发；其中 `timeout_ms` 仅限制 runtime / MCP 初始化后的模型与 PTC 执行阶段。初始化、导出和清理使用各自的超时与取消规则，不计入这个时限。`[limits.ptc]` 控制单个 JavaScript cell 的时限、内存与输出，其内存限额与 Docker 容器限额相互独立。

完整语义和 Schema 见 [Agentfile 规范](../../docs/agentfile.md)、[Agentfile Schema](../../docs/agentfile.schema.json)、[Deployment Schema](../../docs/deployment.schema.json)。Schema 只验证结构；构建、激活、真实模型和 runtime 的检查入口见 [验证指南](../../docs/validation.md)。
