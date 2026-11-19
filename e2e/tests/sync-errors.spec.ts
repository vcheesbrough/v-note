import { expect, test, type APIRequestContext, type Page, type WebSocketRoute } from '../csp-guard';

// #150: the SPA is online-only. When its realtime channel is down or failing it
// withholds what it was showing — the viewer's canvas, the library's grid —
// behind a banner that names the trouble, and shows it again only once a
// reconnect has caught up with the server. No banner means sync is nominal.
//
// Failures are injected at the browser's edge, which is where a user meets
// them: `routeWebSocket` drops a live socket or slips a server error envelope
// into it, and `route` makes the ticket endpoint unreachable, failing or
// refusing the session — every reconnect starts with a ticket, so that is the
// server being down, broken or signing the user out.

const PAGE_SOCKET = /\/api\/pages\/[^/]+\/realtime\?/;
const LIBRARY_SOCKET = /\/api\/realtime\?/;
const TICKET = '**/api/realtime-ticket';

test.describe('SPA disconnect and sync errors', () => {
  test('a dropped page channel hides the canvas until the reconnect has replayed', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('sync-drop'));
    const publisher = await page.context().newPage();
    try {
      await publisher.goto('/', { waitUntil: 'load' });
      await commitStroke(publisher, request, pageId, 'before-drop');

      const sockets = await routeSockets(page, PAGE_SOCKET);
      const tickets = await controlTickets(page);
      await page.goto(`/p/${pageId}`, { waitUntil: 'load' });

      const canvas = page.getByLabel('Read-only ink canvas');
      await expect(canvas).toBeVisible({ timeout: 15_000 });
      await expect.poll(() => strokeCount(page)).toBe(1);
      // Quiet means healthy.
      await expect(page.getByTestId('sync-banner')).toHaveCount(0);

      // The server goes away: the socket drops and reconnects cannot get a ticket.
      tickets.mode = 'unreachable';
      await sockets.last().close({ code: 1011, reason: 'test drop' });

      const banner = page.getByTestId('sync-banner');
      await expect(banner).toBeVisible();
      await expect(banner).toHaveAttribute('data-sync-state', 'disconnected');
      await expect(banner).toContainText('Disconnected from the server');
      // The drop's own close code is the technical detail.
      await expect(banner).toContainText('1011');
      await expect(canvas).toBeHidden();
      await expect(page.getByTestId('canvas-frame')).toHaveClass(/blocked/);

      // Ink lands while the viewer is cut off; it must arrive with the reconnect.
      await commitStroke(publisher, request, pageId, 'during-drop');
      // Still down after a retry or two: still blocked.
      await expect.poll(() => tickets.refused, { timeout: 5_000 }).toBeGreaterThan(0);
      await expect(canvas).toBeHidden();

      tickets.mode = 'pass';
      await expect(banner).toHaveCount(0, { timeout: 10_000 });
      await expect(canvas).toBeVisible();
      await expect.poll(() => strokeCount(page)).toBe(2);
    } finally {
      await publisher.close();
      await deletePage(request, pageId);
    }
  });

  test('a failing server shows a sync error, then recovers', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('sync-5xx'));
    try {
      const canvas = await openLiveViewer(page, pageId);
      const { sockets, tickets } = canvas;

      tickets.mode = 'server-error';
      await sockets.last().close({ code: 1011, reason: 'test drop' });

      const banner = page.getByTestId('sync-banner');
      // Reachable but failing is told apart from unreachable.
      await expect(banner).toHaveAttribute('data-sync-state', 'sync-error', { timeout: 10_000 });
      await expect(banner).toContainText('HTTP 503');
      await expect(canvas.locator).toBeHidden();

      tickets.mode = 'pass';
      await expect(banner).toHaveCount(0, { timeout: 10_000 });
      await expect(canvas.locator).toBeVisible();
    } finally {
      await deletePage(request, pageId);
    }
  });

  test('a server error on the page channel blocks the viewer and resubscribes', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('sync-envelope'));
    try {
      const sockets = await routeSockets(page, PAGE_SOCKET);
      await page.goto(`/p/${pageId}`, { waitUntil: 'load' });
      const canvas = page.getByLabel('Read-only ink canvas');
      await expect(canvas).toBeVisible({ timeout: 15_000 });
      const before = sockets.count();

      // What the server sends when it cannot load the page's ink.
      sockets.last().send(JSON.stringify({ type: 'error', code: 'replay_failed', message: 'could not load page ink' }));

      const banner = page.getByTestId('sync-banner');
      await expect(banner).toHaveAttribute('data-sync-state', 'sync-error');
      await expect(banner).toContainText('replay_failed');
      await expect(canvas).toBeHidden();
      // The viewer could no longer trust what it showed, so it reconnected.
      await expect.poll(() => sockets.count(), { timeout: 10_000 }).toBeGreaterThan(before);
      await expect(banner).toHaveCount(0, { timeout: 10_000 });
      await expect(canvas).toBeVisible();
    } finally {
      await deletePage(request, pageId);
    }
  });

  test('a refused session stops reconnecting and asks to sign in', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('sync-401'));
    try {
      const canvas = await openLiveViewer(page, pageId);
      const { sockets, tickets } = canvas;

      // The session ends server-side while the page is open.
      tickets.mode = 'unauthorized';
      await sockets.last().close({ code: 1011, reason: 'test drop' });

      const banner = page.getByTestId('sync-banner');
      await expect(banner).toHaveAttribute('data-sync-state', 'session-expired', { timeout: 10_000 });
      await expect(banner).toContainText('Your session has expired');
      await expect(banner.getByRole('link', { name: 'Sign in' })).toHaveAttribute('href', '/auth/login');
      await expect(canvas.locator).toBeHidden();

      // Retrying a refused session every second would only be refused again.
      const refused = tickets.refused;
      const opened = sockets.count();
      await page.waitForTimeout(3_000);
      expect(tickets.refused, 'no ticket requests after the refusal').toBe(refused);
      expect(sockets.count(), 'no reconnect after the refusal').toBe(opened);
    } finally {
      await deletePage(request, pageId);
    }
  });

  test('the library hides its pages while its channel is down', async ({ page, request }) => {
    const title = uniqueTitle('sync-library');
    const pageId = await createPage(request, title);
    try {
      const sockets = await routeSockets(page, LIBRARY_SOCKET);
      const tickets = await controlTickets(page);
      await page.goto('/', { waitUntil: 'load' });
      const tile = page.getByRole('button', { name: `Open ${title}`, exact: true });
      await expect(tile).toBeVisible({ timeout: 15_000 });
      await expect(page.getByTestId('sync-banner')).toHaveCount(0);

      tickets.mode = 'unreachable';
      await sockets.last().close({ code: 1011, reason: 'test drop' });

      const banner = page.getByTestId('sync-banner');
      await expect(banner).toHaveAttribute('data-sync-state', 'disconnected');
      await expect(banner).toContainText('Your library is hidden');
      await expect(tile).toHaveCount(0);

      tickets.mode = 'pass';
      await expect(banner).toHaveCount(0, { timeout: 10_000 });
      await expect(tile).toBeVisible();
    } finally {
      await deletePage(request, pageId);
    }
  });
});

/**
 * Open [pageId]'s viewer with its page socket and the ticket endpoint under
 * the test's control, and wait until it is live and quiet.
 */
async function openLiveViewer(page: Page, pageId: string) {
  const sockets = await routeSockets(page, PAGE_SOCKET);
  const tickets = await controlTickets(page);
  await page.goto(`/p/${pageId}`, { waitUntil: 'load' });
  const locator = page.getByLabel('Read-only ink canvas');
  await expect(locator).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId('sync-banner')).toHaveCount(0);
  return { locator, sockets, tickets };
}

/** Every socket the page opens to [pattern], forwarded to the real server. */
async function routeSockets(page: Page, pattern: RegExp) {
  const routes: WebSocketRoute[] = [];
  await page.routeWebSocket(pattern, (ws) => {
    ws.connectToServer();
    routes.push(ws);
  });
  return {
    count: () => routes.length,
    last: () => {
      const route = routes.at(-1);
      if (!route) throw new Error(`no socket opened to ${pattern}`);
      return route;
    },
  };
}

type TicketMode = 'pass' | 'unreachable' | 'server-error' | 'unauthorized';

/** Decide, per request, what the realtime ticket endpoint does. */
async function controlTickets(page: Page) {
  const control = { mode: 'pass' as TicketMode, refused: 0 };
  await page.route(TICKET, async (route) => {
    switch (control.mode) {
      case 'pass':
        return route.continue();
      case 'unreachable':
        control.refused += 1;
        return route.abort('connectionrefused');
      case 'server-error':
        control.refused += 1;
        return route.fulfill({ status: 503, body: 'unavailable' });
      case 'unauthorized':
        control.refused += 1;
        return route.fulfill({ status: 401, body: 'unauthorized' });
    }
  });
  return control;
}

/** Commit one stroke to [pageId] from [publisher], as an Android editor would. */
async function commitStroke(publisher: Page, request: APIRequestContext, pageId: string, label: string) {
  const ticket = await realtimeTicket(request);
  const echoed = await publisher.evaluate(
    async ({ pageId, ticket, label }) => {
      const url = `${location.origin.replace(/^http/, 'ws')}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`;
      const ws = new WebSocket(url);
      const batchId = `${label}-${crypto.randomUUID()}`;
      const result = await new Promise<boolean>((resolve) => {
        const timer = setTimeout(() => resolve(false), 5_000);
        ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'acquire-lease' })));
        ws.addEventListener('message', (event) => {
          const message = JSON.parse(event.data);
          if (message.type === 'lease-granted') {
            ws.send(
              JSON.stringify({
                type: 'commit-batch',
                client_batch_id: batchId,
                strokes: [
                  {
                    id: `stroke-${crypto.randomUUID()}`,
                    style: {
                      tool_kind: 'solid_round',
                      style_version: 2,
                      parameters: { color: '#006400', width: 4.0, cap_style: 'round', join_style: 'round' },
                    },
                    points: [
                      { x: 0.0, y: 0.0, t: 0 },
                      { x: 40.0, y: 40.0, t: 5 },
                    ],
                  },
                ],
              }),
            );
          }
          if (message.type === 'stroke-batch' && message.client_batch_id === batchId) {
            clearTimeout(timer);
            resolve(true);
          }
        });
      });
      ws.send(JSON.stringify({ type: 'release-lease' }));
      ws.close();
      return result;
    },
    { pageId, ticket, label },
  );
  expect(echoed, `stroke ${label} committed`).toBe(true);
}

async function strokeCount(page: Page): Promise<number> {
  return page.evaluate(() => (window as unknown as { __vNoteInkStrokeCount?: number }).__vNoteInkStrokeCount ?? 0);
}

async function realtimeTicket(ctx: APIRequestContext): Promise<string> {
  const res = await ctx.post('/api/realtime-ticket');
  expect(res.status()).toBe(200);
  return (await res.json()).ticket;
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
