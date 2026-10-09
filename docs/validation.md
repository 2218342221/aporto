# 验证指南

以下命令从仓库根目录执行，前端命令另行注明。使用仓库声明的 Rust 工具链、Node.js 24、Python 3 和锁定依赖。GitHub Actions 配置见 [CI](../.github/workflows/ci.yml)。

## 检查层次

| 检查 | 使用的真实组件 | 外部依赖 |
| --- | --- | --- |
| Rust 单元与集成测试 | 构建器、QuickJS、协议、SQLite、HTTP / MCP 解析器 | 模型与 AgentENV 采用 fixture；外部依赖测试默认 ignored |
| 前端单元与浏览器测试 | React、Chromium、HTTP / SSE 客户端 | 独立 fixture API，无真实模型或任务容器 |
| `e2e_stack.py` 默认模式 | Server / Core 独立进程、PTC、broker | Responses 与 AgentENV HTTP / Connect fixture |
| `e2e_instances.py --runtime agentenv` | 会话持久化、实例调度、镜像与工作目录选择 | AgentENV 与模型 fixture |
| Docker 集成和进程 E2E | 本机 daemon、真实容器、文件、命令、stdio MCP | 需预先加载镜像；默认模型仍为 fixture |
| `live_model` / `e2e_stack.py --live` | 显式指定的 Responses 模型服务 | 需模型凭据，产生模型调用费用；runtime 取决于所选模式 |
| 真实 AgentENV 验收 | 实际 AgentENV API、模板、envd 与 microVM | 需独立部署；以下 fixture 命令不覆盖此层 |

## Rust 与前端

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-features --locked --all-targets -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
```

普通产品交付使用 `cargo build --release --workspace --locked`。`--all-features` 用于测试时开启 Core client / Server 所需的测试 fixture；不要用它构建产品二进制。

在 `apps/web` 中执行：

```bash
npm ci
npm run format:check
npm run build
npm test
npx playwright install --with-deps chromium
npm run test:e2e
```

浏览器测试覆盖登录、会话与实例选择、发送/中断、过程展示、分页、幂等重试、SSE 重连、Markdown 和移动布局。构建和浏览器测试应顺序执行，避免测试读取到正在替换的 `dist`。

## 示例构建与进程协议

离线 build 不需要模型 key 或运行中的 runtime：

```bash
./target/debug/aporto build examples/swe-agent -o /tmp/aporto-swe-agent.agent.json
./target/debug/aporto inspect /tmp/aporto-swe-agent.agent.json
./target/debug/aporto build examples/reviewer -o /tmp/aporto-reviewer.agent.json
./target/debug/aporto inspect /tmp/aporto-reviewer.agent.json
./target/debug/aporto build examples/docker-reviewer -o /tmp/aporto-docker-reviewer.agent.json
./target/debug/aporto build examples/agentfile -o /tmp/aporto-full.agent.json
./target/debug/aporto build examples/agentfile -f Agentfile.multiple-images \
  -o /tmp/aporto-images.agent.json

python3 scripts/e2e_stack.py
python3 scripts/e2e_stack.py --mode remote
python3 scripts/e2e_instances.py --runtime agentenv --mode local
python3 scripts/e2e_instances.py --runtime agentenv --mode remote
```

进程脚本默认读取 `target/debug`，可用 `--bin-dir target/release` 指定产品构建。它们创建独立临时配置和 state 目录，验证两轮执行、文件保留、服务重启、幂等冲突、事件重放及已知测试凭据不落入数据库。实例脚本另覆盖共享文件与独立对话历史、共享实例串行执行和独立实例并行。

## 真实 Docker

这些命令会创建测试容器，并按测试记录的 ID 清理自己的资源。只在允许使用所选 Docker daemon 的环境执行：

```bash
docker --host unix:///var/run/docker.sock pull python:3.12-slim
docker --host unix:///var/run/docker.sock pull python:3.11-slim
APORTO_DOCKER_TESTS=1 cargo test --locked --test docker_runtime -- --ignored --nocapture
python3 scripts/e2e_stack.py --runtime docker
python3 scripts/e2e_instances.py --runtime docker
```

Rust Docker 测试覆盖二进制文件、工作目录、环境变量、stdio、取消、deadline、owner 校验、资源限制与 stop / start。进程 E2E 从容器独立读取文件核对结果，并在 Server / Core 重启后检查同一个容器的文件；实例 E2E 使用两种镜像。可通过脚本的 `--docker-host`、`--docker-image` 或 `--default-image` / `--alternate-image` 选择已准备的环境，具体以 `--help` 为准。

## 真实模型

测试只读取显式环境变量，不自动加载 `.env`、`~/.codex` 或其他用户配置。先通过终端或服务管理器设置以下变量，避免把 key 写入仓库或命令历史：

| 变量 | 内容 |
| --- | --- |
| `APORTO_LIVE_ENDPOINT` | 完整 Responses 请求 URL |
| `APORTO_LIVE_KEY` | 模型 API key |
| `APORTO_LIVE_MODEL` | 服务实际可用且支持 Responses custom tools 的模型 ID |
| `APORTO_LIVE_HEADERS` | 可选 JSON header 对象，值为字符串 |

```bash
cargo test --locked --test live_model -- --ignored --nocapture
python3 scripts/e2e_stack.py --runtime docker --live
```

`live_model` 使用真实模型、QuickJS 和测试 ToolBroker，通过不可猜测的随机 receipt 验证工具执行；它不是 runtime 验收。第二条命令结合真实模型和 Docker，验证打包 skill / MCP、命令、文件和跨轮恢复。若省略 `--runtime docker`，即使传入 `--live`，AgentENV 仍是协议 fixture。

## 目标部署验收

发布前按 [发布检查](review.md) 记录被测 commit、命令与结果，不把旧版本测试计数当作当前通过的证据。真实 AgentENV local / remote 需在目标服务分别完成：

1. 显式 build / activate，确认模板和 packaged MCP 可用。
2. 使用真实模型执行命令、读写文件、读取 skill 和调用 MCP，并独立读取产物核对。
3. 下一轮重读原文件，重启 Server / Core 后继续原 sandbox，确认未创建替代工作区。
4. 验证共享实例串行执行、取消、pause / connect、lease 和失败清理。
5. 在 workspace / HOME 放入冲突配置，确认不会自动加载其中的 MCP、skill 或 AGENTS.md。

生产部署还需按实际流量验证压力、断网与崩溃、磁盘耗尽、备份恢复、TLS、凭据轮换和资源保留策略。仓库的自动化测试不宣称完成这些部署相关检查。
