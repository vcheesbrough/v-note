import { expect, request, test, type APIRequestContext, type Page } from '@playwright/test';

// Ink (page channel) e2e. Drives the per-page WSS from a real browser context
// using a short-lived realtime ticket (the SPA auth path) — no extra npm deps.
// Covers commit + sequencing, persistence/snapshot, gap-fill, the single-editor
// edit lease, owner isolation, and SPA Canvas2D live replay.

test.beforeEach(async ({ page }) => {
  await page.goto('/', { waitUntil: 'load' });
});

test.describe('ink page channel', () => {
  test('commits a stroke batch and assigns a monotonic sequence', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('ink-commit'));
    const ticket = await realtimeTicket(request);
    const clientBatchId = 'batch_e2e_commit';

    const result = await driveSocket(page, {
      pageId,
      ticket,
      actions: [
        { delayMs: 50, message: { type: 'subscribe', from_seq: 0 } },
        { delayMs: 50, message: { type: 'acquire-lease' } },
        { delayMs: 150, message: { type: 'commit-batch', client_batch_id: clientBatchId, strokes: sampleStrokes() } },
      ],
      settleMs: 900,
    });

    expect(result.opened).toBeTruthy();
    expect(result.messages.find((m) => m.type === 'welcome')).toBeTruthy();
    expect(result.messages.some((m) => m.type === 'lease-granted')).toBeTruthy();

    const echoed = result.messages.find(
      (m) => m.type === 'stroke-batch' && m.client_batch_id === clientBatchId,
    );
    expect(echoed, 'committed batch is echoed back with a seq').toBeTruthy();
    expect(echoed.seq).toBeGreaterThanOrEqual(1);
    expect(echoed.strokes[0].points.length).toBe(2);
  });

  test('persists strokes across reconnect and gap-fills only newer batches', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('ink-persist'));

    const ticket1 = await realtimeTicket(request);
    const commit = await driveSocket(page, {
      pageId,
      ticket: ticket1,
      actions: [
        { delayMs: 50, message: { type: 'subscribe', from_seq: 0 } },
        { delayMs: 50, message: { type: 'acquire-lease' } },
        { delayMs: 150, message: { type: 'commit-batch', client_batch_id: 'b1', strokes: sampleStrokes() } },
        { delayMs: 150, message: { type: 'commit-batch', client_batch_id: 'b2', strokes: sampleStrokes() } },
      ],
      settleMs: 900,
    });
    const committedSeqs = commit.messages.filter((m) => m.type === 'stroke-batch').map((m) => m.seq);
    expect(committedSeqs).toContain(1);
    expect(committedSeqs).toContain(2);

    // Fresh connection from seq 0 → full snapshot replay then synced.
    const ticket2 = await realtimeTicket(request);
    const snapshot = await driveSocket(page, {
      pageId,
      ticket: ticket2,
      actions: [{ delayMs: 50, message: { type: 'subscribe', from_seq: 0 } }],
      settleMs: 700,
    });
    const snapshotBatches = snapshot.messages.filter((m) => m.type === 'stroke-batch');
    expect(snapshotBatches.map((m) => m.seq).sort()).toEqual([1, 2]);
    const snapshotSynced = snapshot.messages.find((m) => m.type === 'synced');
    expect(snapshotSynced?.last_seq).toBe(2);

    // Reconnect from seq 1 → gap fill returns only seq 2.
    const ticket3 = await realtimeTicket(request);
    const gap = await driveSocket(page, {
      pageId,
      ticket: ticket3,
      actions: [{ delayMs: 50, message: { type: 'subscribe', from_seq: 1 } }],
      settleMs: 700,
    });
    const gapBatches = gap.messages.filter((m) => m.type === 'stroke-batch');
    expect(gapBatches.map((m) => m.seq)).toEqual([2]);
  });

  test('SPA viewer renders live stroke batches without refresh', async ({ page, request }) => {
    const title = uniqueTitle('ink-spa-live');
    const pageId = await createPage(request, title);

    const openPage = page.getByRole('button', { name: `Open ${title}`, exact: true });
    await openPage.waitFor({ state: 'visible', timeout: 5_000 }).catch(async () => {
      await page.reload({ waitUntil: 'load' });
      await expect(openPage).toBeVisible({ timeout: 5_000 });
    });
    await openPage.click();
    await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();
    await expect(page.getByText(/Synced · seq 0|Connected · seq 0|Live · seq 0/)).toBeVisible({ timeout: 5_000 });

    const ticket = await realtimeTicket(request);
    const result = await driveSocket(page, {
      pageId,
      ticket,
      actions: [
        { delayMs: 50, message: { type: 'subscribe', from_seq: 0 } },
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 150,
          message: {
            type: 'commit-batch',
            client_batch_id: 'batch_spa_live',
            strokes: sampleViewerStrokes(),
          },
        },
      ],
      settleMs: 250,
    });
    expect(result.messages.some((m) => m.type === 'stroke-batch' && m.client_batch_id === 'batch_spa_live')).toBeTruthy();
    expect(result.commitSentAt).not.toBeNull();

    await page.waitForFunction(() => {
      const canvas = document.querySelector<HTMLCanvasElement>('[data-testid="ink-canvas"]');
      if (!canvas) return false;
      const context = canvas.getContext('2d');
      if (!context) return false;
      const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
      let greenPixels = 0;
      for (let i = 0; i < pixels.length; i += 4) {
        const red = pixels[i];
        const green = pixels[i + 1];
        const blue = pixels[i + 2];
        const alpha = pixels[i + 3];
        if (alpha > 0 && red < 20 && green > 70 && green < 130 && blue < 20) {
          greenPixels += 1;
        }
      }
      return greenPixels > 20;
    }, null, { timeout: 1_000 });
    const timing = await page.evaluate(() => ({
      appliedAt: (window as any).__vNoteLastInkAppliedAt as number | undefined,
      seq: (window as any).__vNoteLastInkSeq as number | undefined,
    }));
    expect(timing.seq).toBe(1);
    expect(timing.appliedAt).toBeGreaterThan(result.commitSentAt!);
    expect(timing.appliedAt! - result.commitSentAt!).toBeLessThan(1_000);
    await expect(page.getByText(/Live · seq 1|Synced · seq 1/)).toBeVisible({ timeout: 1_000 });
  });

  test('blocks a second session from inking while the lease is held', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('ink-lease'));
    const ticketA = await realtimeTicket(request);
    const ticketB = await realtimeTicket(request);

    // Two concurrent page sockets for the same owner: A holds the lease, B is denied.
    const result = await page.evaluate(
      async ({ pageId, ticketA, ticketB }) => {
        const wsUrl = (ticket: string) =>
          `${location.origin.replace(/^http/, 'ws')}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`;
        const wait = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
        const open = (ticket: string) =>
          new Promise<{ ws: WebSocket | null; messages: any[] }>((resolve) => {
            const ws = new WebSocket(wsUrl(ticket));
            const messages: any[] = [];
            ws.addEventListener('message', (event) => messages.push(JSON.parse(event.data)));
            ws.addEventListener('open', () => resolve({ ws, messages }));
            ws.addEventListener('error', () => resolve({ ws: null, messages }));
            setTimeout(() => resolve({ ws: null, messages }), 3000);
          });

        const a = await open(ticketA);
        if (!a.ws) return { error: 'A failed to open' };
        a.ws.send(JSON.stringify({ type: 'subscribe', from_seq: 0 }));
        a.ws.send(JSON.stringify({ type: 'acquire-lease' }));
        await wait(300);

        const b = await open(ticketB);
        if (!b.ws) {
          a.ws.close();
          return { error: 'B failed to open' };
        }
        b.ws.send(JSON.stringify({ type: 'subscribe', from_seq: 0 }));
        b.ws.send(JSON.stringify({ type: 'acquire-lease' }));
        await wait(300);

        const bWelcome = b.messages.find((m) => m.type === 'welcome');
        const result = {
          aGranted: a.messages.some((m) => m.type === 'lease-granted'),
          bDenied: b.messages.some((m) => m.type === 'lease-denied'),
          bWelcomeHolderPresent: Boolean(bWelcome && bWelcome.lease_holder),
        };
        a.ws.close();
        b.ws.close();
        return result;
      },
      { pageId, ticketA, ticketB },
    );

    expect(result.error).toBeUndefined();
    expect(result.aGranted).toBeTruthy();
    expect(result.bDenied).toBeTruthy();
    expect(result.bWelcomeHolderPresent).toBeTruthy();
  });

  test('another owner cannot open the page channel', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('ink-isolation'));

    const otherCtx = await otherOwnerContext();
    const ticketRes = await otherCtx.post('/api/realtime-ticket');
    expect(ticketRes.status()).toBe(200);
    const otherTicket = (await ticketRes.json()).ticket;
    await otherCtx.dispose();

    const result = await driveSocket(page, {
      pageId,
      ticket: otherTicket,
      actions: [{ delayMs: 50, message: { type: 'subscribe', from_seq: 0 } }],
      settleMs: 600,
    });

    // The upgrade is rejected (403) before any welcome — no page metadata leaks.
    expect(result.opened).toBeFalsy();
    expect(result.messages.find((m) => m.type === 'welcome')).toBeFalsy();
  });
});

type SocketAction = { delayMs?: number; message: Record<string, unknown> };

async function driveSocket(
  page: Page,
  params: { pageId: string; ticket: string; actions: SocketAction[]; settleMs: number },
): Promise<{ opened: boolean; closed: boolean; closeCode: number | null; commitSentAt: number | null; messages: any[] }> {
  return page.evaluate(async ({ pageId, ticket, actions, settleMs }) => {
    const url = `${location.origin.replace(/^http/, 'ws')}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`;
    const ws = new WebSocket(url);
    const messages: any[] = [];
    let commitSentAt: number | null = null;
    let closed = false;
    let closeCode: number | null = null;
    ws.addEventListener('message', (event) => {
      try {
        messages.push(JSON.parse(event.data));
      } catch {
        /* ignore non-JSON frames */
      }
    });
    ws.addEventListener('close', (event) => {
      closed = true;
      closeCode = event.code;
    });

    const opened = await new Promise<boolean>((resolve) => {
      ws.addEventListener('open', () => resolve(true));
      ws.addEventListener('error', () => resolve(false));
      setTimeout(() => resolve(false), 3000);
    });

    if (opened) {
      for (const action of actions) {
        await new Promise((resolve) => setTimeout(resolve, action.delayMs ?? 0));
        if (ws.readyState === WebSocket.OPEN) {
          if (action.message.type === 'commit-batch') {
            commitSentAt = performance.now();
          }
          ws.send(JSON.stringify(action.message));
        }
      }
    }
    await new Promise((resolve) => setTimeout(resolve, settleMs));
    try {
      ws.close();
    } catch {
      /* already closed */
    }
    return { opened, closed, closeCode, commitSentAt, messages };
  }, params);
}

function sampleStrokes() {
  return [
    {
      tool: 'pen',
      color: '#006400',
      width: 2.0,
      points: [
        { x: 5.0, y: 6.0, t: 0 },
        { x: 7.0, y: 8.0, t: 12 },
      ],
    },
  ];
}

function sampleViewerStrokes() {
  return [
    {
      tool: 'pen',
      color: '#006400',
      width: 2.0,
      points: [
        { x: 40.0, y: 40.0, t: 0 },
        { x: 90.0, y: 72.0, t: 12 },
        { x: 150.0, y: 54.0, t: 24 },
      ],
    },
  ];
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

async function otherOwnerContext(): Promise<APIRequestContext> {
  const tokenUrl = process.env.OIDC_TOKEN_URL;
  if (!tokenUrl) {
    throw new Error('OIDC_TOKEN_URL is required');
  }
  const ctx = await request.newContext({ ignoreHTTPSErrors: true });
  let token: string;
  try {
    const res = await ctx.post(tokenUrl, {
      form: {
        grant_type: 'client_credentials',
        client_id: 'v-note-android-test',
        client_secret: process.env.OIDC_CLIENT_SECRET ?? '',
        scope: 'openid profile email v-note:test:access',
      },
    });
    expect(res.ok()).toBeTruthy();
    token = (await res.json()).access_token;
  } finally {
    await ctx.dispose();
  }
  return request.newContext({
    baseURL: process.env.BASE_URL,
    ignoreHTTPSErrors: true,
    extraHTTPHeaders: { Authorization: `Bearer ${token}` },
  });
}
