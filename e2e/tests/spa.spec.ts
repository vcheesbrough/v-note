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

  const release = (await page.locator('.version-watermark').innerText()).replace(/^v/, '');
  const downloadLink = page.locator('.apk-link a');
  await expect(downloadLink).toHaveAttribute(
    'download',
    `v-note-${release}-dev-debug.apk`,
  );
  await expect(downloadLink).toHaveAttribute('href', `/dl/apk?release=${release}`);
});

test('spa narrow header uses an account menu with apk download', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/', { waitUntil: 'load' });

  const menuButton = page.locator('summary[aria-label="Open account menu"]');
  await expect(menuButton).toBeVisible({ timeout: 15_000 });
  await expect(page.locator('.desktop-session-actions')).toBeHidden();
  await expect(page.locator('.apk-link')).toBeHidden();

  await menuButton.click();

  await expect(page.getByRole('link', { name: 'Sign out' })).toBeVisible();
  const downloadLink = page.getByRole('link', { name: 'Download Android app (.apk)' });
  const release = (await page.locator('.version-watermark').innerText()).replace(/^v/, '');
  await expect(downloadLink).toBeVisible();
  await expect(downloadLink).toHaveAttribute('download', `v-note-${release}-dev-debug.apk`);
  await expect(downloadLink).toHaveAttribute('href', `/dl/apk?release=${release}`);
});
