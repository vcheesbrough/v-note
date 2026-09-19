import { expect, test, type Page } from '@playwright/test';

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

// #355: `<details>` opens itself but never closes itself. Each of these is a
// separate route back to a closed menu, so each is asserted on its own.
test.describe('spa top bar menu dismissal', () => {
  test('clicking away from the menu closes it', async ({ page }) => {
    const panel = await openMenu(page);

    // The brand sits in the same bar but outside the disclosure — the nearest
    // thing to "clicked next to the menu" a user would do.
    await page.getByRole('heading', { name: 'v-note' }).click();

    await expect(panel).toBeHidden();
  });

  test('clicking a menu item closes it', async ({ page }) => {
    // The apk item downloads rather than navigating, so nothing but our own
    // handler can close the menu behind it. The transfer itself is not what is
    // under test — `android-apk.spec.ts` covers that — so drop it and keep the
    // click, which is what the handler hangs off.
    await page.route('**/dl/apk*', (route) => route.abort());
    const panel = await openMenu(page);

    await page.getByRole('link', { name: 'Download Android app (.apk)' }).click();

    await expect(panel).toBeHidden();
  });

  test('Escape closes the menu', async ({ page }) => {
    const panel = await openMenu(page);

    await page.keyboard.press('Escape');

    await expect(panel).toBeHidden();
  });

  test('clicking inside the panel leaves it open', async ({ page }) => {
    const panel = await openMenu(page);

    await page.locator('.menu-identity').click();

    await expect(panel).toBeVisible();
  });
});

async function openMenu(page: Page) {
  await page.goto('/', { waitUntil: 'load' });

  const menuButton = page.locator('summary[aria-label="Open main menu"]');
  await expect(menuButton).toBeVisible({ timeout: 15_000 });

  await menuButton.click();

  const panel = page.locator('.app-menu-panel');
  await expect(panel).toBeVisible();
  // These tests act on the signed-in panel, and waiting for an item only it
  // carries also settles the `/api/me` round trip behind it.
  await expect(page.getByRole('link', { name: 'Sign out' })).toBeVisible({ timeout: 15_000 });
  return panel;
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
