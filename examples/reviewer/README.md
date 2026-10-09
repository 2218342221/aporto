# AgentENV reviewer 示例

此示例把审查 prompt、code-review skill 和无第三方 Python 依赖的 policy MCP 打包到 Agent bundle。运行环境由现有 AgentENV 服务提供，Aporto 不负责安装该服务或创建模板。

## 准备

1. 按 [AgentENV](https://github.com/kvcache-ai/AgentENV) 的部署说明准备 local 或 remote 服务。
2. 准备带 Python 3、POSIX `sh` 和可写 `/workspace`、`/opt/agent` 的模板，将 [Agentfile](Agentfile) 中的 `template` 替换成实际模板 ID。[runtime.Dockerfile](runtime.Dockerfile) 仅示意 guest 系统依赖，不会自动注册 AgentENV 模板。
3. 将 `YOUR_MODEL_ID` 换成支持 Responses custom tools 的可用模型 ID，修改对应 deployment 的 Responses endpoint，并设置 `OPENAI_API_KEY` 和 `AENV_API_KEY`。

## 本地服务

从仓库根目录执行：

```bash
cargo build --release --workspace --locked
./target/release/aporto build examples/reviewer \
  -o examples/server/reviewer.agent.json
./target/release/aporto activate \
  --config examples/server/deployment-agentenv-local.toml --agent reviewer
./target/release/aporto run \
  --config examples/server/deployment-agentenv-local.toml --agent reviewer \
  --workspace /absolute/path/to/project --task '审查代码，将结论写入 review.md' \
  --export review.md=./review.md
```

local 模式只接受 loopback API 地址，默认示例为 `http://127.0.0.1:8000`。如 envd 数据面使用其他地址，可在 deployment 显式配置 `sandbox_url`。

远端服务使用 [deployment-agentenv-remote.toml](../server/deployment-agentenv-remote.toml)，将 `api_url` / `sandbox_url` 占位地址换成实际地址，并对该配置单独 activate；模板必须在目标服务存在。相同模板名不保证跨服务内容一致。

CLI 正常结束会删除 sandbox，指定的产物在删除前导出。需要持久会话时使用 [Server 示例](../server/README.md)。AgentENV 的 lease、pause / connect 与清理边界见 [架构](../../docs/architecture.md)。
