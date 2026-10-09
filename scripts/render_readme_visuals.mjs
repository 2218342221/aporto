#!/usr/bin/env node
/** Render the original README HTML artwork without an application server or network access. */
import { createServer } from 'node:http';
import { access, mkdir, readFile, realpath, stat, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { dirname, extname, isAbsolute, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const web = resolve(root, 'apps/web');
const views = ['hero', 'build', 'architecture', 'swe'];
const languages = ['en', 'zh'];
const exactFiles = [
  ['/client/theme.css', resolve(web, 'src/theme.css')],
  ['/client/fonts.css', resolve(web, 'src/fonts.css')],
];
const mounts = [
  ['/docs/visuals/', resolve(root, 'docs/visuals')],
  [
    '/client/@fontsource-variable/google-sans-flex/',
    resolve(web, 'node_modules/@fontsource-variable/google-sans-flex'),
  ],
  [
    '/client/@fontsource-variable/noto-sans-sc/',
    resolve(web, 'node_modules/@fontsource-variable/noto-sans-sc'),
  ],
];
const contentTypes = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.webp': 'image/webp',
  '.gif': 'image/gif',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.txt': 'text/plain; charset=utf-8',
};

function usage() {
  console.log(`Render Aporto README visuals from their original HTML source.

Usage: node scripts/render_readme_visuals.mjs [options]

  --view <name>     hero, build, architecture, or swe (default: all)
  --lang <code>     en or zh (default: both)
  --out-dir <path>  PNG destination (default: docs/images/readme in the repository)
  --serve          Preview the HTML until Ctrl+C, without generating images
  --port <number>   Local server port (default: an available random port)
  --help           Show this help

Rendering requires npm ci and npx playwright install chromium in apps/web.
No model, application API, credentials, or external assets are used.`);
}

function parseOptions() {
  const { values } = parseArgs({
    options: {
      view: { type: 'string' },
      lang: { type: 'string' },
      'out-dir': { type: 'string' },
      serve: { type: 'boolean', default: false },
      port: { type: 'string', default: '0' },
      help: { type: 'boolean', default: false },
    },
    allowPositionals: false,
  });
  if (values.help) return { help: true };
  if (values.view && !views.includes(values.view)) {
    throw new Error(`Unknown view: ${values.view}. Choose ${views.join(', ')}.`);
  }
  if (values.lang && !languages.includes(values.lang)) {
    throw new Error(`Unknown language: ${values.lang}. Choose ${languages.join(', ')}.`);
  }
  if (!/^\d+$/.test(values.port) || Number(values.port) > 65535) {
    throw new Error('--port must be an integer between 0 and 65535.');
  }
  return {
    views: values.view ? [values.view] : views,
    languages: values.lang ? [values.lang] : languages,
    output: values['out-dir'] ? resolve(values['out-dir']) : resolve(root, 'docs/images/readme'),
    serve: values.serve,
    port: Number(values.port),
  };
}

function within(directory, path) {
  const child = relative(directory, path);
  return child !== '..' && !child.startsWith(`..${sep}`) && !isAbsolute(child);
}

async function localServer(port) {
  const files = new Map(
    await Promise.all(exactFiles.map(async ([url, path]) => [url, await realpath(path)])),
  );
  const directories = await Promise.all(
    mounts.map(async ([prefix, directory]) => [prefix, await realpath(directory)]),
  );
  const server = createServer(async (request, response) => {
    const send = (code, body) => {
      response.writeHead(code, { 'Content-Type': 'text/plain; charset=utf-8' });
      response.end(body);
    };
    if (!['GET', 'HEAD'].includes(request.method)) {
      response.setHeader('Allow', 'GET, HEAD');
      send(405, 'Method not allowed');
      return;
    }
    try {
      const url = new URL(request.url, 'http://127.0.0.1');
      const pathname = decodeURIComponent(url.pathname);
      if (pathname === '/') {
        response.writeHead(302, { Location: `/docs/visuals/index.html${url.search}` }).end();
        return;
      }
      // Browsers may request an icon even when the artwork has no favicon.
      if (pathname === '/favicon.ico') {
        response.writeHead(204).end();
        return;
      }
      let resolved = files.get(pathname);
      if (!resolved) {
        const mount = directories.find(([prefix]) => pathname.startsWith(prefix));
        if (!mount) {
          send(404, 'Not found');
          return;
        }
        const [prefix, directory] = mount;
        const path = resolve(directory, pathname.slice(prefix.length));
        if (!within(directory, path)) {
          send(404, 'Not found');
          return;
        }
        resolved = await realpath(path);
        if (!within(directory, resolved)) {
          send(404, 'Not found');
          return;
        }
      }
      if (!(await stat(resolved)).isFile()) {
        send(404, 'Not found');
        return;
      }
      const data = await readFile(resolved);
      response.writeHead(200, {
        'Content-Type': contentTypes[extname(resolved)] ?? 'application/octet-stream',
        'Content-Length': data.byteLength,
        'Cache-Control': 'no-store',
        'X-Content-Type-Options': 'nosniff',
      });
      response.end(request.method === 'HEAD' ? undefined : data);
    } catch (error) {
      if (error instanceof URIError || error.code === 'ERR_INVALID_ARG_VALUE') {
        send(400, 'Invalid path');
      } else if (['ENOENT', 'ENOTDIR', 'EACCES'].includes(error.code)) {
        send(404, 'Not found');
      } else {
        console.error(`Artwork server: ${error.message}`);
        send(500, 'Unable to read asset');
      }
    }
  });
  await new Promise((accept, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => {
      server.off('error', reject);
      accept();
    });
  });
  return { server, origin: `http://127.0.0.1:${server.address().port}` };
}

const previewUrl = (origin, view, language) =>
  `${origin}/docs/visuals/index.html?view=${view}&lang=${language}`;

async function render(browser, origin, view, language, output) {
  const context = await browser.newContext({
    viewport: { width: 1280, height: 1000 },
    deviceScaleFactor: 2,
    locale: language === 'zh' ? 'zh-CN' : 'en-US',
    reducedMotion: 'reduce',
    colorScheme: 'dark',
    serviceWorkers: 'block',
  });
  const failures = new Set();
  try {
    const page = await context.newPage();
    page.setDefaultTimeout(30_000);
    page.on('pageerror', (error) => failures.add(`Page error: ${error.message}`));
    page.on('console', (message) => {
      if (['error', 'warning'].includes(message.type())) {
        failures.add(`Console ${message.type()}: ${message.text()}`);
      }
    });
    page.on('requestfailed', (request) => {
      failures.add(`Request failed: ${request.url()} (${request.failure()?.errorText})`);
    });
    page.on('response', (response) => {
      if (response.status() >= 400) failures.add(`HTTP ${response.status()}: ${response.url()}`);
    });
    await page.route('**/*', (route) => {
      const url = new URL(route.request().url());
      if (url.origin !== origin && !['data:', 'blob:'].includes(url.protocol)) {
        failures.add(`External resource blocked: ${url.href}`);
        return route.abort('blockedbyclient');
      }
      return route.continue();
    });
    await page.goto(previewUrl(origin, view, language), {
      waitUntil: 'domcontentloaded',
    });
    await page.waitForFunction(() => document.documentElement.dataset.ready === 'true');
    await page.evaluate(async () => {
      await document.fonts.ready;
      await Promise.all(
        Array.from(document.images, async (image) => {
          await image.decode();
          if (!image.naturalWidth || !image.naturalHeight) {
            throw new Error(`Image has no rendered content: ${image.currentSrc || image.src}`);
          }
        }),
      );
    });
    await page.waitForLoadState('networkidle');
    await page.evaluate(
      () => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))),
    );
    const artboard = page.locator('[data-artboard]');
    if ((await artboard.count()) !== 1) throw new Error('Expected exactly one [data-artboard].');
    const bounds = await artboard.boundingBox();
    if (!bounds || Math.abs(bounds.width - 1200) > 0.1 || bounds.height <= 0) {
      throw new Error(`Expected a visible 1200px-wide artboard, got ${JSON.stringify(bounds)}.`);
    }
    const overflow = await artboard.evaluate((board) => {
      const problems = [];
      const rect = board.getBoundingClientRect();
      if (board.scrollWidth > board.clientWidth + 1) problems.push('horizontal artboard overflow');
      if (board.scrollHeight > board.clientHeight + 1) problems.push('vertical artboard overflow');
      if (document.documentElement.scrollWidth > innerWidth + 1) {
        problems.push('horizontal page overflow');
      }
      const visible = (element) => {
        if (getComputedStyle(element).visibility !== 'visible') return false;
        for (let ancestor = element; ancestor; ancestor = ancestor.parentElement) {
          const style = getComputedStyle(ancestor);
          if (
            style.display === 'none' ||
            style.opacity === '0' ||
            style.contentVisibility === 'hidden'
          )
            return false;
        }
        return true;
      };
      const label = (element) => {
        const name = element.getAttribute('data-layout-bounds');
        if (name && name !== 'true') return name;
        return `${element.tagName.toLowerCase()}${element.id ? `#${element.id}` : ''}${
          element.classList.length ? `.${Array.from(element.classList).join('.')}` : ''
        }`;
      };
      const containers = Array.from(board.querySelectorAll('[data-layout-bounds]'));
      if (board.matches('[data-layout-bounds]')) containers.unshift(board);
      for (const container of containers) {
        if (!visible(container) || !container.getClientRects().length) continue;
        if (container.scrollWidth > container.clientWidth + 2) {
          problems.push(`Horizontal layout overflow (${label(container)})`);
        }
        if (container.scrollHeight > container.clientHeight + 2) {
          problems.push(`Vertical layout overflow (${label(container)})`);
        }
      }
      const walker = document.createTreeWalker(board, NodeFilter.SHOW_TEXT);
      let node;
      while ((node = walker.nextNode())) {
        if (!node.textContent.trim() || !node.parentElement || !visible(node.parentElement))
          continue;
        const container = node.parentElement.closest('[data-layout-bounds]');
        const layout =
          container && board.contains(container) ? container.getBoundingClientRect() : null;
        const range = document.createRange();
        range.selectNodeContents(node);
        for (const text of range.getClientRects()) {
          if (text.width <= 0 || text.height <= 0) continue;
          const outside = (bounds, tolerance) =>
            text.left < bounds.left - tolerance ||
            text.right > bounds.right + tolerance ||
            text.top < bounds.top - tolerance ||
            text.bottom > bounds.bottom + tolerance;
          if (outside(rect, 1)) {
            problems.push(`Text outside artboard: ${node.textContent.trim().slice(0, 100)}`);
            break;
          }
          if (layout && outside(layout, 2)) {
            problems.push(
              `Text outside layout bounds (${label(container)}): ${node.textContent.trim().slice(0, 100)}`,
            );
            break;
          }
        }
      }
      return problems;
    });
    overflow.forEach((problem) => failures.add(problem));
    if (failures.size) throw new Error([...failures].join('\n'));
    const png = await artboard.screenshot({
      animations: 'disabled',
      scale: 'device',
    });
    if (failures.size) throw new Error([...failures].join('\n'));
    const width = png.readUInt32BE(16);
    const height = png.readUInt32BE(20);
    if (width !== 2400 || Math.abs(height - bounds.height * 2) > 2) {
      throw new Error(`Unexpected PNG size ${width}x${height}; expected a 2x artboard capture.`);
    }
    await mkdir(output, { recursive: true });
    const destination = resolve(output, `${view}-${language}.png`);
    await writeFile(destination, png);
    const label = within(root, destination) ? relative(root, destination) : destination;
    console.log(`${label} (${width}×${height})`);
  } catch (error) {
    const details = [...failures].filter((message) => !error.message.includes(message));
    throw new Error(
      `${view}/${language}: ${error.message}${details.length ? `\n${details.join('\n')}` : ''}`,
    );
  } finally {
    await context.close();
  }
}

async function main() {
  const options = parseOptions();
  if (options.help) {
    usage();
    return;
  }
  await access(resolve(root, 'docs/visuals/index.html'));
  const { server, origin } = await localServer(options.port);
  let browser;
  let interrupted;
  let resume;
  const waitForStop = new Promise((done) => {
    resume = done;
  });
  const onInterrupt = () => {
    interrupted = true;
    resume();
    if (browser) void browser.close().catch(() => {});
  };
  process.once('SIGINT', onInterrupt);
  process.once('SIGTERM', onInterrupt);
  try {
    if (options.serve) {
      for (const view of options.views) {
        for (const language of options.languages) console.log(previewUrl(origin, view, language));
      }
      console.log('Local HTML preview. Press Ctrl+C to stop.');
      await waitForStop;
      return;
    }
    const require = createRequire(resolve(web, 'package.json'));
    const { chromium } = require('playwright');
    browser = await chromium.launch({ headless: true });
    for (const view of options.views) {
      for (const language of options.languages) {
        if (interrupted) throw new Error('Rendering interrupted.');
        await render(browser, origin, view, language, options.output);
      }
    }
  } finally {
    process.off('SIGINT', onInterrupt);
    process.off('SIGTERM', onInterrupt);
    try {
      await browser?.close();
    } finally {
      server.closeAllConnections();
      await new Promise((done) => server.close(done));
    }
  }
}

main().catch((error) => {
  console.error(`README visuals: ${error.message}`);
  process.exitCode = 1;
});
