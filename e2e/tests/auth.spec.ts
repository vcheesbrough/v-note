import { test, expect, request } from '@playwright/test';

test.describe('auth — happy path', () => {
  test('GET /api/me returns the test service account identity', async ({ request }) => {
    const res = await request.get('/api/me');
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body).toHaveProperty('sub');
    expect(body.sub.length).toBeGreaterThan(0);
  });

  test('SPA loads for an authenticated session', async ({ page }) => {
    await page.goto('/', { waitUntil: 'load' });
    await expect(page.getByText('Sign out')).toBeVisible({ timeout: 15_000 });
    await expect(page).not.toHaveURL(/\/auth\/login/);
  });
});

test.describe('auth — rejection', () => {
  const unauthOptions = {
    baseURL: process.env.BASE_URL,
    ignoreHTTPSErrors: true,
    storageState: { cookies: [], origins: [] },
    extraHTTPHeaders: {},
  };

  test('GET /api/me without a token returns 401', async () => {
    const ctx = await request.newContext(unauthOptions);
    const res = await ctx.get('/api/me');
    expect(res.status()).toBe(401);
    await ctx.dispose();
  });

  test('GET /api/me with a malformed bearer token returns 401', async () => {
    const ctx = await request.newContext({
      ...unauthOptions,
      extraHTTPHeaders: {
        Authorization: 'Bearer not.a.real.jwt',
      },
    });
    const res = await ctx.get('/api/me');
    expect(res.status()).toBe(401);
    await ctx.dispose();
  });

  test('GET /api/me with android-shaped bearer token returns 200', async () => {
    const tokenUrl = process.env.OIDC_TOKEN_URL;
    test.skip(!tokenUrl, 'OIDC_TOKEN_URL required for android bearer test');

    const ctx = await request.newContext({ ignoreHTTPSErrors: true });
    const tokenRes = await ctx.post(tokenUrl!, {
      form: {
        grant_type: 'client_credentials',
        client_id: 'v-note-android-test',
        client_secret: process.env.OIDC_CLIENT_SECRET,
        scope: 'openid profile email v-note:test:access',
      },
    });
    expect(tokenRes.ok()).toBeTruthy();
    const tokenBody = await tokenRes.json();

    const apiCtx = await request.newContext({
      ...unauthOptions,
      extraHTTPHeaders: {
        Authorization: `Bearer ${tokenBody.access_token}`,
      },
    });
    const res = await apiCtx.get('/api/me');
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body.sub).toBe('v-note-android-test-user');
    await apiCtx.dispose();
    await ctx.dispose();
  });

  test('GET /api/me with wrong scope returns 403', async () => {
    const tokenUrl = process.env.OIDC_TOKEN_URL;
    test.skip(!tokenUrl, 'OIDC_TOKEN_URL required for scope rejection test');

    const ctx = await request.newContext({ ignoreHTTPSErrors: true });
    const tokenRes = await ctx.post(tokenUrl!, {
      form: {
        grant_type: 'client_credentials',
        client_id: 'v-note-test-wrong',
        client_secret: process.env.OIDC_CLIENT_SECRET,
        scope: 'openid profile email v-note:wrong:access',
      },
    });
    expect(tokenRes.ok()).toBeTruthy();
    const tokenBody = await tokenRes.json();

    const apiCtx = await request.newContext({
      ...unauthOptions,
      extraHTTPHeaders: {
        Authorization: `Bearer ${tokenBody.access_token}`,
      },
    });
    const res = await apiCtx.get('/api/me');
    expect(res.status()).toBe(403);
    await apiCtx.dispose();
    await ctx.dispose();
  });
});

test.describe('auth — public routes', () => {
  const unauthOptions = {
    baseURL: process.env.BASE_URL,
    ignoreHTTPSErrors: true,
    storageState: { cookies: [], origins: [] },
    extraHTTPHeaders: {},
  };

  test('GET /health is reachable without a token', async () => {
    const ctx = await request.newContext(unauthOptions);
    const res = await ctx.get('/health');
    expect(res.status()).toBe(200);
    await ctx.dispose();
  });

  test('GET /api/meta is public', async () => {
    const ctx = await request.newContext(unauthOptions);
    const res = await ctx.get('/api/meta');
    expect(res.status()).toBe(200);
    await ctx.dispose();
  });

  test('GET /auth/login redirects to the IdP', async () => {
    const ctx = await request.newContext({
      ...unauthOptions,
      maxRedirects: 0,
    });
    const res = await ctx.get('/auth/login');
    expect([301, 302, 303, 307]).toContain(res.status());
    const location = res.headers()['location'] ?? '';
    expect(location).toContain('mock-oidc');
    expect(location).toContain('client_id=');
    expect(location).toContain('state=');
    await ctx.dispose();
  });
});
