# Server 部署配置

这些 TOML 文件绑定模型连接、bundle 和 runtime 资源。模型名与行为来自 Agentfile，API 请求不能覆盖 endpoint 或选择未发布的镜像。

| 配置 | Agent ID | 先构建的 bundle | 前置条件 |
| --- | --- | --- | --- |
| [deployment-docker.toml](deployment-docker.toml) | `docker-reviewer` | `docker-reviewer.agent.json` | 本机 Docker daemon、`python:3.12-slim` |
| [deployment-agentenv-local.toml](deployment-agentenv-local.toml) | `reviewer` | `reviewer.agent.json` | loopback AgentENV API、可用模板 |
| [deployment-agentenv-remote.toml](deployment-agentenv-remote.toml) | `reviewer` | `reviewer.agent.json` | 显式 AgentENV API / sandbox 地址、可用模板 |

相对 bundle 路径以 deployment 文件所在目录为基准。先按 [Docker 示例](../docker-reviewer/README.md) 或 [AgentENV 示例](../reviewer/README.md) 构建并 activate，然后设置至少 32 个可见 ASCII 字符的随机 `APORTO_SERVER_TOKEN`。

Docker 服务启动示例（从仓库根目录执行）：

```bash
./target/release/aporto-server \
  --core-bin ./target/release/aporto-core \
  --core-config examples/server/deployment-docker.toml \
  --state-dir .aporto/server-state \
  --listen 127.0.0.1:8080 \
  --allow-origin http://localhost:5173
```

AgentENV 部署只需将 `--core-config` 指向已经 activate 的对应配置。默认 release 目录为 deployment 旁的 `releases/`；如果 activate 使用了 `--releases-dir`，服务必须使用相同目录。更新配置后需重新 activate，并受控重启 Server / Core 才能让新会话采用新 release。

Core 保存会话与 runtime 实例，turn 结束会 stop / pause 并保留文件。一个 token 拥有该服务的全部会话权限；公网部署需配置 TLS 和访问控制。Client UI 启动见项目 [README](../../README.md)，完整 API、代理和 systemd 示例见 [HTTP Server](../../docs/http-server.md)。
