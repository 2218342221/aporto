# 示例

| 目录 | 适用场景 |
| --- | --- |
| [swe-agent](swe-agent/README.md) | 软件工程任务：复现缺陷、实现修复、运行回归测试并导出补丁 |
| [docker-reviewer](docker-reviewer/README.md) | 最小 Docker Agent，打包 prompt、skill 和 Python stdio MCP |
| [reviewer](reviewer/README.md) | 相同审查能力，运行在已有的 AgentENV local / remote 服务 |
| [agentfile](agentfile/README.md) | 完整 TOML 配置、assets、多个镜像、模型参数和资源限额 |
| [server](server/README.md) | 将 Docker 或 AgentENV bundle 绑定到 Responses 连接，启动持久会话服务 |

示例模型名 `YOUR_MODEL_ID` 是占位符，需替换成所用服务实际可用、支持 Responses custom tools 的模型 ID。连接使用完整 Responses URL；API key 和 MCP secret 通过环境变量绑定，不写入 Agentfile 或 bundle。

所有构建命令从仓库根目录执行。离线 build 不需要模型或 runtime；activate 会启动临时运行环境并检查 MCP，run 才会调用模型。完整流程见 [Agentfile 规范](../docs/agentfile.md)。
