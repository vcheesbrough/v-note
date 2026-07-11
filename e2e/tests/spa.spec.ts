import { expect, test } from '@playwright/test';

test('spa loads and renders metadata', async ({ page }) => {
  await page.goto('/');

  await expect(page.getByRole('heading', { name: 'v-note' })).toBeVisible();
  await expect(page.getByText(/Protocol\s+1/)).toBeVisible();

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
