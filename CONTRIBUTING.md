# Contributing to Aporto

Start with the [README](README.md), [architecture](docs/architecture.md), and [Agentfile specification](docs/agentfile.md). Keep changes focused on one behavior and include the commands used to verify it. Issues and pull requests may be written in English or Chinese.

## Local setup

Use Linux, the Rust toolchain in `rust-toolchain.toml`, a C compiler and linker, Python 3, and Node.js 24. Rust's QuickJS and bundled SQLite dependencies compile native code. Docker tests also require a local daemon accessible through a Unix socket and the images listed below. AgentENV protocol tests use local fixtures and do not need KVM.

```bash
cargo build --workspace --locked
npm --prefix apps/web ci
```

Keep local deployment files, credentials, releases, state databases, and validation output under the ignored `.aporto/` directory. Examples contain placeholders and environment-variable references. Never add a real credential, local data, or a provider-specific private endpoint to an example or screenshot.

## Code boundaries

- Put execution and runtime behavior in the root library; Core owns persistence, scheduling, and lifecycle.
- Keep protocol data types in `crates/protocol`. Server translates HTTP/SSE to Core RPC; it must not invoke the execution engine directly.
- Keep browser transport and reconciliation in `apps/web/src/api`; components handle presentation and interaction.
- Preserve explicit configuration. Do not discover prompts, skills, MCP, or `AGENTS.md` from a task workspace or home directory.
- Do not automatically retry side effects. Preserve idempotency keys and make cancellation, truncation, and partial failure visible.

Use existing modules and patterns. Add regression tests for behavior changes and bugs; documentation and purely visual cleanup usually need existing checks rather than new implementation-mirroring tests.

## Checks

Run Rust formatting, linting, and tests:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-features --locked --all-targets -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
```

`--all-features` enables test fixture binaries. Build distributable binaries without that flag.

Run process integration with deterministic Responses and AgentENV fixtures:

```bash
python3 scripts/e2e_stack.py
python3 scripts/e2e_stack.py --mode remote
python3 scripts/e2e_instances.py --runtime agentenv --mode local
python3 scripts/e2e_instances.py --runtime agentenv --mode remote
```

For real Docker integration:

```bash
docker --host unix:///var/run/docker.sock pull python:3.12-slim
docker --host unix:///var/run/docker.sock pull python:3.11-slim
APORTO_DOCKER_TESTS=1 cargo test --locked --test docker_runtime -- --ignored --nocapture
python3 scripts/e2e_stack.py --runtime docker
python3 scripts/e2e_instances.py --runtime docker
```

Tests create and clean up their own containers. They must never prune the daemon or modify existing service state. Real-model tests are opt-in and use explicit `APORTO_LIVE_*` environment variables; see [validation](docs/validation.md). Do not require model credentials in pull-request CI.

For the client:

```bash
cd apps/web
npm run format:check
npm run typecheck
npm test
npx playwright install --with-deps chromium
npm run test:e2e
npm run build
```

Playwright starts its own UI and API fixture on ports 4174 and 4318. See the [client guide](apps/web/README.md) for screenshots and deployment.

README illustrations are maintained as original HTML/CSS in `docs/visuals/`. After editing them, run `npm --prefix apps/web run docs:images` from the repository root and inspect both languages. Commit the source and generated PNGs together. See the [artwork guide](docs/visuals/README.md) for local previews and single-image exports. CI renders all eight images to check font loading and layout bounds.

## Protocol and configuration changes

When changing `crates/protocol`, regenerate and commit its schema:

```bash
cargo run --locked -p aporto-protocol --example export-schema > docs/protocol.schema.json
```

The Agentfile and deployment schemas are maintained alongside their Rust parsers. Update the relevant schema, parser validation, examples, documentation, and browser types together. Exercise invalid input as well as valid round trips. Configuration errors must not echo secret values.

The [CI workflow](.github/workflows/ci.yml) checks protocol schema drift, example builds, Rust, browser behavior, and real Docker integration. AgentENV HTTP fixtures prove protocol handling; they do not prove compatibility with every deployed AgentENV version or microVM environment.

## Preparing a GitHub release

1. Run CI on the exact commit being released, including a clean source build. Record separately which model and runtime checks used fixtures or real services.
2. Check the public diff and Git history for credentials, private endpoints, local paths, task data, and generated runtime state. `.gitignore` does not remove previously committed data.
3. Verify the English and Chinese README, examples, documentation links, illustrations, and bundled license files.
4. Document behavior changes and remaining limitations in the GitHub release notes. Do not describe untested runtime deployments as verified.
5. Build Rust with `cargo build --release --workspace --locked` and the client with `npm ci` / `npm run build`. Document any public `VITE_APORTO_API_URL` baked into the client. Ship the entire client `dist/`, including font licenses.
6. Choose the repository owner, visibility, tag, and release artifacts explicitly. Enable private vulnerability reporting and branch checks in the destination repository.

The source workspace is not yet configured for publishing its crates to crates.io. Publishing source to GitHub does not require doing so.

## License

Contributions are provided under the project's [Apache-2.0 license](LICENSE). Retain the license and attribution of third-party assets. Report security issues as described in [SECURITY.md](SECURITY.md).
