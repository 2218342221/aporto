import { test, expect } from '@playwright/test';
import type { Page, APIRequestContext } from '@playwright/test';
import { mkdir } from 'node:fs/promises';

const api = 'http://127.0.0.1:4318';
async function login(page: Page, token: string) {
  await page.goto('/');
  await page.getByLabel('服务地址', { exact: true }).fill(api);
  await page.getByLabel(/访问令牌/).fill(token);
  await page.getByRole('button', { name: '进入工作台' }).click();
  await expect(page.getByRole('heading', { name: '梳理项目结构与关键模块' })).toBeVisible();
}
async function showEvents(page: Page) {
  await page.getByRole('button', { name: '展开执行进度' }).click();
  await expect(page.getByRole('complementary', { name: '执行进度' })).toBeVisible();
}
async function control(request: APIRequestContext, token: string, action: string, extra = {}) {
  await request.post(`${api}/_fixture`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { action, ...extra },
  });
}

test('submits to the real HTTP fixture when the browser has no randomUUID', async ({
  page,
  request,
}) => {
  await page.addInitScript(() => {
    Object.defineProperty(globalThis.crypto, 'randomUUID', {
      configurable: true,
      value: undefined,
    });
  });
  const token = `http-random-${test.info().workerIndex}`;
  await login(page, token);
  expect(await page.evaluate(() => typeof crypto.randomUUID)).toBe('undefined');
  expect(await page.evaluate(() => typeof crypto.getRandomValues)).toBe('function');
  await page.getByLabel('发送给 Agent 的消息').fill('验证 HTTP 来源下正常提交消息。');
  const submitted = page.waitForResponse(
    (response) =>
      response.request().method() === 'POST' &&
      new URL(response.url()).pathname === '/v1/threads/architecture/turns',
  );
  await page.getByRole('button', { name: '发送消息' }).click();
  const response = await submitted;
  expect(response.status()).toBe(202);
  expect(response.request().postDataJSON().idempotency_key).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
  );
  await expect(page.getByRole('button', { name: '中断任务', exact: true })).toBeVisible();
  await control(request, token, 'complete');
  await expect(page.getByText('任务已完成：验证通过。', { exact: true })).toBeVisible();
});

test('connect, create with selected agent, send, interrupt, and clear memory-only login', async ({
  page,
}) => {
  const token = `create-${test.info().workerIndex}`;
  await login(page, token);
  await expect(page.getByText('关键模块', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '新建会话', exact: false }).click();
  await page.getByLabel('Agent', { exact: true }).selectOption('researcher');
  await page.getByLabel(/会话名称/).fill('评估部署方案');
  await page.getByRole('button', { name: '创建会话' }).click();
  await expect(page.getByRole('heading', { name: '评估部署方案' })).toBeVisible();
  await page.getByLabel('发送给 Agent 的消息').fill('比较本地与远程环境的使用场景。');
  await page.getByRole('button', { name: '发送消息' }).click();
  await expect(page.getByRole('button', { name: '中断任务', exact: true })).toBeVisible();
  await page.getByRole('button', { name: '中断任务', exact: true }).click();
  await expect(page.locator('.terminal-note')).toHaveText('任务已中断');
  expect(
    await page.evaluate(() => ({
      local: Object.keys(localStorage),
      session: Object.keys(sessionStorage),
    })),
  ).toEqual({ local: [], session: [] });
  expect(await page.context().cookies()).toHaveLength(0);
  expect(page.url()).not.toContain(token);
  await page.reload();
  await expect(page.getByRole('heading', { name: '连接工作台' })).toBeVisible();
  await expect(page.getByLabel(/访问令牌/)).toHaveValue('');
});

test('real SSE reconnect sends cursor, deduplicates, and receives terminal result', async ({
  page,
  request,
}) => {
  const token = `reconnect-${test.info().workerIndex}`;
  await login(page, token);
  await showEvents(page);
  await expect(
    page.getByRole('complementary', { name: '执行进度' }).getByRole('status'),
  ).toContainText('实时连接');
  await expect(page.getByTestId('event-7')).toHaveCount(1);
  await control(request, token, 'duplicate');
  await expect(page.getByTestId('event-7')).toHaveCount(1);
  await control(request, token, 'disconnect');
  await expect
    .poll(async () => {
      const response = await request.get(`${api}/_fixture`, {
        headers: { Authorization: `Bearer ${token}` },
      });
      const data = await response.json();
      return data.history.filter(
        (h: { after: string; last: string }) => h.after === '7' && h.last === '7',
      ).length;
    })
    .toBeGreaterThan(0);
  await expect(page.getByTestId('event-7')).toHaveCount(1);
  await page.getByLabel('发送给 Agent 的消息').fill('继续检查构建配置。');
  await page.getByRole('button', { name: '发送消息' }).click();
  await expect(page.getByRole('button', { name: '中断任务', exact: true })).toBeVisible();
  await control(request, token, 'complete');
  await expect(page.getByText('任务已完成：验证通过。', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: '发送消息' })).toBeVisible();
});

test('switching threads ignores late snapshots and previous-thread completion events', async ({
  page,
  request,
}) => {
  const token = `stale-${test.info().workerIndex}`;
  await login(page, token);
  await page.getByLabel('发送给 Agent 的消息').fill('在旧会话运行的任务。');
  await page.getByRole('button', { name: '发送消息' }).click();
  await expect(page.getByRole('button', { name: '中断任务', exact: true })).toBeVisible();
  let delayedRead = false;
  let delayedReadFinished = false;
  await page.route('**/v1/threads/research?**', async (route) => {
    delayedRead = true;
    await new Promise((resolve) => setTimeout(resolve, 350));
    await route
      .fulfill({
        json: {
          thread: {
            id: 'research',
            title: '旧会话延迟响应',
            agent_id: 'researcher',
            bundle_digest: 'fixture',
            sandbox_id: null,
            created_at: Date.now(),
            updated_at: Date.now(),
            last_sequence: 1,
          },
          turns: [
            {
              id: 'old-response',
              thread_id: 'research',
              input: '不能混入新会话的内容',
              status: 'completed',
              output: '旧会话内容',
              error: null,
              created_at: Date.now(),
              updated_at: Date.now(),
            },
          ],
          next_cursor: null,
        },
      })
      .catch(() => {});
    delayedReadFinished = true;
  });
  await page.getByRole('button', { name: /整理相关工作与研究方向/ }).click();
  await expect.poll(() => delayedRead).toBe(true);
  await page.getByRole('button', { name: /完善项目的使用文档/ }).click();
  await expect(page.getByRole('heading', { name: '完善项目的使用文档' })).toBeVisible();
  await expect(page.getByRole('heading', { name: '发送消息开始' })).toBeVisible();
  await expect.poll(() => delayedReadFinished).toBe(true);
  await control(request, token, 'complete', { output: '只属于旧会话的异步结果' });
  await expect(page.getByText('不能混入新会话的内容')).toHaveCount(0);
  await expect(page.getByText('只属于旧会话的异步结果')).toHaveCount(0);
  await expect(page.getByRole('heading', { name: '完善项目的使用文档' })).toBeVisible();
});

test('older message pages survive subsequent event-triggered snapshot refreshes', async ({
  page,
  request,
}) => {
  const token = `pagination-${test.info().workerIndex}`;
  await page.route('**/v1/threads/architecture?**', async (route) => {
    const response = await route.fetch();
    const data = await response.json();
    if (new URL(route.request().url()).searchParams.get('before') === 'older-page') {
      data.turns = [
        {
          id: 'older-turn',
          thread_id: 'architecture',
          input: '此前讨论的项目约束',
          output: '保留已有的数据接口。',
          status: 'completed',
          error: null,
          created_at: Date.now() - 600000,
          updated_at: Date.now() - 500000,
        },
      ];
      data.next_cursor = null;
    } else data.next_cursor = 'older-page';
    await route.fulfill({ response, json: data });
  });
  await login(page, token);
  await page.getByRole('button', { name: '加载更早的消息', exact: true }).click();
  await expect(page.getByTestId('turn-older-turn')).toHaveCount(1);
  await expect(page.getByRole('button', { name: '加载更早的消息', exact: true })).toHaveCount(0);
  await page.getByLabel('发送给 Agent 的消息').fill('继续本次任务。');
  await page.getByRole('button', { name: '发送消息' }).click();
  await expect(page.getByRole('button', { name: '中断任务', exact: true })).toBeVisible();
  await control(request, token, 'complete');
  await expect(page.getByText('任务已完成：验证通过。', { exact: true })).toBeVisible();
  await expect(page.getByTestId('turn-older-turn')).toHaveCount(1);
  await expect(page.getByTestId('turn-initial')).toHaveCount(1);
  await expect(page.getByRole('button', { name: '加载更早的消息', exact: true })).toHaveCount(0);
});

test('failed send retry preserves idempotency key and untrusted output cannot execute HTML', async ({
  page,
  request,
}) => {
  const token = `retry-${test.info().workerIndex}`;
  await login(page, token);
  const bodies: Array<{ idempotency_key: string }> = [];
  await page.route('**/v1/threads/architecture/turns', async (route) => {
    bodies.push(route.request().postDataJSON());
    if (bodies.length === 1)
      return route.fulfill({ status: 503, json: { error: { message: '暂时不可用，请重试' } } });
    await route.continue();
  });
  await page.getByLabel('发送给 Agent 的消息').fill('审阅文档内容。');
  await page.getByRole('button', { name: '发送消息' }).click();
  await expect(page.getByRole('alert')).toContainText('暂时不可用，请重试');
  await expect(page.getByLabel('发送给 Agent 的消息')).toHaveValue('审阅文档内容。');
  await page.getByRole('button', { name: '发送消息' }).click();
  await expect(page.getByRole('button', { name: '中断任务', exact: true })).toBeVisible();
  expect(bodies).toHaveLength(2);
  expect(bodies[0].idempotency_key).toBe(bodies[1].idempotency_key);
  await control(request, token, 'complete', {
    output:
      '<script>window.__injected = true</script>\n\n<img src=x onerror="window.__injected = true">\n\n[危险链接](javascript:alert(1))\n\n![外部图片](https://example.invalid/tracker)\n\n安全文本',
  });
  await expect(page.getByText('安全文本', { exact: true })).toBeVisible();
  expect(
    await page.evaluate(() => (window as unknown as { __injected?: boolean }).__injected),
  ).toBeUndefined();
  await expect(page.locator('a[href^="javascript:"]')).toHaveCount(0);
  await expect(page.locator('.markdown img')).toHaveCount(0);
});

test('invalid credentials remain on login with a useful error', async ({ page }) => {
  await page.goto('/');
  await page.getByLabel('服务地址', { exact: true }).fill(api);
  await page.getByLabel(/访问令牌/).fill('invalid-token');
  await page.getByRole('button', { name: '进入工作台' }).click();
  await expect(page.getByRole('alert')).toHaveText('访问令牌无效');
  await expect(page.getByRole('heading', { name: '连接工作台' })).toBeVisible();
});

test('structured transient SSE errors recover automatically with the last cursor', async ({
  page,
  request,
}) => {
  const token = `stream-error-${test.info().workerIndex}`;
  await login(page, token);
  await showEvents(page);
  await expect(page.getByTestId('event-7')).toHaveCount(1);
  await control(request, token, 'stream-error');
  await expect
    .poll(async () => {
      const response = await request.get(`${api}/_fixture`, {
        headers: { Authorization: `Bearer ${token}` },
      });
      const data = await response.json();
      return data.history.filter((item: { after: string }) => item.after === '7').length;
    })
    .toBeGreaterThan(0);
  await expect(
    page.getByRole('complementary', { name: '执行进度' }).getByRole('status'),
  ).toContainText('实时连接');
  await expect(page.getByTestId('event-7')).toHaveCount(1);
});

test('slow snapshot reads remain bounded while tool events continue', async ({ page, request }) => {
  const token = `slow-refresh-${test.info().workerIndex}`;
  let active = 0;
  let maximum = 0;
  await page.route('**/v1/threads/architecture?**', async (route) => {
    maximum = Math.max(maximum, ++active);
    try {
      const response = await route.fetch();
      await new Promise((resolve) => setTimeout(resolve, 350));
      await route.fulfill({ response });
    } catch {
      /* The test can close the page during the last read. */
    } finally {
      active--;
    }
  });
  await login(page, token);
  await expect(page.getByText('关键模块', { exact: true })).toBeVisible();
  await showEvents(page);
  // StrictMode can cancel the initial read while its intercepted route is still waiting.
  // Measure the live refresh loop after those startup routes have settled.
  await expect.poll(() => active).toBe(0);
  maximum = 0;
  for (let i = 0; i < 12; i++) {
    await control(request, token, 'event');
    await new Promise((resolve) => setTimeout(resolve, 70));
  }
  await expect(page.getByTestId('event-19')).toHaveCount(1);
  await expect.poll(() => active).toBe(0);
  expect(maximum).toBe(1);
});

test('a new page of turns cannot hide the messages between it and loaded history', async ({
  page,
  request,
}) => {
  const token = `history-gap-${test.info().workerIndex}`;
  await login(page, token);
  await expect(page.getByTestId('turn-initial')).toHaveCount(1);
  await control(request, token, 'append-turns', { count: 30 });
  await expect(page.getByText('补齐第30条消息', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '加载更早的消息', exact: true }).click();
  await expect(page.getByText('补齐第1条消息', { exact: true })).toBeVisible();
  await expect(page.locator('.turn')).toHaveCount(31);
  await expect(page.getByRole('button', { name: '加载更早的消息', exact: true })).toHaveCount(0);
});

test('desktop and mobile workbench screenshots', async ({ page }) => {
  const artifacts = process.env.APORTO_SCREENSHOT_DIR || '/tmp/aporto-web-artifacts';
  await mkdir(artifacts, { recursive: true });
  await page.setViewportSize({ width: 1512, height: 982 });
  await page.goto('/');
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({
    path: `${artifacts}/login-desktop.png`,
    fullPage: true,
    animations: 'disabled',
  });
  await login(page, `screenshots-${test.info().workerIndex}`);
  await expect(page.getByRole('button', { name: '展开执行进度' })).toBeVisible();
  await expect(page.getByRole('complementary', { name: '执行进度' })).toHaveCount(0);
  await expect(page.getByText('关键模块', { exact: true })).toBeVisible();
  await page.evaluate(() => document.fonts.ready);
  await page.locator('.messages').evaluate(async (element) => {
    await new Promise(requestAnimationFrame);
    element.scrollTop = 0;
    await new Promise(requestAnimationFrame);
  });
  await page.screenshot({
    path: `${artifacts}/workbench-desktop.png`,
    fullPage: true,
    animations: 'disabled',
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator('.messages').evaluate(async (element) => {
    await new Promise(requestAnimationFrame);
    element.scrollTop = 0;
    await new Promise(requestAnimationFrame);
  });
  await page.screenshot({
    path: `${artifacts}/workbench-mobile.png`,
    fullPage: true,
    animations: 'disabled',
  });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(
    true,
  );
  await page.getByRole('button', { name: '打开会话列表' }).click();
  await expect(page.getByRole('button', { name: /完善项目的使用文档/ })).toBeVisible();
  await page.getByRole('button', { name: /完善项目的使用文档/ }).click();
  await expect(page.getByRole('heading', { name: '完善项目的使用文档' })).toBeVisible();
  await expect(page.getByRole('button', { name: '打开会话列表' })).toBeVisible();
});
