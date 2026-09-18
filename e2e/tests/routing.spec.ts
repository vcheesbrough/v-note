import { expect, test, type APIRequestContext, type Page } from '@playwright/test';

// #189: the SPA used to keep the open page in a signal and nowhere else, so the
// browser's Back button had no entry of ours to pop and walked out of v-note,
// and no page had a URL to link, bookmark or reload. `/p/{page_id}` is now the
// viewer's address; these tests drive the browser's own controls, not just the
// in-app ones, because those are what the bug was about.

test.describe('SPA routing', () => {
  test('opening a page deep links it, and Back and Forward walk the app', async ({
    page,
    request,
  }) => {
    const title = uniqueTitle('route-back');
    const pageId = await createPage(request, title);
    try {
      await page.goto('/', { waitUntil: 'load' });
      await openTile(page, title);

      await expect(page).toHaveURL(new RegExp(`/p/${pageId}$`));
      await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();

      // The bug: this used to leave the site.
      await page.goBack();
      await expect(page.getByLabel('Page library')).toBeVisible();
      await expect(page).toHaveURL(/\/$/);
      await expect(page.getByLabel('Read-only ink canvas')).toHaveCount(0);

      await page.goForward();
      await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();
      await expect(page).toHaveURL(new RegExp(`/p/${pageId}$`));
    } finally {
      await deletePage(request, pageId);
    }
  });

  test('the in-app back arrow returns to the library and to its URL', async ({ page, request }) => {
    const title = uniqueTitle('route-arrow');
    const pageId = await createPage(request, title);
    try {
      await page.goto('/', { waitUntil: 'load' });
      await openTile(page, title);
      await expect(page).toHaveURL(new RegExp(`/p/${pageId}$`));

      await page.getByRole('button', { name: 'Back to library', exact: true }).click();

      await expect(page.getByLabel('Page library')).toBeVisible();
      await expect(page).toHaveURL(/\/$/);

      // The arrow pops the entry the tile pushed rather than pushing another,
      // so one Back from the library is not a no-op that lands on the library
      // again.
      await page.goForward();
      await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();
    } finally {
      await deletePage(request, pageId);
    }
  });

  test('a page URL opens its viewer cold and survives a reload', async ({ page, request }) => {
    const title = uniqueTitle('route-deep');
    const pageId = await createPage(request, title);
    try {
      await page.goto(`/p/${pageId}`, { waitUntil: 'load' });
      await expect(page.getByLabel('Read-only ink canvas')).toBeVisible({ timeout: 15_000 });

      await page.reload({ waitUntil: 'load' });
      await expect(page.getByLabel('Read-only ink canvas')).toBeVisible({ timeout: 15_000 });
      await expect(page).toHaveURL(new RegExp(`/p/${pageId}$`));

      // Nothing was pushed, so the back arrow rewrites the URL instead of
      // stepping out of v-note.
      await page.getByRole('button', { name: 'Back to library', exact: true }).click();
      await expect(page.getByLabel('Page library')).toBeVisible();
      await expect(page).toHaveURL(/\/$/);
    } finally {
      await deletePage(request, pageId);
    }
  });

  test('a page id that is not in the library lands on the library with a message', async ({
    page,
  }) => {
    await page.goto('/p/page_00000000000000000000000000000000', { waitUntil: 'load' });

    await expect(page.getByRole('alert')).toContainText('That page is not in your library.', {
      timeout: 15_000,
    });
    await expect(page.getByLabel('Page library')).toBeVisible();
    await expect(page).toHaveURL(/\/$/);
  });

  test('deleting the open page closes the viewer and drops it from the URL', async ({
    page,
    request,
  }) => {
    const title = uniqueTitle('route-deleted');
    const pageId = await createPage(request, title);
    let deleted = false;
    try {
      await page.goto('/', { waitUntil: 'load' });
      await openTile(page, title);
      await expect(page).toHaveURL(new RegExp(`/p/${pageId}$`));

      // Deleted from another client: the library socket closes the viewer, and
      // the address bar must stop naming a page that no longer exists.
      await deletePage(request, pageId);
      deleted = true;

      await expect(page.getByLabel('Read-only ink canvas')).toHaveCount(0, { timeout: 15_000 });
      await expect(page.getByLabel('Page library')).toBeVisible();
      await expect(page).toHaveURL(/\/$/);
    } finally {
      if (!deleted) {
        await deletePage(request, pageId);
      }
    }
  });
});

async function openTile(page: Page, title: string) {
  const tile = page.getByRole('button', { name: `Open ${title}`, exact: true });
  await tile.waitFor({ state: 'visible', timeout: 5_000 }).catch(async () => {
    await page.reload({ waitUntil: 'load' });
    await expect(tile).toBeVisible({ timeout: 5_000 });
  });
  await tile.click();
}

async function createPage(ctx: APIRequestContext, title: string): Promise<string> {
  const created = await ctx.post('/api/pages', { data: { title } });
  expect(created.status()).toBe(201);
  return (await created.json()).page.id;
}

async function deletePage(ctx: APIRequestContext, pageId: string) {
  await ctx.delete(`/api/pages/${pageId}`);
}

function uniqueTitle(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}
