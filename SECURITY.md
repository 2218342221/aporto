# Security

Aporto currently targets trusted operators in a single-tenant environment. All holders of a server bearer token share access to that server's configured agents and conversations. There is no per-user authorization or tenant isolation.

## Execution and data boundaries

- Core runs QuickJS in-process. Cells have execution and memory limits, but these do not replace operating-system isolation for hostile code.
- Docker containers share the host kernel. Core needs access to the configured Docker daemon; never pass its socket into a task container. AgentENV's isolation depends on the deployed service and templates.
- Agentfile resources, images, MCP servers, and deployment configuration are trusted operator inputs. Workspace configuration is never auto-loaded, but tasks can still read files and send their contents to the model or explicit tools.
- Docker guest networking defaults to `none`. This does not restrict Core's model requests or HTTP MCP connections. Explicit MCP credentials are available to their configured MCP server.
- State databases contain conversation content, tool results, and model history. Redaction of configured credentials is best-effort protection, not a guarantee that arbitrary task data contains no secrets. Protect state, logs, backups, and exports accordingly.
- Interruption stops future work where possible; it does not undo completed filesystem writes or external MCP operations. Crashed tasks are not automatically retried.

Use a separate, random server token of at least 32 characters. Keep model and runtime credentials in explicit environment-variable bindings; do not place them in the client, bundle resources, URLs, or source control. Serve remote deployments over HTTPS, restrict allowed browser origins, and avoid logging Authorization headers.

The server does not automatically garbage-collect persistent runtime instances. Docker has no lease-based automatic pause; after a crash, check the recorded instance IDs and ownership before cleanup. Details are in the [architecture](docs/architecture.md), [HTTP deployment guide](docs/http-server.md), and [Docker runtime guide](docs/docker-runtime.md).

## Reporting a vulnerability

Use **Security → Report a vulnerability** in the GitHub repository when private reporting is enabled. Include the affected commit, a minimal reproduction, expected and observed behavior, and the runtime involved. Remove real credentials and private task data.

If private reporting is unavailable, open an issue asking the maintainers for a private reporting channel without disclosing exploit details or sensitive data. Do not use a public issue to share a working exploit against an active deployment.

Security fixes are developed against the current default branch. No older release line currently has a separate maintenance commitment.
