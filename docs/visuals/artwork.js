// Original, deterministic HTML/CSS artwork. No application screenshots or live data.
import { drawConnections } from './connections.js';
const params = new URLSearchParams(location.search);
const lang = params.get('lang') === 'zh' ? 'zh' : 'en';
const view = params.get('view') || 'hero';
document.documentElement.lang = lang === 'zh' ? 'zh-CN' : 'en';

const paths = {
  file: '<path d="M7 3h8l4 4v14H7z"/><path d="M15 3v5h4M10 12h6m-6 4h6"/>',
  code: '<path d="m8 7-5 5 5 5m8-10 5 5-5 5m-3-13-2 16"/>',
  box: '<path d="m3 7 9-4 9 4v10l-9 4-9-4zM3 7l9 4 9-4M12 11v10M7 5l10 4"/>',
  layers: '<path d="m3 7 9-4 9 4-9 4zm0 5 9 4 9-4M3 17l9 4 9-4"/>',
  terminal: '<rect x="3" y="4" width="18" height="16" rx="3"/><path d="m7 8 4 4-4 4m7 0h3"/>',
  window:
    '<rect x="3" y="4" width="18" height="16" rx="3"/><path d="M3 9h18M7 6.5h.01M10 6.5h.01"/>',
  server:
    '<rect x="4" y="3" width="16" height="7" rx="2"/><rect x="4" y="14" width="16" height="7" rx="2"/><path d="M8 6.5h.01M8 17.5h.01M12 6.5h5M12 17.5h5"/>',
  database:
    '<ellipse cx="12" cy="5" rx="8" ry="3"/><path d="M4 5v14c0 4 16 4 16 0V5M4 12c0 4 16 4 16 0"/>',
  chip: '<rect x="6" y="6" width="12" height="12" rx="3"/><path d="M9 2v4m6-4v4M9 18v4m6-4v4M2 9h4m-4 6h4m12-6h4m-4 6h4M10 10h4v4h-4z"/>',
  sliders:
    '<path d="M4 7h8m4 0h4M4 17h3m4 0h9"/><circle cx="14" cy="7" r="2"/><circle cx="9" cy="17" r="2"/>',
  check:
    '<path d="m5 12 4 4L19 6"/><path d="M20 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h9"/>',
  search: '<circle cx="10" cy="10" r="6"/><path d="m15 15 6 6"/>',
  edit: '<path d="m15 4 5 5M4 20l5-1L21 7a2 2 0 0 0-5-5L4 14z"/>',
  spark: '<path d="m12 2 2.5 7.5L22 12l-7.5 2.5L12 22l-2.5-7.5L2 12l7.5-2.5z"/>',
};
const icon = (name, extra = '') =>
  `<svg class="icon ${extra}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths[name]}</svg>`;
// Keep the mark's geometry and stops identical to the Client UI's BrandMark.
const logo = `<svg viewBox="0 0 40 40" fill="none" stroke-width="2.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><defs><linearGradient id="brand" x1="0" y1="0" x2="1" y2="1"><stop offset="0%" stop-color="#779cff"/><stop offset="55%" stop-color="#b18cde"/><stop offset="100%" stop-color="#df839e"/></linearGradient></defs><path d="M10 29 20 10 30 29M15 22h10" stroke="url(#brand)"/></svg>`;
const t = (en, zh) => (lang === 'zh' ? zh : en);
const masthead = (label) =>
  `<div class="masthead"><div class="brand">${logo}<span class="brand-wordmark">Aporto</span></div><span class="edition">${label}</span></div>`;
const heading = (title, subtitle) =>
  `<header class="section-heading"><h1>${title}</h1><p>${subtitle}</p></header>`;
const art = (name, content) =>
  `<section class="artboard ${name}" data-artboard aria-label="${name}">${content}</section>`;
const arrowLayer = (content, height) =>
  `<svg class="arrows" viewBox="0 0 1200 ${height}" aria-hidden="true"><defs><marker id="arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="m1 1 7 4-7 4z"/></marker><marker id="secondary-arrow" class="secondary" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="m1 1 7 4-7 4z"/></marker></defs>${content}</svg>`;
const connections = {
  build: [
    { from: '.source-card', to: '.build-card', align: true },
    { from: '.build-card', to: '.activation-card', align: true },
    { from: '.activation-card', to: '.release-card', align: true },
    {
      from: '.deployment-card',
      to: '.activation-card',
      fromSide: 'top',
      toSide: 'bottom',
      align: true,
      secondary: true,
    },
  ],
  architecture: [
    { from: '.clients', to: '.server', align: true },
    { from: '.server', to: '.core', align: true },
    { from: '.core', to: '.database', align: true },
    { from: '.core', to: '.engine', fromSide: 'bottom', toSide: 'top', align: true },
    { from: '.cli', to: '.engine', align: true },
    { from: '.engine', to: '.runtime', align: true },
    {
      from: '.engine',
      to: '.model-node',
      fromSide: 'left',
      toSide: 'right',
      fromOffset: 0.22,
      secondary: true,
    },
    {
      from: '.engine',
      to: '.http-mcp',
      fromSide: 'left',
      toSide: 'right',
      fromOffset: 0.8,
      secondary: true,
    },
  ],
};

const views = {
  hero: () =>
    art(
      'hero',
      `
    ${masthead(t('Agent framework', 'Agent 构建与运行框架'))}
    <div class="hero-content">
      <div class="hero-copy">
        <h1>${t('Define, package,<br>and run your agents.', '定义、打包、<br>运行你的 Agent。')}</h1>
        <p>${t('Compose prompts, skills, and tools.<br>Run tasks in Docker or AgentENV.<br>Manage conversations and workspaces.', '组合 Prompt、Skill 和工具，<br>在 Docker 或 AgentENV 中执行任务，<br>管理会话与工作区。')}</p>
      </div>
      <div class="hero-flow">
        <div class="hero-source card">
          <div class="hero-source-title">${icon('file')}<h2>Agentfile</h2><span>TOML</span></div>
          <p>Model · Runtime<br>Prompt · Skill · MCP · Assets</p>
        </div>
        <div class="flow-connector" aria-hidden="true"><span>${t('Build', '构建')}</span></div>
        <div class="hero-artifact card">${icon('box')}<div><h2>Bundle</h2><p>${t('Declared local resources', '显式本地资源')}</p></div></div>
        <div class="flow-connector" aria-hidden="true"><span>${t('Activate & run', '激活并运行')}</span></div>
        <div class="hero-artifact card">${icon('terminal')}<div><h2>${t('Task execution', '任务执行')}</h2><p>Docker · AgentENV</p></div></div>
      </div>
    </div>
  `,
    ),

  build: () =>
    art(
      'build',
      `
    ${masthead(t('Build & release', '构建与发布'))}
    ${heading(t('From source to an immutable release.', '从本地资源，到不可变发布。'), t('Package declared resources. Validate the environment. Pin the execution contract.', '打包声明的资源，验证运行环境，固定执行契约。'))}
    ${arrowLayer('', 702)}
    <div class="build-stages">
      <div><div class="stage-label"><span class="stage-number">1</span>${t('Define', '定义')}</div><div class="source-card card"><h2>Agentfile</h2>
        <div class="resource-line configuration">${icon('sliders')}${t('Model / Runtime', '模型 / Runtime')}</div>
        <div class="resource-line">${icon('file')}Prompt</div><div class="resource-line">${icon('layers')}Skill</div><div class="resource-line">${icon('code')}MCP</div>
      </div></div>
      <div><div class="stage-label"><span class="stage-number">2</span>${t('Build offline', '离线构建')}</div><div class="build-card card"><div class="icon-box">${icon('box')}</div><h2 class="mono">*.agent.json</h2><p>${t('Packed files<br>File digests', '资源快照<br>文件摘要')}</p></div></div>
      <div><div class="stage-label"><span class="stage-number">3</span>${t('Activate', '激活')}</div><div class="build-card activation-card card"><div class="icon-box">${icon('check')}</div><h2>${t('Validate', '验证并绑定')}</h2><p>${t('Start runtime<br>Discover tools', '启动 runtime<br>发现 MCP 工具')}</p></div></div>
      <div><div class="stage-label"><span class="stage-number">4</span>${t('Release', '发布')}</div><div class="release-card card"><div class="icon-box success">${icon('layers')}</div><h2>Release</h2><p>${t('Bundle<br>Bindings<br>Tool catalog', '资源包<br>配置绑定<br>工具目录')}</p></div></div>
    </div>
    <div class="deployment-card card"><div class="icon-box">${icon('sliders')}</div><div><h3>Deployment</h3><p>${t('Endpoints · Env refs', '连接 · 环境变量引用')}</p></div></div>
    <p class="footnote">${t('Model services, images/templates, and HTTP MCP remain external dependencies.', '模型服务、镜像或模板及 HTTP MCP 仍是外部依赖。')}</p>
  `,
    ),

  architecture: () =>
    art(
      'architecture',
      `
    ${masthead(t('Modular by design', '模块化架构'))}
    ${heading(t('Execution engine. Persistent conversations.', '执行引擎与持久会话。'), t('Core manages threads. The engine runs the model and tool loop.', 'Core 管理会话，引擎执行模型与工具循环。'))}
    ${arrowLayer('', 848)}
    <div class="node clients card">${icon('window', 'node-icon')}<h2>${t('Clients', '客户端')}</h2><p>${t('Client UI · Scripts', 'Client UI · 脚本')}</p></div>
    <div class="node server card">${icon('server', 'node-icon')}<h2>HTTP Server</h2><p>HTTP API · SSE</p></div>
    <div class="node core card">${icon('layers', 'node-icon')}<h2>Core</h2><p>${t('Threads · Scheduling', '会话 · 调度 · 取消')}</p></div>
    <div class="node database card">${icon('database', 'node-icon')}<h2>SQLite</h2><p>${t('History · Events', '历史 · 过程事件')}</p></div>
    <div class="node cli card">${icon('terminal', 'node-icon')}<h2>CLI</h2><p>${t('One task<br>Temporary run', '单次任务<br>临时 runtime')}</p></div>
    <div class="model-node dependency card">${icon('spark')}<h2>Responses API</h2></div>
    <div class="http-mcp dependency card">${icon('code')}<div><h2>HTTP MCP</h2><p>${t('Optional', '可选')}</p></div></div>
    <span class="direct-label">${t('Direct execution', '直接执行')}</span>
    <div class="node engine card">${icon('chip', 'node-icon')}<h2>Agent engine</h2><p>${t('Model loop<br>PTC · exec / wait<br>Tool broker', '模型循环<br>PTC · exec / wait<br>工具调度')}</p></div>
    <div class="node runtime card">${icon('box', 'node-icon')}<h2>Docker /<br>AgentENV</h2><p>${t('Commands · Files<br>Packaged stdio MCP', '命令 · 文件<br>打包的 stdio MCP')}</p></div>
    <p class="footnote">${t('Client UI calls the Server HTTP API. CLI runs the engine directly.', 'Client UI 通过 Server 的 HTTP API 调用；CLI 直接运行引擎。')}</p>
  `,
    ),

  swe: () =>
    art(
      'swe',
      `
    ${masthead(t('SWE-Agent · Example workflow', 'SWE-Agent · 示例流程'))}
    ${heading(t('Reproduce. Fix. Verify. Deliver.', '复现问题，完成修复，交付补丁。'), t('A small Python defect shows the full software engineering loop.', '用一个 Python 缺陷，展示完整的软件工程任务流程。'))}
    <div class="swe-stages">
      <div class="swe-stage card"><div class="icon-box error">${icon('search')}</div><h2>${t('Reproduce', '复现')}</h2><p class="mono">mean([1, 2])<br><span class="failure">1</span> ≠ <span class="success">1.5</span></p></div>
      <div class="swe-stage card"><div class="icon-box">${icon('edit')}</div><h2>${t('Fix', '修复')}</h2><p class="operator mono">// → /</p></div>
      <div class="swe-stage card"><div class="icon-box success">${icon('check')}</div><h2>${t('Verify', '验证')}</h2><p>${t('Run unittest<br>Check regressions', '运行 unittest<br>检查回归行为')}</p></div>
      <div class="swe-stage card"><div class="icon-box">${icon('file')}</div><h2>${t('Deliver', '交付')}</h2><p class="mono">fix.patch</p></div>
    </div>
    <div class="diff-card"><div class="diff-file">${icon('file')}<span class="mono">calculator.py</span></div><code class="diff-line removed">− return sum(values) // len(values)</code><code class="diff-line added">+ return sum(values) / len(values)</code></div>
    <div class="delivery"><h2>${t('A patch you can inspect.', '可导出、可检查的补丁。')}</h2><p>${t('Generated with difflib.<br>Exported explicitly by CLI.', '使用 difflib 生成，<br>通过 CLI 显式导出。')}</p></div>
    <p class="footnote">${t('Illustrated example · The agent edits a runtime copy of the project.', '流程示意 · Agent 在 runtime 中的项目副本上修改代码。')}</p>
  `,
    ),
};

if (!Object.hasOwn(views, view)) throw new Error(`Unknown artwork: ${view}`);
document.querySelector('#artwork').innerHTML = views[view]();
document.querySelectorAll('.card, .diff-card').forEach((node) => {
  node.dataset.layoutBounds = 'true';
});
document.title = `Aporto · ${view} · ${lang}`;
await document.fonts.ready;
if (connections[view]) {
  drawConnections(document.querySelector('[data-artboard]'), connections[view]);
}
document.documentElement.dataset.ready = 'true';
