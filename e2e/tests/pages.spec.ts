import { expect, request, test, type APIRequestContext } from '@playwright/test';
import * as path from 'path';

const storageState = path.resolve(__dirname, '..', '.auth-state.json');
test.describe('page library', () => {
  test('SPA can open and delete a page', async ({ page, request }) => {
    const created = await request.post('/api/pages', { data: {} });
    expect(created.status()).toBe(201);

    await page.goto('/', { waitUntil: 'load' });

    await expect(page.getByRole('button', { name: 'New page' })).toHaveCount(0);
    const unnamedPage = page.getByRole('button', { name: 'Open Untitled page', exact: true }).first();
    await expect(unnamedPage).toBeVisible();
    await expect(page.getByText('Untitled page')).toBeVisible();
    await unnamedPage.click();
    await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();
    await expectCanvasFillsFrame(page);
    await page.setViewportSize({ width: 720, height: 520 });
    await expectCanvasFillsFrame(page);
    await page.setViewportSize({ width: 1100, height: 700 });
    await expectCanvasFillsFrame(page);
    await page.getByRole('button', { name: 'Back' }).click();

    page.on('dialog', (dialog) => dialog.accept());
    await page.getByRole('button', { name: 'Delete Untitled page', exact: true }).first().click();
    await expect(unnamedPage).toHaveCount(0);
  });

  test('REST pages are owner-scoped', async ({ request }) => {
    const title = uniqueTitle('owner-only');
    const created = await request.post('/api/pages', { data: { title } });
    expect(created.status()).toBe(201);
    const pageId = (await created.json()).page.id;

    const other = await otherOwnerContext();
    const otherGet = await other.get(`/api/pages/${pageId}`);
    expect(otherGet.status()).toBe(403);
    const otherDelete = await other.delete(`/api/pages/${pageId}`);
    expect(otherDelete.status()).toBe(403);
    await other.dispose();

    const ownerDelete = await request.delete(`/api/pages/${pageId}`);
    expect(ownerDelete.status()).toBe(204);
  });

  test('SPA page cards do not show absolute metadata', async ({ page, request }) => {
    const title = uniqueTitle('card-label');
    const created = await request.post('/api/pages', { data: { title } });
    expect(created.status()).toBe(201);
    const pageId = (await created.json()).page.id;

    await page.goto('/', { waitUntil: 'load' });
    await expect(page.getByRole('button', { name: `Open ${title}`, exact: true })).toBeVisible();
    await expect(page.getByText(/Created .* Updated /)).toHaveCount(0);

    const deleted = await request.delete(`/api/pages/${pageId}`);
    expect(deleted.status()).toBe(204);
  });

  test('library events fan out to sibling owner sessions without refresh', async ({ browser, request }) => {
    const title = uniqueTitle('fanout');
    const contextB = await browser.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      storageState,
      extraHTTPHeaders: {},
    });
    const pageB = await contextB.newPage();
    const websocketB = pageB.waitForEvent('websocket');
    await pageB.goto('/', { waitUntil: 'load' });
    await expect(pageB.getByLabel('Page library')).toBeVisible();
    await websocketB;

    const created = await request.post('/api/pages', { data: { title } });
    expect(created.status()).toBe(201);
    const pageId = (await created.json()).page.id;
    await expect(pageB.getByRole('button', { name: `Open ${title}`, exact: true })).toBeVisible({ timeout: 5_000 });

    const deleted = await request.delete(`/api/pages/${pageId}`);
    expect(deleted.status()).toBe(204);
    await expect(pageB.getByRole('button', { name: `Open ${title}`, exact: true })).toHaveCount(0, { timeout: 5_000 });

    await contextB.close();
  });

  test('other owner does not receive library events', async ({ browser, request }) => {
    const title = uniqueTitle('private-fanout');
    const otherToken = await fetchOtherOwnerToken();
    const contextB = await browser.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      storageState: cookieState(otherToken),
      extraHTTPHeaders: {},
    });
    const pageB = await contextB.newPage();
    const websocketB = pageB.waitForEvent('websocket');
    await pageB.goto('/', { waitUntil: 'load' });
    await expect(pageB.getByLabel('Page library')).toBeVisible();
    await websocketB;

    const created = await request.post('/api/pages', { data: { title } });
    expect(created.status()).toBe(201);
    const pageId = (await created.json()).page.id;

    await pageB.waitForTimeout(750);
    await expect(pageB.getByRole('button', { name: `Open ${title}`, exact: true })).toHaveCount(0);

    const ownerDelete = await request.delete(`/api/pages/${pageId}`);
    expect(ownerDelete.status()).toBe(204);
    await contextB.close();
  });
});

async function expectCanvasFillsFrame(page: import('@playwright/test').Page) {
  await expect.poll(async () => {
    const frameBox = await page.locator('.canvas-frame').boundingBox();
    const canvasBox = await page.getByTestId('ink-canvas').boundingBox();
    if (!frameBox || !canvasBox) {
      return false;
    }
    return Math.abs(canvasBox.width - frameBox.width) < 2 && Math.abs(canvasBox.height - frameBox.height) < 2;
  }).toBeTruthy();
}

function uniqueTitle(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

async function otherOwnerContext(): Promise<APIRequestContext> {
  const token = await fetchOtherOwnerToken();
  return request.newContext({
    baseURL: process.env.BASE_URL,
    ignoreHTTPSErrors: true,
    extraHTTPHeaders: {
      Authorization: `Bearer ${token}`,
    },
  });
}

async function fetchOtherOwnerToken(): Promise<string> {
  const tokenUrl = process.env.OIDC_TOKEN_URL;
  if (!tokenUrl) {
    throw new Error('OIDC_TOKEN_URL is required');
  }
  const ctx = await request.newContext({ ignoreHTTPSErrors: true });
  try {
    const res = await ctx.post(tokenUrl, {
      form: {
        grant_type: 'client_credentials',
        client_id: 'v-note-android-test',
        client_secret: process.env.OIDC_CLIENT_SECRET,
        scope: 'openid profile email v-note:test:access',
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.json();
    return body.access_token;
  } finally {
    await ctx.dispose();
  }
}

function cookieState(token: string) {
  return {
    cookies: [
      {
        name: 'auth',
        value: token,
        domain: new URL(process.env.BASE_URL!).hostname,
        path: '/',
        expires: -1,
        httpOnly: true,
        secure: true,
        sameSite: 'Lax' as const,
      },
    ],
    origins: [],
  };
}
