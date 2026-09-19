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
    // Sign out lives in the top bar's menu (#317), so the menu has to be opened
    // before the link is on screen.
    const menuButton = page.locator('summary[aria-label="Open main menu"]');
    await expect(menuButton).toBeVisible({ timeout: 15_000 });
    await menuButton.click();
    await expect(page.getByRole('link', { name: 'Sign out' })).toBeVisible({ timeout: 15_000 });
    await expect(page).not.toHaveURL(/\/auth\/login/);
  });
});

// #274 turned the SPA's code exchange from a confidential (client-secret) swap
// into a public Authorization Code + PKCE swap. Nothing previously drove the real
// `/auth/callback`, so the exchange itself was uncovered — this walks the whole
// redirect chain in a real browser against the mock IdP.
test.describe('auth — full login flow (PKCE)', () => {
  test('a signed-out browser completes the code exchange and ends up signed in', async ({
    browser,
  }) => {
    // A clean context: no seeded cookie, no bearer header — the flow has to
    // establish the session entirely by itself.
    const context = await browser.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: {},
    });
    try {
      const page = await context.newPage();
      await page.goto('/auth/login', { waitUntil: 'load' });

      // The chain ends back on the app root, not on the IdP or an error page.
      // Compare host+path: the browser drops the default :443 from BASE_URL.
      const appHost = new URL(process.env.BASE_URL!).hostname;
      await expect
        .poll(() => {
          const url = new URL(page.url());
          return `${url.hostname}${url.pathname}`;
        })
        .toBe(`${appHost}/`);

      const cookies = await context.cookies();
      const session = cookies.find((c) => c.name === 'auth');
      expect(session, 'the exchange must set the session cookie').toBeTruthy();
      expect(session!.httpOnly).toBe(true);
      expect(session!.value.split('.')).toHaveLength(3);

      // The verifier is single-use: the callback clears it.
      const verifier = cookies.find((c) => c.name === 'auth_pkce' && c.value !== '');
      expect(verifier, 'the PKCE verifier must not outlive the exchange').toBeFalsy();

      // The session the exchange produced is actually usable.
      const me = await context.request.get('/api/me');
      expect(me.status()).toBe(200);
      expect(await me.json()).toHaveProperty('sub');
    } finally {
      await context.close();
    }
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

  // #274: the Android app now authenticates against the same public client as
  // the SPA, so a second identity is just a different `sub` under one audience.
  test('GET /api/me with a second identity on the unified client returns 200', async () => {
    const tokenUrl = process.env.OIDC_TOKEN_URL;
    test.skip(!tokenUrl, 'OIDC_TOKEN_URL required for unified client test');

    const ctx = await request.newContext({ ignoreHTTPSErrors: true });
    const tokenRes = await ctx.post(tokenUrl!, {
      form: {
        grant_type: 'client_credentials',
        client_id: 'v-note-other-test',
        client_secret: process.env.MOCK_OIDC_CLIENT_SECRET,
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
    expect(body.sub).toBe('v-note-other-test-user');
    await apiCtx.dispose();
    await ctx.dispose();
  });

  // The retired standalone Android provider minted its own `aud`/`iss`. The
  // server accepted that via `oidc/android/*` before #274 and must not now.
  test('GET /api/me with a legacy Android-audience token returns 401', async () => {
    const tokenUrl = process.env.OIDC_TOKEN_URL;
    test.skip(!tokenUrl, 'OIDC_TOKEN_URL required for legacy audience test');

    const ctx = await request.newContext({ ignoreHTTPSErrors: true });
    const tokenRes = await ctx.post(tokenUrl!, {
      form: {
        grant_type: 'client_credentials',
        client_id: 'v-note-legacy-android-test',
        client_secret: process.env.MOCK_OIDC_CLIENT_SECRET,
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
    expect(res.status()).toBe(401);
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
        client_secret: process.env.MOCK_OIDC_CLIENT_SECRET,
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
    // #274: public client — the authorize request is bound by PKCE, not a secret.
    expect(location).toContain('code_challenge=');
    expect(location).toContain('code_challenge_method=S256');
    expect(location).not.toContain('client_secret');

    // The verifier is held in an HttpOnly, /auth-scoped cookie for the round trip.
    const setCookie = res.headersArray().filter((h) => h.name.toLowerCase() === 'set-cookie');
    const pkceCookie = setCookie.find((h) => h.value.startsWith('auth_pkce='));
    expect(pkceCookie, 'login must set the PKCE verifier cookie').toBeTruthy();
    expect(pkceCookie!.value).toContain('HttpOnly');
    expect(pkceCookie!.value).toContain('Path=/auth');
    await ctx.dispose();
  });
});
