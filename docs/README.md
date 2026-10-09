# 文档

首次使用从项目 [README](../README.md) 或 [中文说明](../README.zh-CN.md) 开始。

| 文档 | 内容 |
| --- | --- |
| [Agentfile](agentfile.md) | TOML 字段、模型配置、资源打包、MCP / skill、激活与 release |
| [架构](architecture.md) | Core / Server / Client 边界、持久化、调度、恢复与 runtime 生命周期 |
| [执行引擎](design.md) | PTC、Responses、tool broker、秘密和运行环境 |
| [Docker runtime](docker-runtime.md) | 镜像要求、部署、工作区、资源限制和清理 |
| [HTTP Server](http-server.md) | 启动、REST / SSE、鉴权、错误和反向代理 |
| [对话过程](conversation.md) | item 协议、流式文本、过程展示和客户端恢复 |
| [验证指南](validation.md) | 本地、浏览器、Docker、真实模型和 AgentENV 验证入口 |
| [发布检查](review.md) | 发布核对项、当前边界和保留 TODO |
| [字体](fonts.md) | 字体来源、打包和授权 |

配置示例位于 [examples](../examples/README.md)。Schema 提供 [Agentfile](agentfile.schema.json)、[Deployment](deployment.schema.json) 和 [Core protocol](protocol.schema.json)；TOML 的跨字段和路径约束以 Rust 校验器为准。
