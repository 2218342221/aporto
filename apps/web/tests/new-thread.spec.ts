import { test, expect } from '@playwright/test';
import type { APIRequestContext, Page } from '@playwright/test';

const api = 'http://127.0.0.1:4318';
async function login(page: Page, token: string) {
  await page.goto('/');
  await page.getByLabel('服务地址', { exact: true }).fill(api);
  await page.getByLabel(/访问令牌/).fill(token);
  await page.getByRole('button', { name: '进入工作台' }).click();
  await expect(page.getByTestId('turn-initial')).toBeVisible();
}
async function openDialog(page: Page) {
  const menu = page.getByRole('button', { name: '打开会话列表' });
  if (await menu.isVisible()) await menu.click();
  await page
    .getByRole('complementary', { name: '会话列表' })
    .getByRole('button', { name: '新建会话' })
    .click();
  return page.getByRole('dialog', { name: '新建会话' });
}
async function create(page: Page) {
  const response = page.waitForResponse(
    (value) =>
      value.request().method() === 'POST' && new URL(value.url()).pathname === '/v1/threads',
  );
  await page.getByRole('button', { name: '创建会话', exact: true }).click();
  const result = await response;
  expect(result.status()).toBe(201);
  return { input: result.request().postDataJSON(), thread: await result.json() };
}
async function seed(request: APIRequestContext, token: string) {
  await request.post(`${api}/_fixture`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { action: 'append-instances', count: 101 },
  });
}

test('uses new-instance defaults and transmits selected image and custom workdir', async ({
  page,
}) => {
  await login(page, `new-default-${test.info().workerIndex}`);
  await openDialog(page);
  await expect(page.getByRole('radio', { name: '新建实例' })).toBeChecked();
  await expect(page.getByLabel('镜像', { exact: true })).toHaveValue('node:22-bookworm');
  await expect(page.getByLabel('工作目录')).toHaveValue('/workspace');
  const defaults = await create(page);
  expect(defaults.input.runtime).toEqual({ mode: 'new', image: 'node:22-bookworm' });
  expect(defaults.input.workdir).toBe('/workspace');
  expect(defaults.thread.sandbox_id).toBeNull();
  await openDialog(page);
  await page.getByLabel('镜像', { exact: true }).selectOption('python:3.12-slim');
  await page.getByLabel('工作目录').fill('/srv/project/');
  await page.getByLabel(/会话名称/).fill('自定义 Python 环境');
  const selected = await create(page);
  expect(selected.input.runtime).toEqual({ mode: 'new', image: 'python:3.12-slim' });
  expect(selected.input.workdir).toBe('/srv/project');
  await expect(page.locator('.conversation-runtime')).toContainText('python:3.12-slim');
  await expect(page.locator('.conversation-runtime')).toContainText('/srv/project');
});

test('reuses a busy old-release instance with fixed image and an overridden directory', async ({
  page,
}) => {
  await login(page, `new-reuse-${test.info().workerIndex}`);
  await openDialog(page);
  await page.getByRole('radio', { name: '复用实例' }).check();
  await expect(
    page.getByLabel('已有实例').locator('option[value="instance-reviewer-legacy"]'),
  ).toContainText('使用中');
  await page.getByLabel('已有实例').selectOption('instance-reviewer-legacy');
  await expect(page.getByLabel('工作目录')).toHaveValue('/workspace/legacy');
  await expect(page.getByLabel('镜像', { exact: true })).toHaveValue('node:20-bookworm');
  await expect(page.getByLabel('镜像', { exact: true })).toHaveAttribute('readonly', '');
  await expect(page.getByText('使用中，任务将排队执行。')).toBeVisible();
  await page.getByLabel('工作目录').fill('/data/another-task');
  const result = await create(page);
  expect(result.input.runtime).toEqual({ mode: 'reuse', instance_id: 'instance-reviewer-legacy' });
  expect(result.input.workdir).toBe('/data/another-task');
  expect(result.thread.release_id).toBe('sha256:release-reviewer-old');
  expect(result.thread.runtime_image).toBe('node:20-bookworm');
  await expect(page.locator('.conversation-runtime')).toContainText('node:20-bookworm');
});

test('discards stale instance pages after agent changes and preserves user-edited workdir on load', async ({
  page,
  request,
}) => {
  const token = `new-races-${test.info().workerIndex}`;
  await seed(request, token);
  await login(page, token);
  await openDialog(page);
  await page.getByRole('radio', { name: '复用实例' }).check();
  await expect(page.getByLabel('已有实例').locator('option')).toHaveCount(51);
  await page.getByLabel('已有实例').selectOption('instance-reviewer-default');
  await page.getByLabel('工作目录').fill('/custom/keep');
  await page.getByRole('button', { name: '加载更多实例' }).click();
  await expect(page.getByLabel('已有实例').locator('option')).toHaveCount(101);
  await expect(page.getByLabel('工作目录')).toHaveValue('/custom/keep');
  let oldPending = false;
  let oldFinished = false;
  await page.route('**/v1/agents/reviewer/instances?**', async (route) => {
    const response = await route.fetch();
    oldPending = true;
    await new Promise((resolve) => setTimeout(resolve, 400));
    await route.fulfill({ response }).catch(() => {});
    oldFinished = true;
  });
  await page.getByRole('button', { name: '加载更多实例' }).click();
  await expect.poll(() => oldPending).toBe(true);
  await expect(page.getByLabel('工作目录')).toHaveValue('/custom/keep');
  await page.route('**/v1/agents/researcher/instances?**', async (route) => {
    const response = await route.fetch();
    await new Promise((resolve) => setTimeout(resolve, 300));
    await route.fulfill({ response }).catch(() => {});
  });
  await page.getByLabel('Agent', { exact: true }).selectOption('researcher');
  await expect(page.getByLabel('工作目录')).toHaveValue('/workspace/research');
  await page.getByLabel('工作目录').fill('/custom/research');
  await expect(
    page.getByLabel('已有实例').locator('option[value="instance-researcher-ready"]'),
  ).toHaveCount(1);
  await expect.poll(() => oldFinished).toBe(true);
  await expect(page.getByLabel('工作目录')).toHaveValue('/custom/research');
  await expect(page.getByLabel('已有实例').locator('option')).toHaveCount(2);
  await page.getByLabel('已有实例').selectOption('instance-researcher-ready');
  await expect(page.getByLabel('工作目录')).toHaveValue('/workspace/research');
  await page.getByRole('radio', { name: '新建实例' }).check();
  await expect(page.getByLabel('环境模板', { exact: true })).toHaveValue('research-v1');
});

test('retries empty instance lists and validates directories in a scrollable mobile dialog', async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await login(page, `new-mobile-${test.info().workerIndex}`);
  const dialog = await openDialog(page);
  let attempts = 0;
  await page.route('**/v1/agents/reviewer/instances?**', async (route) => {
    attempts++;
    await route.fulfill(
      attempts === 1
        ? { status: 503, json: { error: { message: '实例列表暂时不可用' } } }
        : { json: { instances: [], next_cursor: null } },
    );
  });
  await page.getByRole('radio', { name: '复用实例' }).check();
  await expect(dialog.getByRole('alert')).toContainText('实例列表暂时不可用');
  await page.getByRole('button', { name: '重试加载实例' }).click();
  await expect(page.getByText('暂无可复用实例')).toBeVisible();
  await expect(page.getByRole('button', { name: '创建会话', exact: true })).toBeDisabled();
  await page.getByRole('radio', { name: '新建实例' }).check();
  await page.getByLabel('工作目录').fill('/workspace/../other');
  await page.getByRole('button', { name: '创建会话', exact: true }).click();
  await expect(dialog.getByRole('alert')).toContainText('绝对工作目录');
  await page.getByLabel('工作目录').fill('/opt/agent/config');
  await page.getByRole('button', { name: '创建会话', exact: true }).click();
  await expect(dialog.getByRole('alert')).toContainText('Aporto 保留');
  expect(
    await dialog.evaluate(
      (element) =>
        element.scrollWidth <= element.clientWidth &&
        element.getBoundingClientRect().height <= innerHeight,
    ),
  ).toBe(true);
  await page.getByLabel('工作目录').fill('/srv/mobile/');
  const result = await create(page);
  expect(result.input.workdir).toBe('/srv/mobile');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});
