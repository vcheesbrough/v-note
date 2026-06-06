import { expect, test } from '@playwright/test';

test('spa loads and renders metadata', async ({ page }) => {
  await page.goto('/');

  await expect(page.getByRole('heading', { name: 'v-note' })).toBeVisible();
  await expect(page.getByText(/Protocol\s+1/)).toBeVisible();
});
