# SWE-Agent 示例

这是 Aporto 的软件工程 Agent 示例：复现缺陷、修改代码、运行回归检查并交付补丁。它使用 Python 标准库，不依赖 Git、第三方包或网络，Agent 名称为 `swe-agent`。

| 路径 | 用途 |
| --- | --- |
| [Agentfile](Agentfile) | 模型、Docker 镜像及显式资源声明 |
| [deployment.toml](deployment.toml) | Responses 连接、Agent ID、bundle 路径及 Docker 资源 |
| [prompts/swe-agent.md](prompts/swe-agent.md) | 实现、验证、交付的角色约束 |
| [skills/software-engineering/SKILL.md](skills/software-engineering/SKILL.md) | 复现、编辑、回归验证和生成补丁的操作流程 |
| [mcp/project/](mcp/project/) | 无第三方依赖的 stdio MCP，提供 `engineering_guidelines` |
| [workspace/](workspace/) | 带真实缺陷和 unittest 的演示项目，作为任务文件单独上传 |

`workspace` 不属于 Agentfile 的打包资源。构建产物只包含 prompt、skill 和 MCP 的代码与规则；切换任务项目无需重新构建 Agent。

## 构建并激活

从仓库根目录执行，先复制示例到本地配置目录：

```bash
cargo build --release --workspace --locked
docker --host unix:///var/run/docker.sock pull python:3.12-slim
mkdir -p .aporto
cp -R examples/swe-agent .aporto/swe-agent
```

将 `.aporto/swe-agent/Agentfile` 中的 `YOUR_MODEL_ID` 换成服务实际可用、支持 Responses custom tools 的模型 ID。修改 `.aporto/swe-agent/deployment.toml` 的完整 Responses endpoint，并在执行 CLI 或 Server 的环境设置 `OPENAI_API_KEY`。不要把凭据值写入这些文件。

```bash
./target/release/aporto build .aporto/swe-agent \
  -o .aporto/swe-agent/swe-agent.agent.json
./target/release/aporto inspect .aporto/swe-agent/swe-agent.agent.json
./target/release/aporto activate \
  --config .aporto/swe-agent/deployment.toml --agent swe-agent
```

模型连接名为 `primary`，调用使用 deployment 中的 Agent ID `swe-agent`。bundle 路径相对于 deployment 所在目录；activate 的 release 默认保存在同目录的 `releases/` 中。

## 修复演示项目

项目的 `calculator.mean([1, 2])` 应返回 `1.5`，当前实现的整除运算返回 `1`。在仓库根目录运行以下命令，可在不调用模型的情况下复现测试失败：

```bash
(cd examples/swe-agent/workspace && python3 -m unittest discover -s tests -v)
```

正数和负数的非整数平均值测试会失败；空输入、单值、不修改输入和整数平均值检查应通过。这是故意保留的任务输入，请让 Agent 在上传后的容器副本中修复。

```bash
./target/release/aporto run \
  --config .aporto/swe-agent/deployment.toml --agent swe-agent \
  --workspace examples/swe-agent/workspace \
  --task 'Fix mean([1, 2]) returning 1 instead of 1.5. Reproduce the failing test, fix the code, run python3 -m unittest discover -s tests -v, and write fix.patch as a unified diff against the original files.' \
  --export fix.patch=./fix.patch
```

CLI 将项目快照上传到 `/workspace`，不修改宿主上的示例源码。Agent 会在修改前保存原始文件，用 Python `difflib` 生成 unified diff；默认镜像不包含 Git，上传快照也不会包含 `.git`。执行结束后最终回答输出到 stdout，`fix.patch` 导出到宿主当前目录，临时容器正常删除。

检查补丁内容与实际测试结果后再应用到目标项目。不要把模型文字中的“通过”当作执行证据；对话过程或 CLI 结果应说明运行的测试命令及结果。

## API 和 UI

完成激活后，用同一 deployment 启动 Server，API 的 `agent_id` 和 UI 选项均使用 `swe-agent`。完整步骤见项目 [README](../../README.md)。

API / UI 不上传本地 `workspace`。新实例从所选镜像开始；可以提交自包含任务，要求先创建 `calculator.py` 和 unittest，复现整除缺陷，再修改并将补丁保存到 `fix.patch`。此时补丁基线是刚复现的错误实现。后续轮次可继续读取文件，或者复用 Aporto 已创建的实例。Server 没有 CLI `--export` 对应的文件下载接口；可通过任务读取文件内容，或按 [Docker 文档](../../docs/docker-runtime.md) 从准确容器 ID 导出。
