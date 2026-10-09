import { test, expect } from '@playwright/test';
import type { APIRequestContext, Locator, Page } from '@playwright/test';
import { mkdir } from 'node:fs/promises';

const api = 'http://127.0.0.1:4318';
async function login(page: Page, token: string) {
  await page.goto('/');
  await page.getByLabel('服务地址', { exact: true }).fill(api);
  await page.getByLabel(/访问令牌/).fill(token);
  await page.getByRole('button', { name: '进入工作台' }).click();
  await expect(page.getByTestId('turn-initial')).toBeVisible();
}
async function expand(details: Locator) {
  if (!(await details.evaluate((element) => (element as HTMLDetailsElement).open)))
    await details.locator(':scope > summary').click();
}
async function control(request: APIRequestContext, token: string, action: string, extra = {}) {
  const response = await request.post(`${api}/_fixture`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { action, ...extra },
  });
  expect(response.ok()).toBe(true);
}
async function start(page: Page) {
  await page.getByLabel('发送给 Agent 的消息').fill('检查运行过程与结果。');
  const response = page.waitForResponse(
    (value) =>
      value.request().method() === 'POST' &&
      new URL(value.url()).pathname === '/v1/threads/architecture/turns',
  );
  await page.getByRole('button', { name: '发送消息' }).click();
  const turn = await (await response).json();
  await expect(page.getByRole('button', { name: '中断任务', exact: true })).toBeVisible();
  return turn.id as string;
}

test('shows thinking and compact commands, folds exec code, then collapses process around the final answer', async ({
  page,
  request,
}) => {
  const token = `activity-live-${test.info().workerIndex}`;
  await login(page, token);
  const turnId = await start(page);
  const post = (item: object) => control(request, token, 'item', { turn_id: turnId, item });
  const progress = page.getByTestId(`turn-process-${turnId}`);
  await expect(page.getByTestId(`turn-${turnId}`).getByRole('status')).toHaveText('正在思考');
  await post({ id: 'comment', text: '先检查依赖，再运行验证。' });
  await expect(page.getByTestId('item-comment')).toHaveText('先检查依赖，再运行验证。');
  await expect(page.getByRole('button', { name: '中断任务', exact: true })).toBeVisible();
  await post({ id: 'comment', text: '先检查依赖，再运行验证。', status: 'completed' });
  await post({
    id: 'exec',
    kind: 'ptc_call',
    phase: null,
    name: 'exec',
    input:
      'const results = await Promise.all([tools.read_file({path: "Cargo.toml"}), tools.exec_command({cmd: "cargo test --workspace"})]);\ntext(results);',
  });
  await post({
    id: 'check',
    kind: 'tool_call',
    phase: null,
    name: 'tools.read_file',
    input: '{"path":"Cargo.toml"}',
  });
  await post({
    id: 'test',
    kind: 'tool_call',
    phase: null,
    name: 'tools.exec_command',
    input: '{"cmd":"cargo test --workspace"}',
  });
  await expect(progress.locator(':scope > summary')).toContainText('已处理');
  await expect(page.getByTestId('item-check')).not.toBeVisible();
  await expect(page.getByTestId('item-exec').locator(':scope > summary')).not.toContainText('exec');
  await page.getByTestId('item-exec').locator(':scope > summary').click();
  await expect(page.getByTestId('item-test').locator('summary')).toContainText('正在运行');
  await expect(page.getByTestId('item-test').locator('summary')).toContainText(
    'cargo test --workspace',
  );
  await expect(page.getByTestId('item-check').locator('summary')).toContainText('Cargo.toml');
  await post({ id: 'test', status: 'completed', output: '29 tests passed', elapsed_ms: 340 });
  await post({
    id: 'check',
    status: 'completed',
    output: '<img src=x onerror="window.__activityInjected=true">\n依赖有效',
    elapsed_ms: 580,
  });
  await post({ id: 'exec', status: 'completed', output: '检查完成', elapsed_ms: 610 });
  await progress.locator('summary').filter({ hasText: '执行代码与输出' }).click();
  await expect(page.getByTestId('item-exec').locator('pre').first()).toContainText('Promise.all');
  await page.getByTestId('item-check').locator('summary').click();
  await expect(page.getByTestId('item-check').locator('pre').last()).toContainText('依赖有效');
  await expect(page.getByTestId('item-check').locator('img')).toHaveCount(0);
  expect(
    await page.evaluate(
      () => (window as unknown as { __activityInjected?: boolean }).__activityInjected,
    ),
  ).toBeUndefined();
  await expect(page.getByTestId('item-check').getByLabel('耗时 580 毫秒')).toBeVisible();
  expect(
    await progress
      .locator('[data-testid="item-check"], [data-testid="item-test"]')
      .evaluateAll((rows) => rows.map((row) => row.getAttribute('data-testid'))),
  ).toEqual(['item-check', 'item-test']);
  await post({ id: 'final', phase: 'final_answer', text: '依赖检查通过，' });
  await expect(page.getByTestId('item-final')).toContainText('依赖检查通过，');
  await expect(page.getByRole('button', { name: '中断任务', exact: true })).toBeVisible();
  await post({ id: 'final', text: '依赖检查通过，29 项测试通过。' });
  await control(request, token, 'complete');
  await expect(page.getByText('依赖检查通过，29 项测试通过。', { exact: true })).toHaveCount(1);
  await expect(progress).toHaveJSProperty('open', false);
  await expect(progress.locator(':scope > summary')).toContainText('用时');
  await expect(page.getByTestId('item-comment')).not.toBeVisible();
  await expect(page.getByTestId('item-final')).toBeVisible();
  await expect(progress.getByTestId('item-final')).toHaveCount(0);
  await progress.locator(':scope > summary').click();
  await expect(page.getByTestId('item-comment')).toBeVisible();
  await expect(page.getByTestId('item-final')).toBeVisible();
  await expect(page.getByRole('button', { name: '发送消息' })).toBeVisible();
});

test('keeps streamed updates over delayed snapshots, deduplicates replay, and restores after login', async ({
  page,
  request,
}) => {
  const token = `activity-replay-${test.info().workerIndex}`;
  await login(page, token);
  const turnId = await start(page);
  await control(request, token, 'item', {
    turn_id: turnId,
    item: { id: 'restore', text: '第一段说明' },
  });
  await expect(page.getByTestId('item-restore')).toContainText('第一段说明');
  let captured = false;
  let released = false;
  await page.route(`**/turns/${turnId}/items?**`, async (route) => {
    const response = await route.fetch();
    captured = true;
    await new Promise((resolve) => setTimeout(resolve, 400));
    await route.fulfill({ response }).catch(() => {});
    released = true;
  });
  await control(request, token, 'disconnect');
  await expect.poll(() => captured).toBe(true);
  await control(request, token, 'item', {
    turn_id: turnId,
    item: { id: 'restore', text: '第一段说明，第二段已实时到达。' },
  });
  await control(request, token, 'duplicate');
  await expect(page.getByTestId('item-restore')).toHaveCount(1);
  await expect(page.getByTestId('item-restore')).toContainText('第二段已实时到达');
  await expect.poll(() => released).toBe(true);
  await expect(page.getByTestId('item-restore')).toContainText('第二段已实时到达');
  await page.unroute(`**/turns/${turnId}/items?**`);
  await control(request, token, 'complete', { output: '已完成验证。' });
  await expect(page.getByText('已完成验证。', { exact: true })).toBeVisible();
  await page.reload();
  await login(page, token);
  await expect(page.getByTestId(`turn-process-${turnId}`)).toHaveJSProperty('open', false);
  await page.getByTestId(`turn-process-${turnId}`).locator(':scope > summary').click();
  await expect(page.getByTestId('item-restore')).toHaveCount(1);
  await expect(page.getByTestId('item-restore')).toContainText('第二段已实时到达');
  await expect(page.getByText('已完成验证。', { exact: true })).toHaveCount(1);
});

test('loads activity history one page at a time and retains it through later turns', async ({
  page,
  request,
}) => {
  const token = `activity-pages-${test.info().workerIndex}`;
  await control(request, token, 'append-items', { count: 105, silent: true });
  await login(page, token);
  const initial = page.getByTestId('turn-initial');
  await expect(initial.locator('[data-testid^="item-"]')).toHaveCount(100);
  await expect(page.getByTestId('turn-process-initial')).toHaveJSProperty('open', false);
  await page.getByTestId('turn-process-initial').locator(':scope > summary').click();
  await initial.getByRole('button', { name: '加载更多活动' }).click();
  await expect(initial.locator('[data-testid^="item-"]')).toHaveCount(105);
  await expect(initial.getByRole('button', { name: '加载更多活动' })).toHaveCount(0);
  await start(page);
  await control(request, token, 'complete');
  await expect(page.getByText('任务已完成：验证通过。', { exact: true })).toBeVisible();
  await expect(initial.locator('[data-testid^="item-"]')).toHaveCount(105);
  await control(request, token, 'disconnect');
  await page.getByRole('button', { name: '展开执行进度' }).click();
  await expect(
    page.getByRole('complementary', { name: '执行进度' }).getByRole('status'),
  ).toContainText('实时连接');
  await expect(initial.locator('[data-testid^="item-"]')).toHaveCount(105);
});

test('follows live activity at the bottom but preserves a manually scrolled position', async ({
  page,
  request,
}) => {
  const token = `activity-scroll-${test.info().workerIndex}`;
  await login(page, token);
  const turnId = await start(page);
  const paragraphs = Array.from({ length: 35 }, (_, index) => `检查段落 ${index + 1}。`).join(
    '\n\n',
  );
  await control(request, token, 'item', {
    turn_id: turnId,
    item: { id: 'scroll', text: paragraphs },
  });
  await expect(page.getByTestId('item-scroll')).toContainText('检查段落 35。');
  const remaining = () =>
    page
      .locator('.messages')
      .evaluate((element) => element.scrollHeight - element.scrollTop - element.clientHeight);
  await expect.poll(remaining).toBeLessThan(4);
  await page.locator('.messages').evaluate((element) => {
    element.scrollTop = 0;
    element.dispatchEvent(new Event('scroll'));
  });
  await control(request, token, 'item', {
    turn_id: turnId,
    item: { id: 'scroll', text: `${paragraphs}\n\n新增说明。` },
  });
  await expect(page.getByTestId('item-scroll')).toContainText('新增说明。');
  await expect
    .poll(() => page.locator('.messages').evaluate((element) => element.scrollTop))
    .toBe(0);
  await control(request, token, 'complete', { output: '检查完毕。' });
  await expect(page.getByTestId(`turn-process-${turnId}`)).toHaveJSProperty('open', false);
  await expect(page.getByText('检查完毕。', { exact: true })).toHaveCount(1);
  await expect
    .poll(() => page.locator('.messages').evaluate((element) => element.scrollTop))
    .toBe(0);
});

test('shows failed tool output and interrupted partial messages on mobile', async ({
  page,
  request,
}) => {
  const token = `activity-mobile-${test.info().workerIndex}`;
  await page.setViewportSize({ width: 390, height: 844 });
  await login(page, token);
  await expect(page.getByRole('button', { name: '展开执行进度' })).toBeVisible();
  const turnId = await start(page);
  await control(request, token, 'item', {
    turn_id: turnId,
    item: { id: 'partial', phase: 'final_answer', text: '已完成第一部分检查，' },
  });
  await control(request, token, 'item', {
    turn_id: turnId,
    item: {
      id: 'failed-tool',
      kind: 'tool_call',
      phase: null,
      name: 'tools.exec_command',
      input: '{"cmd":"cargo test"}',
      status: 'failed',
      output: 'error: test failed',
      error: '命令退出码 1',
      elapsed_ms: 1500,
      truncated: true,
    },
  });
  await control(request, token, 'item', {
    turn_id: turnId,
    item: {
      id: 'wait',
      kind: 'ptc_call',
      phase: null,
      name: 'wait',
      input: '{"cell_id":"cell-1"}',
    },
  });
  await expand(page.getByTestId('item-failed-tool'));
  await expect(page.getByTestId('item-failed-tool').getByRole('alert')).toHaveText('命令退出码 1');
  await expect(page.getByTestId('item-failed-tool').getByText('内容已截断')).toBeVisible();
  await page.getByRole('button', { name: '中断任务', exact: true }).click();
  await expect(page.getByTestId('item-partial')).toContainText('已完成第一部分检查，');
  await expect(page.getByTestId('item-partial').getByText('已中断', { exact: true })).toBeVisible();
  await expect(page.getByTestId('item-wait').locator('summary')).toContainText('已中断');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});

test('uses the completed turn answer over stale, truncated, multiple, or empty streamed finals', async ({
  page,
  request,
}) => {
  const token = `activity-canonical-${test.info().workerIndex}`;
  await login(page, token);
  const cases = [
    [
      { id: 'earlier', text: '第一段过期结果', status: 'completed' },
      { id: 'latest', text: '第二段截断结果', truncated: true },
    ],
    [{ id: 'empty', text: '' }],
  ];
  for (const [index, items] of cases.entries()) {
    const turnId = await start(page);
    for (const item of items)
      await control(request, token, 'item', {
        turn_id: turnId,
        item: { phase: 'final_answer', ...item },
      });
    const output = `完整结果 ${index + 1}：所有步骤已完成。`;
    await control(request, token, 'complete', { output });
    const turn = page.getByTestId(`turn-${turnId}`);
    await expect(turn.getByText(output, { exact: true })).toHaveCount(1);
    await expect(turn.getByText(output, { exact: true })).toBeVisible();
    await expect(turn.getByText('第一段过期结果', { exact: true })).not.toBeVisible();
    await expect(turn.getByText('第二段截断结果', { exact: true })).not.toBeVisible();
    await expect(turn.getByText('内容已截断', { exact: true })).not.toBeVisible();
    await expect(page.getByRole('button', { name: '发送消息' })).toBeVisible();
  }
});

test('keeps the final answer visible before its history page loads and preserves an expanded process', async ({
  page,
  request,
}) => {
  const token = `activity-final-page-${test.info().workerIndex}`;
  const response = await request.get(`${api}/v1/threads/architecture`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  const original = (await response.json()).turns[0].output;
  await control(request, token, 'append-items', { count: 100, silent: true });
  await control(request, token, 'item', {
    turn_id: 'initial',
    silent: true,
    item: { id: 'history-final', phase: 'final_answer', text: original, status: 'completed' },
  });
  await login(page, token);
  const progress = page.getByTestId('turn-process-initial');
  await expect(progress).toHaveJSProperty('open', false);
  await expect(page.getByRole('heading', { name: '关键模块', exact: true })).toBeVisible();
  await expect(page.getByTestId('item-history-final')).toHaveCount(0);
  await progress.locator(':scope > summary').click();
  await progress.getByRole('button', { name: '加载更多活动' }).click();
  await expect(page.getByTestId('item-history-final')).toBeVisible();
  await expect(page.getByRole('heading', { name: '关键模块', exact: true })).toHaveCount(1);
  await expect(progress.getByTestId('item-history-final')).toHaveCount(0);
  await control(request, token, 'disconnect');
  await expect(progress).toHaveJSProperty('open', true);
  await expect(page.getByTestId('item-history-item-99')).toBeVisible();
  await start(page);
  await control(request, token, 'complete');
  await expect(page.getByText('任务已完成：验证通过。', { exact: true })).toBeVisible();
  await expect(progress).toHaveJSProperty('open', true);
  await expect(page.getByTestId('item-history-final')).toBeVisible();
});

test('keeps a standalone failed exec inspectable and reports the turn failure', async ({
  page,
  request,
}) => {
  const token = `activity-failed-ptc-${test.info().workerIndex}`;
  await login(page, token);
  const turnId = await start(page);
  await control(request, token, 'item', {
    turn_id: turnId,
    item: {
      id: 'failed-exec',
      kind: 'ptc_call',
      phase: null,
      name: 'exec',
      input: 'throw new Error("validation failed");',
      status: 'failed',
      output: 'partial execution output',
      error: 'validation failed',
    },
  });
  await control(request, token, 'fail');
  const progress = page.getByTestId(`turn-process-${turnId}`);
  await expect(progress).toHaveJSProperty('open', true);
  const operation = page.getByTestId('item-failed-exec');
  await expand(operation);
  await expect(operation.getByRole('alert')).toHaveText('validation failed');
  await expect(operation.locator('pre').last()).toContainText('partial execution output');
  await expect(
    page.getByTestId(`turn-${turnId}`).getByText('模型连接失败', { exact: true }),
  ).toBeVisible();
});

test('shows confirmed file writes below the answer and captures the conversation states', async ({
  page,
  request,
}) => {
  const token = `activity-file-${test.info().workerIndex}`;
  const artifacts = process.env.APORTO_SCREENSHOT_DIR || '/tmp/aporto-web-artifacts';
  await mkdir(artifacts, { recursive: true });
  await page.setViewportSize({ width: 1512, height: 982 });
  await login(page, token);
  await page.getByRole('button', { name: /完善项目的使用文档/ }).click();
  await expect(page.getByRole('heading', { name: '完善项目的使用文档' })).toBeVisible();
  await page.getByLabel('发送给 Agent 的消息').fill('这里留一个 TODO 吧');
  const submitted = page.waitForResponse(
    (response) =>
      response.request().method() === 'POST' &&
      new URL(response.url()).pathname === '/v1/threads/docs/turns',
  );
  await page.getByRole('button', { name: '发送消息' }).click();
  const turnId = (await (await submitted).json()).id as string;
  const post = (item: object) => control(request, token, 'item', { turn_id: turnId, item });
  const progress = page.getByTestId(`turn-process-${turnId}`);
  await expect(page.getByTestId(`turn-${turnId}`).getByRole('status')).toHaveText('正在思考');
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({
    path: `${artifacts}/conversation-thinking-desktop.png`,
    fullPage: true,
    animations: 'disabled',
  });
  await post({
    id: 'plan',
    text: '我会在轮末停止容器的位置留一个 TODO，注明后续可优化：同实例还有排队任务时保持运行。',
    status: 'completed',
  });
  await post({
    id: 'command-code',
    kind: 'ptc_call',
    phase: null,
    name: 'exec',
    input: 'text(await tools.exec_command({cmd: "git status --short"}));',
  });
  await post({
    id: 'command',
    kind: 'tool_call',
    phase: null,
    name: 'tools.exec_command',
    input: '{"cmd":"git status --short"}',
  });
  await expect(page.getByTestId('item-command-code').locator(':scope > summary')).toContainText(
    '正在运行',
  );
  await expect(page.getByTestId('item-command-code').locator(':scope > summary')).toContainText(
    'git status --short',
  );
  await expect(page.getByTestId('item-command')).not.toBeVisible();
  await page.screenshot({
    path: `${artifacts}/conversation-running-desktop.png`,
    fullPage: true,
    animations: 'disabled',
  });
  await post({ id: 'command', status: 'completed', output: '', elapsed_ms: 300 });
  await post({ id: 'command-code', status: 'completed', output: '', elapsed_ms: 400 });
  const content = '// TODO: Keep this instance running while another turn is queued.\n';
  await post({
    id: 'write',
    kind: 'tool_call',
    phase: null,
    name: 'tools.write_file',
    input: JSON.stringify({ path: 'crates/core/src/executor.rs', content }),
    status: 'completed',
    output: JSON.stringify({
      path: '/workspace/crates/core/src/executor.rs',
      bytes: new TextEncoder().encode(content).length,
    }),
    elapsed_ms: 12,
  });
  await post({
    id: 'final',
    phase: 'final_answer',
    text: '已添加 TODO：同实例有排队任务时，保持容器运行并安全交接，避免重复 `stop/start`。',
  });
  await control(request, token, 'complete');
  await expect(progress).toHaveJSProperty('open', false);
  await expect(page.getByTestId('item-final')).toBeVisible();
  const card = page.getByTestId('file-result-write');
  await expect(card).toBeVisible();
  await expect(card.locator('summary')).toContainText('已写入 executor.rs');
  await expect(card.locator('.file-result-path')).toHaveAttribute(
    'title',
    '/workspace/crates/core/src/executor.rs',
  );
  await expect(progress.getByTestId('file-result-write')).toHaveCount(0);
  await expect(page.getByRole('button', { name: '撤销', exact: true })).toHaveCount(0);
  await page.screenshot({
    path: `${artifacts}/conversation-completed-desktop.png`,
    fullPage: true,
    animations: 'disabled',
  });
  await card.locator('summary').click();
  await expect(card.locator('pre')).toHaveText(content);
  await expect(card.locator('.file-result-full-path')).toHaveText(
    '/workspace/crates/core/src/executor.rs',
  );
  await card.locator('summary').click();
  await progress.locator(':scope > summary').click();
  await expect(page.getByTestId('item-plan')).toBeVisible();
  await expect(page.getByTestId('item-final')).toBeVisible();
  await card.locator('summary').click();
  await page.screenshot({
    path: `${artifacts}/conversation-expanded-desktop.png`,
    fullPage: true,
    animations: 'disabled',
  });
  await card.locator('summary').click();
  await progress.locator(':scope > summary').click();
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(card.locator('.file-result-path')).toHaveText('已写入 executor.rs');
  await page.screenshot({
    path: `${artifacts}/conversation-completed-mobile.png`,
    fullPage: true,
    animations: 'disabled',
  });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});

test('copies the canonical answer on HTTP, restores focus, and reports clipboard failure', async ({
  page,
  request,
}) => {
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: undefined });
    Object.defineProperty(document, 'execCommand', {
      configurable: true,
      value(command: string) {
        if (command !== 'copy') return false;
        const field = document.activeElement;
        (window as unknown as { copiedText: string }).copiedText =
          field instanceof HTMLTextAreaElement ? field.value : '';
        return true;
      },
    });
  });
  const token = `activity-copy-${test.info().workerIndex}`;
  await login(page, token);
  const turnId = await start(page);
  await control(request, token, 'item', {
    turn_id: turnId,
    item: { id: 'draft', phase: 'final_answer', text: '过期回答片段' },
  });
  const output = '完整回答：**复制原始 Markdown**，保留 `code`。';
  await control(request, token, 'complete', { output });
  const turn = page.getByTestId(`turn-${turnId}`);
  const copy = turn.getByRole('button', { name: '复制回答', exact: true });
  await copy.click();
  await expect(turn.getByText('已复制', { exact: true })).toBeVisible();
  expect(await page.evaluate(() => (window as unknown as { copiedText: string }).copiedText)).toBe(
    output,
  );
  await expect(page.locator('textarea[aria-hidden="true"]')).toHaveCount(0);
  await expect(copy).toBeFocused();
  await page.evaluate(() =>
    Object.defineProperty(document, 'execCommand', { configurable: true, value: () => false }),
  );
  await copy.click();
  await expect(turn.getByText('复制失败，请选择文本复制', { exact: true })).toBeVisible();
  await expect(page.locator('textarea[aria-hidden="true"]')).toHaveCount(0);
  await expect(copy).toBeFocused();
});
