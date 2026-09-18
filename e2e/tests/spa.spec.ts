import { expect, test } from '@playwright/test';

test('spa loads and renders metadata', async ({ page, request }) => {
  await page.goto('/');

  await expect(page.getByRole('heading', { name: 'v-note' })).toBeVisible();
  // Derived from /api/meta rather than pinned, so a `PROTOCOL_VERSION` bump
  // does not have to be remembered here too — the constant itself is pinned by
  // the Rust and Kotlin fixture tests. What this asserts is that the SPA
  // renders the protocol the server actually reports.
  const meta = await (await request.get('/api/meta')).json();
  await expect(page.getByText(new RegExp(`Protocol\\s+${meta.protocol_version}`))).toBeVisible();
});

// One top bar at every width (#317): the account menu is no longer a
// narrow-screen substitute for a desktop action row, so the apk download and
// sign out live in the same place on a phone and on a desktop.
for (const viewport of [
  { name: 'desktop', width: 1280, height: 800 },
  { name: 'narrow', width: 390, height: 844 },
]) {
  test(`spa ${viewport.name} top bar menu holds sign out and the apk download`, async ({ page }) => {
    await page.setViewportSize({ width: viewport.width, height: viewport.height });
    await page.goto('/', { waitUntil: 'load' });

    const menuButton = page.locator('summary[aria-label="Open main menu"]');
    await expect(menuButton).toBeVisible({ timeout: 15_000 });
    await expect(page.locator('.app-menu-panel')).toBeHidden();

    await menuButton.click();

    await expect(page.getByRole('link', { name: 'Sign out' })).toBeVisible();
    const downloadLink = page.getByRole('link', { name: 'Download Android app (.apk)' });
    const release = (await page.locator('.version-watermark').innerText()).replace(/^v/, '');
    await expect(downloadLink).toBeVisible();
    await expect(downloadLink).toHaveAttribute('download', `v-note-${release}-dev-debug.apk`);
    await expect(downloadLink).toHaveAttribute('href', `/dl/apk?release=${release}`);
  });
}

test('signed out shows the library with no pages and a sign-in control', async ({ browser }) => {
  const context = await browser.newContext({
    baseURL: process.env.BASE_URL,
    ignoreHTTPSErrors: true,
    storageState: { cookies: [], origins: [] },
    extraHTTPHeaders: {},
  });
  const page = await context.newPage();
  try {
    await page.goto('/', { waitUntil: 'load' });

    // The same shell the signed-in library uses — brand, menu, library region —
    // just without any pages in it.
    await expect(page.getByRole('heading', { name: 'v-note' })).toBeVisible();
    await expect(page.getByLabel('Page library')).toBeVisible();
    await expect(page.locator('summary[aria-label="Open main menu"]')).toBeVisible();
    await expect(
      page.locator('.bar-right').getByRole('link', { name: 'Sign in' }),
    ).toBeVisible({ timeout: 15_000 });
    await expect(page.locator('.page-tile')).toHaveCount(0);
  } finally {
    await context.close();
  }
});
