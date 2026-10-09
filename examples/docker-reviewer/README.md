# Docker reviewer 示例

此示例将 prompt、code-review skill 和无第三方 Python 依赖的 policy MCP 打包，runtime 为 `python:3.12-slim`。镜像需提供 Python 3、POSIX `sh` 和可写目录。

先把 [Agentfile](Agentfile) 中的 `YOUR_MODEL_ID` 换成所用服务实际可用、支持 Responses custom tools 的模型 ID，配置 `OPENAI_API_KEY` 及 deployment 中的完整 Responses endpoint。从仓库根目录执行：

```bash
cargo build --release --workspace --locked
docker --host unix:///var/run/docker.sock pull python:3.12-slim
./target/release/aporto build examples/docker-reviewer -o examples/server/docker-reviewer.agent.json
./target/release/aporto activate --config examples/server/deployment-docker.toml --agent docker-reviewer
./target/release/aporto run --config examples/server/deployment-docker.toml --agent docker-reviewer \
  --workspace /absolute/path/to/project --task '审查代码，将结论写入 review.md' \
  --export review.md=./review.md
```

Docker image 提供系统依赖，Agent bundle 提供模型配置、指令、技能和 MCP 源码。`tools.*` 由框架与已发布 MCP 目录注册，无需 TOOLS 清单。需要新增系统依赖时另行构建 Dockerfile，再将镜像引用填入 `[runtime].image`；activation 固定实际 image ID。

默认容器无网络、无宿主挂载。单任务 CLI 在导出后删除容器；持久会话使用 [Server](../server/README.md)。完整配置及清理规则见 [Docker runtime](../../docs/docker-runtime.md)，AgentENV 用法见 [reviewer](../reviewer/README.md)。
