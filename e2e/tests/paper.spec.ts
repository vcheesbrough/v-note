import { expect, request, test, type APIRequestContext, type Page } from '@playwright/test';

// Page paper (rule lines) e2e. Covers the create-with-paper round trip through
// every read path, the lease gate, thumbnail regeneration with old-URL
// immutability preserved, the inkless-page rule (library re-sorts, no revision
// bump, no paper-only thumbnail), the same-value no-op, and the graded density
// cull in the SPA renderer.

test.beforeEach(async ({ page }) => {
  await page.goto('/', { waitUntil: 'load' });
});

test.describe('page paper', () => {
  test('create-with-paper is echoed through every read path and unknown values are rejected', async ({
    page,
    request,
  }) => {
    const title = uniqueTitle('paper-create');
    const pageId = await createPage(request, title, 'ruled-margin-narrow');

    // 1. The create response itself.
    // 2. GET /api/pages/{id}
    const single = await request.get(`/api/pages/${pageId}`);
    expect(single.status()).toBe(200);
    expect((await single.json()).page.paper).toBe('ruled-margin-narrow');

    // 3. GET /api/pages (the library listing)
    const listed = (await (await request.get('/api/pages')).json()).pages.find(
      (item: any) => item.id === pageId,
    );
    expect(listed.paper).toBe('ruled-margin-narrow');

    // 4. The page channel's Welcome, which is what makes a reconnect
    //    self-sufficient.
    const ticket = await realtimeTicket(request);
    const snapshot = await driveSocket(page, { pageId, ticket, actions: [], settleMs: 300 });
    const welcome = snapshot.messages.find((message) => message.type === 'welcome');
    expect(welcome, 'welcome received').toBeTruthy();
    expect(welcome.paper).toBe('ruled-margin-narrow');

    // An unrecognised value is rejected outright, never stored as a blank page.
    const bad = await request.post('/api/pages', {
      data: { title: uniqueTitle('paper-bad'), paper: 'ruled-margin-huge' },
    });
    expect(bad.status()).toBeGreaterThanOrEqual(400);
    expect(bad.status()).toBeLessThan(500);

    // Absent paper still means "none" — pre-v5 clients keep working.
    const legacyId = await createPage(request, uniqueTitle('paper-legacy'));
    const legacy = await request.get(`/api/pages/${legacyId}`);
    expect((await legacy.json()).page.paper).toBe('none');
  });

  test('set-paper requires the edit lease', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('paper-lease'));

    // `set-paper` acquires-or-renews like the ink commits do, so a lone session
    // is granted implicitly. Hold the lease from a second session to see the
    // refusal.
    const holderTicket = await realtimeTicket(request);
    const secondTicket = await realtimeTicket(request);
    const result = await driveTwoSockets(page, {
      pageId,
      holderTicket,
      secondTicket,
      holderActions: [{ delayMs: 50, message: { type: 'acquire-lease' } }],
      secondActions: [
        {
          delayMs: 400,
          message: { type: 'set-paper', client_mutation_id: 'paper-denied', paper: 'squared-large' },
        },
      ],
      settleMs: 600,
    });
    expect(
      result.second.some((message) => message.type === 'lease-denied'),
      'the non-holder is refused',
    ).toBeTruthy();
    expect(
      result.second.some((message) => message.type === 'paper-changed'),
      'no paper change is broadcast',
    ).toBeFalsy();

    // …and the page is unchanged.
    const after = await request.get(`/api/pages/${pageId}`);
    expect((await after.json()).page.paper).toBe('none');
  });

  test('a paper change on an inked page mints a new thumbnail and leaves the old URL immutable', async ({
    page,
    request,
  }) => {
    const title = uniqueTitle('paper-thumb');
    const pageId = await createPage(request, title);

    const ticket = await realtimeTicket(request);
    await driveSocket(page, {
      pageId,
      ticket,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 100,
          message: {
            type: 'commit-batch',
            client_batch_id: 'paper-thumb-1',
            strokes: sampleViewerStrokes(),
          },
        },
      ],
      settleMs: 500,
    });

    await expect
      .poll(async () => (await summaryOf(request, pageId))?.thumbnail?.status, { timeout: 15_000 })
      .toBe('available');
    const before = await summaryOf(request, pageId);
    const beforeUrl = before.thumbnail.url;
    const beforeBytes = Buffer.from(await (await request.get(beforeUrl)).body());

    const paperTicket = await realtimeTicket(request);
    const changed = await driveSocket(page, {
      pageId,
      ticket: paperTicket,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 100,
          message: {
            type: 'set-paper',
            client_mutation_id: 'paper-thumb-set',
            paper: 'ruled-margin-narrow',
          },
        },
      ],
      settleMs: 600,
    });
    const ack = changed.messages.find((message) => message.type === 'paper-changed');
    expect(ack, 'paper-changed broadcast reaches the sender too').toBeTruthy();
    expect(ack.paper).toBe('ruled-margin-narrow');

    // A new immutable artifact at a new source_seq.
    await expect
      .poll(async () => (await summaryOf(request, pageId))?.thumbnail?.source_seq, {
        timeout: 15_000,
      })
      .toBeGreaterThan(before.thumbnail.source_seq);
    await expect
      .poll(async () => (await summaryOf(request, pageId))?.thumbnail?.status, { timeout: 15_000 })
      .toBe('available');
    const after = await summaryOf(request, pageId);
    expect(after.thumbnail.url).not.toBe(beforeUrl);

    const afterBytes = Buffer.from(await (await request.get(after.thumbnail.url)).body());
    expect(afterBytes.equals(beforeBytes), 'the new preview shows the paper').toBeFalsy();

    // The previously published URL still serves its original bytes: thumbnails
    // are immutable per revision, so a paper change never rewrites history.
    const replayed = await request.get(beforeUrl);
    expect(replayed.status()).toBe(200);
    expect(Buffer.from(await replayed.body()).equals(beforeBytes)).toBeTruthy();
  });

  test('an inkless page re-sorts the library without minting a paper-only thumbnail', async ({
    page,
    request,
  }) => {
    const olderTitle = uniqueTitle('paper-inkless-older');
    const newerTitle = uniqueTitle('paper-inkless-newer');
    const olderId = await createPage(request, olderTitle);
    await createPage(request, newerTitle);

    await page.reload({ waitUntil: 'load' });
    await expect(page.getByRole('button', { name: `Open ${olderTitle}`, exact: true })).toBeVisible();

    const before = await summaryOf(request, olderId);
    expect(before.thumbnail.status).toBe('empty');

    const ticket = await realtimeTicket(request);
    const result = await driveSocket(page, {
      pageId: olderId,
      ticket,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 100,
          message: {
            type: 'set-paper',
            client_mutation_id: 'paper-inkless',
            paper: 'squared-large',
          },
        },
      ],
      settleMs: 600,
    });
    const ack = result.messages.find((message) => message.type === 'paper-changed');
    expect(ack).toBeTruthy();

    // The paper is stored…
    const after = await summaryOf(request, olderId);
    expect(after.paper).toBe('squared-large');
    // …the library re-sorted (updated_at advanced)…
    expect(new Date(after.updated_at).getTime()).toBeGreaterThan(
      new Date(before.updated_at).getTime(),
    );
    // …and no paper-only thumbnail was minted: a never-inked page keeps its
    // placeholder, and ink_revision was not bumped (which would orphan the
    // head artifact that thumbnail retention protects).
    expect(ack.revision).toBe(0);
    await page.waitForTimeout(2_000);
    expect((await summaryOf(request, olderId)).thumbnail.status).toBe('empty');
  });

  test('setting the paper already in force changes nothing', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('paper-noop'), 'ruled-wide');

    const ticket = await realtimeTicket(request);
    await driveSocket(page, {
      pageId,
      ticket,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 100,
          message: {
            type: 'commit-batch',
            client_batch_id: 'paper-noop-ink',
            strokes: sampleViewerStrokes(),
          },
        },
      ],
      settleMs: 500,
    });
    await expect
      .poll(async () => (await summaryOf(request, pageId))?.thumbnail?.status, { timeout: 15_000 })
      .toBe('available');
    const before = await summaryOf(request, pageId);

    const noopTicket = await realtimeTicket(request);
    const result = await driveSocket(page, {
      pageId,
      ticket: noopTicket,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 100,
          // Already 'ruled-wide' — value-idempotent.
          message: { type: 'set-paper', client_mutation_id: 'paper-noop', paper: 'ruled-wide' },
        },
      ],
      settleMs: 600,
    });

    // A direct ack still arrives, so a racing client converges…
    const ack = result.messages.find((message) => message.type === 'paper-changed');
    expect(ack, 'a same-value set is still acknowledged').toBeTruthy();
    expect(ack.paper).toBe('ruled-wide');

    // …but nothing was minted or bumped.
    await page.waitForTimeout(2_000);
    const after = await summaryOf(request, pageId);
    expect(after.thumbnail.source_seq).toBe(before.thumbnail.source_seq);
    expect(after.updated_at).toBe(before.updated_at);
  });

  test('the SPA renders paper behind the ink, subject to the graded density cull', async ({
    page,
    request,
  }) => {
    const title = uniqueTitle('paper-spa');
    const pageId = await createPage(request, title, 'ruled-margin-narrow');
    await page.reload({ waitUntil: 'load' });

    const openPage = page.getByRole('button', { name: `Open ${title}`, exact: true });
    await expect(openPage).toBeVisible();
    await openPage.click();
    await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();

    const ticket = await realtimeTicket(request);
    await driveSocket(page, {
      pageId,
      ticket,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 100,
          message: {
            type: 'commit-batch',
            client_batch_id: 'paper-spa-ink',
            strokes: sampleViewerStrokes(),
          },
        },
      ],
      settleMs: 400,
    });
    await expect(page.getByText(/Live · seq 1|Synced · seq 1/)).toBeVisible({ timeout: 5_000 });

    // At the SPA's default (minimum) zoom the margin — a single line with no
    // pitch to alias against, so never culled — is painted, and the ink is
    // painted on top of it. Both present at once is what "paper behind the ink"
    // means in practice.
    await expect
      .poll(async () => countPaperPixels(page, 'margin'), { timeout: 5_000 })
      .toBeGreaterThan(0);
    expect(await countInkPixels(page)).toBeGreaterThan(5);

    // 48-unit rules, meanwhile, fall under the 4 device-px cull at this zoom —
    // the specified graded behaviour, not a bug.
    expect(await countPaperPixels(page, 'rule')).toBe(0);

    // Zoom in until the rules clear the cull. Zoom is anchored on the cursor,
    // so this also pans the ink and the margin out of view — the rules are the
    // only thing asserted from here on.
    const canvas = page.getByLabel('Read-only ink canvas');
    for (let i = 0; i < 40; i += 1) {
      await canvas.hover();
      await page.mouse.wheel(0, -100);
    }

    await expect
      .poll(async () => countPaperPixels(page, 'rule'), { timeout: 5_000 })
      .toBeGreaterThan(5);
  });

  test('coarse paper draws at the default zoom while fine paper is culled', async ({
    page,
    request,
  }) => {
    const coarseTitle = uniqueTitle('paper-coarse');
    const fineTitle = uniqueTitle('paper-fine');
    await createPage(request, coarseTitle, 'squared-large');
    await createPage(request, fineTitle, 'squared-small');
    await page.reload({ waitUntil: 'load' });

    // squared-large: 64 world units × 0.08 = 5.12 device px — drawn.
    await page.getByRole('button', { name: `Open ${coarseTitle}`, exact: true }).click();
    await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();
    await expect
      .poll(async () => countPaperPixels(page, 'rule'), { timeout: 5_000 })
      .toBeGreaterThan(5);
    await page.getByRole('button', { name: 'Back' }).click();

    // squared-small: 32 × 0.08 = 2.56 device px — culled, not aliased.
    await page.getByRole('button', { name: `Open ${fineTitle}`, exact: true }).click();
    await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();
    await page.waitForTimeout(500);
    expect(await countPaperPixels(page, 'rule')).toBe(0);
  });
});

// ---- helpers --------------------------------------------------------------

type SocketAction = { delayMs?: number; message: Record<string, unknown> };

/**
 * Count paper pixels on the SPA canvas. Rule/grid marks (`#B0C4DE`) blend
 * toward white preserving `b > g > r`; the margin (`#E06C6C`) preserves
 * `r > g` and `r > b`. Both orderings are disjoint from the ink classifier.
 */
async function countPaperPixels(page: Page, kind: 'rule' | 'margin'): Promise<number> {
  return page.evaluate((markKind) => {
    const canvas = document.querySelector<HTMLCanvasElement>('[data-testid="ink-canvas"]');
    if (!canvas) return 0;
    const context = canvas.getContext('2d');
    if (!context) return 0;
    const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
    let count = 0;
    for (let i = 0; i < pixels.length; i += 4) {
      const red = pixels[i];
      const green = pixels[i + 1];
      const blue = pixels[i + 2];
      const alpha = pixels[i + 3];
      if (alpha === 0) continue;
      if (red === 255 && green === 255 && blue === 255) continue;
      const matches =
        markKind === 'rule' ? blue > green && green > red : red > green && red > blue;
      if (matches) count += 1;
    }
    return count;
  }, kind);
}

/** The repo's canonical ink classifier, reused so "ink survived" means the same thing everywhere. */
async function countInkPixels(page: Page): Promise<number> {
  return page.evaluate(() => {
    const canvas = document.querySelector<HTMLCanvasElement>('[data-testid="ink-canvas"]');
    if (!canvas) return 0;
    const context = canvas.getContext('2d');
    if (!context) return 0;
    const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
    let count = 0;
    for (let i = 0; i < pixels.length; i += 4) {
      const red = pixels[i];
      const green = pixels[i + 1];
      const blue = pixels[i + 2];
      const alpha = pixels[i + 3];
      if (alpha > 0 && red < 20 && green > 70 && green < 130 && blue < 20) {
        count += 1;
      }
    }
    return count;
  });
}

async function driveSocket(
  page: Page,
  params: { pageId: string; ticket: string; actions: SocketAction[]; settleMs: number },
): Promise<{ opened: boolean; messages: any[] }> {
  return page.evaluate(async ({ pageId, ticket, actions, settleMs }) => {
    const url = `${location.origin.replace(/^http/, 'ws')}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`;
    const ws = new WebSocket(url);
    const messages: any[] = [];
    ws.addEventListener('message', (event) => {
      try {
        messages.push(JSON.parse(event.data));
      } catch {
        /* ignore non-JSON frames */
      }
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
    return { opened, messages };
  }, params);
}

/** Two concurrent sessions on one page, so the single-editor lease can be exercised. */
async function driveTwoSockets(
  page: Page,
  params: {
    pageId: string;
    holderTicket: string;
    secondTicket: string;
    holderActions: SocketAction[];
    secondActions: SocketAction[];
    settleMs: number;
  },
): Promise<{ holder: any[]; second: any[] }> {
  return page.evaluate(
    async ({ pageId, holderTicket, secondTicket, holderActions, secondActions, settleMs }) => {
      const origin = location.origin.replace(/^http/, 'ws');
      const open = (ticket: string) => {
        const ws = new WebSocket(
          `${origin}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`,
        );
        const messages: any[] = [];
        ws.addEventListener('message', (event) => {
          try {
            messages.push(JSON.parse(event.data));
          } catch {
            /* ignore non-JSON frames */
          }
        });
        const ready = new Promise<boolean>((resolve) => {
          ws.addEventListener('open', () => resolve(true));
          ws.addEventListener('error', () => resolve(false));
          setTimeout(() => resolve(false), 3000);
        });
        return { ws, messages, ready };
      };
      const drive = async (
        socket: { ws: WebSocket; ready: Promise<boolean> },
        actions: SocketAction[],
      ) => {
        if (!(await socket.ready)) return;
        for (const action of actions) {
          await new Promise((resolve) => setTimeout(resolve, action.delayMs ?? 0));
          if (socket.ws.readyState === WebSocket.OPEN) {
            socket.ws.send(JSON.stringify(action.message));
          }
        }
      };

      const holder = open(holderTicket);
      const second = open(secondTicket);
      await Promise.all([drive(holder, holderActions), drive(second, secondActions)]);
      await new Promise((resolve) => setTimeout(resolve, settleMs));
      for (const socket of [holder, second]) {
        try {
          socket.ws.close();
        } catch {
          /* already closed */
        }
      }
      return { holder: holder.messages, second: second.messages };
    },
    params,
  );
}

function sampleViewerStrokes(id = `stroke-${crypto.randomUUID()}`) {
  return [
    {
      id,
      style: {
        tool_kind: 'solid_round',
        style_version: 1,
        parameters: { color: '#006400', width: 20.0, cap_style: 'round', join_style: 'round' },
      },
      points: [
        { x: 40.0, y: 40.0, t: 0 },
        { x: 360.0, y: 220.0, t: 12 },
        { x: 760.0, y: 96.0, t: 24 },
      ],
    },
  ];
}

async function summaryOf(ctx: APIRequestContext, pageId: string): Promise<any> {
  const response = await ctx.get(`/api/pages/${pageId}`);
  expect(response.status()).toBe(200);
  return (await response.json()).page;
}

async function createPage(
  ctx: APIRequestContext,
  title: string,
  paper?: string,
): Promise<string> {
  const data: Record<string, unknown> = { title };
  if (paper !== undefined) {
    data.paper = paper;
  }
  const created = await ctx.post('/api/pages', { data });
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
