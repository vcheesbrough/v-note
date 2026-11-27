import { expect, test, type APIRequestContext, type Page } from '../csp-guard';

// The recycle bin for pages (#631), against the running stack: deleting moves a
// page to the bin (out of the library on every client, ink and thumbnail kept,
// commits still accepted), restoring brings it all back, and permanent deletion
// — from the bin only — removes it for good, after which a write for it is the
// definitive `page_deleted` (410 over REST). The bin *screen* is Android's
// (PageBinInstrumentedTest); the SPA has none, so here it is driven over REST
// and the SPA is held to hiding binned pages and showing restored ones.

test.describe('page recycle bin', () => {
  test('delete moves a page to the bin and restore brings it back with its ink and thumbnail', async ({
    page,
    request,
  }) => {
    const title = uniqueTitle('bin-restore');
    const pageId = await createPage(request, title);
    await page.goto('/', { waitUntil: 'load' });
    const tile = page.getByRole('button', { name: `Open ${title}`, exact: true });
    await expect(tile).toBeVisible();

    await openSocket(page, request, pageId);
    await sendAndAwait(page, commit('before-bin', 'stroke-before-bin'), (m) => m.type === 'stroke-batch');
    const thumbnail = await awaitThumbnail(request, pageId, 1);
    await expect(page.locator(`img.page-preview[src="${thumbnail}"]`)).toBeVisible();

    // Delete from the SPA: the tile goes, live, and the page is in the bin.
    page.once('dialog', (dialog) => {
      expect(dialog.message()).toContain('bin');
      void dialog.accept();
    });
    await page.getByRole('button', { name: `Delete ${title}`, exact: true }).click();
    await expect(tile).toHaveCount(0, { timeout: 5_000 });
    expect(await listedIds(request, '/api/pages')).not.toContain(pageId);
    const binned = (await (await request.get('/api/bin')).json()).pages.find((p: any) => p.id === pageId);
    expect(binned, 'the page is in the bin').toBeTruthy();
    expect(Date.parse(binned.binned_at)).not.toBeNaN();
    expect(binned.thumbnail.url).toBe(thumbnail);
    expect((await request.get(thumbnail)).status(), 'a binned page keeps its thumbnail').toBe(200);

    // The open session was told, and ink committed now is kept.
    await awaitMessage(page, (m) => m.type === 'page-binned');
    await sendAndAwait(page, commit('while-binned', 'stroke-while-binned'), (m) => m.type === 'stroke-batch');
    expect(await listedIds(request, '/api/bin'), 'still binned after a commit').toContain(pageId);

    // A reload does not bring it back: the library never lists a binned page.
    await page.reload({ waitUntil: 'load' });
    await expect(page.getByLabel('Page library')).toBeVisible();
    await expect(tile).toHaveCount(0);

    const restored = await request.post(`/api/pages/${pageId}/restore`);
    expect(restored.status()).toBe(200);
    expect((await restored.json()).page.binned_at).toBeUndefined();
    await expect(tile).toBeVisible({ timeout: 5_000 });
    expect(await listedIds(request, '/api/bin')).not.toContain(pageId);

    // Back with all its ink, including what was drawn while binned.
    await openSocket(page, request, pageId);
    const replay = await sendAndAwait(page, { type: 'subscribe', from_seq: 0 }, (m) => m.type === 'page-replay');
    expect(replay.batches.flatMap((batch: any) => batch.strokes.map((s: any) => s.id))).toEqual([
      'stroke-before-bin',
      'stroke-while-binned',
    ]);
    await tile.click();
    await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();

    await request.delete(`/api/pages/${pageId}`);
  });

  test('permanent delete removes the page for good and later writes are page_deleted', async ({
    page,
    request,
  }) => {
    const pageId = await createPage(request, uniqueTitle('bin-purge'));
    await page.goto('/', { waitUntil: 'load' });
    await openSocket(page, request, pageId);
    await sendAndAwait(page, commit('doomed-1', 'stroke-doomed'), (m) => m.type === 'stroke-batch');
    const thumbnail = await awaitThumbnail(request, pageId, 1);

    const notInBin = await request.delete(`/api/bin/${pageId}`);
    expect(notInBin.status(), 'only a binned page can be permanently deleted').toBe(409);

    expect((await request.delete(`/api/pages/${pageId}`)).status()).toBe(204);
    expect((await request.delete(`/api/bin/${pageId}`)).status()).toBe(204);

    expect(await listedIds(request, '/api/bin')).not.toContain(pageId);
    expect(await listedIds(request, '/api/pages')).not.toContain(pageId);
    const read = await request.get(`/api/pages/${pageId}`);
    expect(read.status()).toBe(410);
    expect(await read.text()).toBe('page permanently deleted');
    expect((await request.post(`/api/pages/${pageId}/restore`)).status()).toBe(410);
    expect((await request.get(thumbnail)).status(), 'its thumbnail went with it').not.toBe(200);

    // The session still open on it is told, and its next commit is rejected
    // with the distinct class — not commit_failed, which a client retries.
    const told = await awaitMessage(page, (m) => m.type === 'error');
    expect(told.code).toBe('page_deleted');
    const rejected = await sendAndAwait(page, commit('doomed-2', 'stroke-after-purge'), (m) => m.type === 'error' && m.client_mutation_id === 'doomed-2');
    expect(rejected.code).toBe('page_deleted');

    // A fresh page channel is refused outright.
    expect(await upgradeRefused(page, request, pageId)).toBe(true);
  });

  test('empty bin permanently deletes every binned page and leaves the library alone', async ({ request }) => {
    const live = await createPage(request, uniqueTitle('bin-live'));
    const first = await createPage(request, uniqueTitle('bin-empty-1'));
    const second = await createPage(request, uniqueTitle('bin-empty-2'));
    for (const id of [first, second]) {
      expect((await request.delete(`/api/pages/${id}`)).status()).toBe(204);
    }
    expect(await listedIds(request, '/api/bin')).toEqual(expect.arrayContaining([first, second]));

    expect((await request.delete('/api/bin')).status()).toBe(204);

    expect(await listedIds(request, '/api/bin')).toEqual([]);
    expect(await listedIds(request, '/api/pages')).toContain(live);
    for (const id of [first, second]) {
      expect((await request.get(`/api/pages/${id}`)).status()).toBe(410);
    }
    await request.delete(`/api/pages/${live}`);
  });
});

type Message = Record<string, any>;

function commit(batch: string, strokeId: string): Message {
  return {
    type: 'commit-batch',
    client_batch_id: batch,
    strokes: [
      {
        id: strokeId,
        style: {
          tool_kind: 'solid_round',
          style_version: 2,
          parameters: { color: '#006400', width: 20.0, cap_style: 'round', join_style: 'round' },
        },
        points: [
          { x: 40.0, y: 40.0, t: 0 },
          { x: 360.0, y: 220.0, t: 12 },
        ],
      },
    ],
  };
}

/// Opens a page channel in the page's own context (the SPA's ticket path) and
/// keeps it open across REST calls the test makes meanwhile, collecting every
/// message on `window.__binMessages`. A later call replaces the socket.
async function openSocket(page: Page, request: APIRequestContext, pageId: string) {
  const ticket = await realtimeTicket(request);
  const opened = await page.evaluate(
    async ({ pageId, ticket }) => {
      const w = window as any;
      w.__binSocket?.close();
      w.__binMessages = [];
      const ws = new WebSocket(
        `${location.origin.replace(/^http/, 'ws')}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`,
      );
      ws.addEventListener('message', (event) => w.__binMessages.push(JSON.parse(event.data)));
      w.__binSocket = ws;
      return new Promise<boolean>((resolve) => {
        ws.addEventListener('open', () => resolve(true));
        ws.addEventListener('error', () => resolve(false));
      });
    },
    { pageId, ticket },
  );
  expect(opened, 'the page channel opens').toBe(true);
  await awaitMessage(page, (m) => m.type === 'welcome');
}

/// Whether a fresh page channel to `pageId` is refused at the upgrade.
async function upgradeRefused(page: Page, request: APIRequestContext, pageId: string): Promise<boolean> {
  const ticket = await realtimeTicket(request);
  return page.evaluate(
    ({ pageId, ticket }) =>
      new Promise<boolean>((resolve) => {
        const ws = new WebSocket(
          `${location.origin.replace(/^http/, 'ws')}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`,
        );
        ws.addEventListener('open', () => {
          ws.close();
          resolve(false);
        });
        ws.addEventListener('error', () => resolve(true));
      }),
    { pageId, ticket },
  );
}

/// The first collected message `matches` accepts, removed from the queue so a
/// later wait for the same type sees the next one.
async function awaitMessage(page: Page, matches: (message: Message) => boolean): Promise<Message> {
  let found: Message | undefined;
  await expect
    .poll(async () => {
      const messages: Message[] = await page.evaluate(() => (window as any).__binMessages);
      const index = messages.findIndex(matches);
      if (index < 0) return false;
      found = messages[index];
      await page.evaluate((i) => (window as any).__binMessages.splice(i, 1), index);
      return true;
    }, { timeout: 10_000 })
    .toBe(true);
  return found!;
}

async function sendAndAwait(page: Page, message: Message, matches: (message: Message) => boolean): Promise<Message> {
  await page.evaluate((text) => (window as any).__binSocket.send(text), JSON.stringify(message));
  return awaitMessage(page, matches);
}

async function awaitThumbnail(request: APIRequestContext, pageId: string, sourceSeq: number): Promise<string> {
  let url = '';
  await expect
    .poll(async () => {
      const summary = (await (await request.get(`/api/pages/${pageId}`)).json()).page;
      url = summary.thumbnail?.url ?? '';
      return summary.thumbnail?.status === 'available' && summary.thumbnail.source_seq === sourceSeq;
    }, { timeout: 15_000 })
    .toBe(true);
  return url;
}

async function listedIds(request: APIRequestContext, uri: string): Promise<string[]> {
  const response = await request.get(uri);
  expect(response.status()).toBe(200);
  return (await response.json()).pages.map((p: { id: string }) => p.id);
}

async function createPage(ctx: APIRequestContext, title: string): Promise<string> {
  const created = await ctx.post('/api/pages', { data: { title } });
  expect(created.status()).toBe(201);
  return (await created.json()).page.id;
}

async function realtimeTicket(ctx: APIRequestContext): Promise<string> {
  const res = await ctx.post('/api/realtime-ticket');
  expect(res.status()).toBe(200);
  return (await res.json()).ticket;
}

function uniqueTitle(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}
