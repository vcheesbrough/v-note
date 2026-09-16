import { expect, test, type APIRequestContext, type Page } from '@playwright/test';

// SPA viewer render performance on a dense page (#271). The viewer used to
// repaint the whole page for every pointer, wheel and replayed batch event,
// cloning every point each time, and stroked pressure ink one segment at a time.
// These tests pin the structural fixes — at most one paint per animation frame,
// and ink still correct after panning and zooming — and record frame timings as
// annotations so a regression is visible in the report without making CI flaky
// on absolute numbers.

const BATCHES = 40;
const STROKES_PER_BATCH = 30;
const POINTS_PER_STROKE = 40;

test.beforeEach(async ({ page }) => {
  await page.goto('/', { waitUntil: 'load' });
});

test.describe('SPA render performance', () => {
  test('a dense page paints at most once per frame while panning and zooming', async ({ page, request }) => {
    test.setTimeout(120_000);
    const title = `dense-render-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    const pageId = await createPage(request, title);
    await seedDensePage(page, pageId, await realtimeTicket(request));

    const openPage = page.getByRole('button', { name: `Open ${title}`, exact: true });
    await openPage.waitFor({ state: 'visible', timeout: 5_000 }).catch(async () => {
      await page.reload({ waitUntil: 'load' });
      await expect(openPage).toBeVisible({ timeout: 5_000 });
    });
    const openStarted = Date.now();
    await openPage.click();
    await expect(page.getByText(new RegExp(`seq ${BATCHES}$`))).toBeVisible({ timeout: 30_000 });
    await nextFrames(page, 2);
    test.info().annotations.push({ type: 'open-to-replayed-ms', description: String(Date.now() - openStarted) });

    const canvas = page.getByTestId('ink-canvas');
    expect(await inkPixelCount(page)).toBeGreaterThan(0);

    const zoomedOut = await panBenchmark(page);
    test.info().annotations.push({ type: 'pan-frame-ms (zoomed out)', description: JSON.stringify(zoomedOut) });

    // Zoom in about the top-left of the ink, so most strokes leave the screen.
    const box = await canvas.boundingBox();
    expect(box).not.toBeNull();
    await page.evaluate(
      ({ x, y }) => {
        const target = document.querySelector('[data-testid="ink-canvas"]')!;
        for (let notch = 0; notch < 150; notch += 1) {
          target.dispatchEvent(new WheelEvent('wheel', { deltaY: -1, clientX: x, clientY: y, bubbles: true, cancelable: true }));
        }
      },
      { x: box!.x + 120, y: box!.y + 120 },
    );
    await nextFrames(page, 2);
    const zoomedIn = await panBenchmark(page);
    test.info().annotations.push({ type: 'pan-frame-ms (zoomed in)', description: JSON.stringify(zoomedIn) });

    // A burst of pointer events, each in its own task, must not paint more
    // often than the browser shows frames.
    const burst = await page.evaluate(async () => {
      const target = document.querySelector('[data-testid="ink-canvas"]')!;
      const rect = target.getBoundingClientRect();
      const draws = () => ((window as any).__vNoteInkDraws as number | undefined) ?? 0;
      const yieldTask = () =>
        new Promise<void>((resolve) => {
          const channel = new MessageChannel();
          channel.port1.onmessage = () => resolve();
          channel.port2.postMessage(null);
        });
      await new Promise((resolve) => requestAnimationFrame(resolve));
      const drawsBefore = draws();
      let frames = 0;
      let counting = true;
      const tick = () => {
        frames += 1;
        if (counting) requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
      const pointer = (type: string, x: number) =>
        target.dispatchEvent(
          new PointerEvent(type, { pointerId: 7, clientX: x, clientY: rect.top + 50, bubbles: true }),
        );
      const started = performance.now();
      pointer('pointerdown', rect.left + 50);
      for (let step = 1; step <= 60; step += 1) {
        pointer('pointermove', rect.left + 50 + step);
        await yieldTask();
      }
      pointer('pointerup', rect.left + 110);
      await new Promise((resolve) => requestAnimationFrame(resolve));
      await new Promise((resolve) => requestAnimationFrame(resolve));
      counting = false;
      const elapsedMs = Math.round(performance.now() - started);
      return { draws: draws() - drawsBefore, frames, events: 60, elapsedMs };
    });
    test.info().annotations.push({ type: 'pointer-burst', description: JSON.stringify(burst) });
    expect(burst.draws).toBeGreaterThan(0);
    expect(burst.draws).toBeLessThanOrEqual(burst.frames + 1);
    expect(burst.draws).toBeLessThan(burst.events);

    // Culling and run-merging must not lose ink: the zoomed-in view still
    // shows strokes, and zooming back out shows the whole page again.
    expect(await inkPixelCount(page)).toBeGreaterThan(0);
    await page.evaluate(() => {
      const target = document.querySelector('[data-testid="ink-canvas"]')!;
      for (let notch = 0; notch < 400; notch += 1) {
        target.dispatchEvent(new WheelEvent('wheel', { deltaY: 1, bubbles: true, cancelable: true }));
      }
    });
    await nextFrames(page, 2);
    expect(await inkPixelCount(page)).toBeGreaterThan(0);
  });
});

/// Drag across the canvas one pointer move per frame and report frame intervals.
async function panBenchmark(page: Page): Promise<{ meanMs: number; p95Ms: number; lastDrawMs: number | null }> {
  return page.evaluate(async () => {
    const target = document.querySelector('[data-testid="ink-canvas"]')!;
    const rect = target.getBoundingClientRect();
    const frame = () => new Promise<number>((resolve) => requestAnimationFrame(resolve));
    const pointer = (type: string, x: number) =>
      target.dispatchEvent(new PointerEvent(type, { pointerId: 9, clientX: x, clientY: rect.top + 80, bubbles: true }));
    pointer('pointerdown', rect.left + 200);
    let last = await frame();
    const intervals: number[] = [];
    for (let step = 1; step <= 60; step += 1) {
      pointer('pointermove', rect.left + 200 + (step % 20) * (step % 40 < 20 ? 3 : -3));
      const now = await frame();
      intervals.push(now - last);
      last = now;
    }
    pointer('pointerup', rect.left + 200);
    intervals.sort((a, b) => a - b);
    const mean = intervals.reduce((sum, value) => sum + value, 0) / intervals.length;
    const round = (value: number) => Math.round(value * 10) / 10;
    const lastDraw = (window as any).__vNoteInkLastDrawMs as number | undefined;
    return {
      meanMs: round(mean),
      p95Ms: round(intervals[Math.floor(intervals.length * 0.95)]),
      lastDrawMs: lastDraw === undefined ? null : round(lastDraw),
    };
  });
}

async function nextFrames(page: Page, count: number): Promise<void> {
  await page.evaluate(async (frames) => {
    for (let index = 0; index < frames; index += 1) {
      await new Promise((resolve) => requestAnimationFrame(resolve));
    }
  }, count);
}

/// Pixels tinted by the seeded ink colour (dark blue), including the
/// antialiased edges of hairlines, ignoring white and the grey/blue-grey paper.
async function inkPixelCount(page: Page): Promise<number> {
  return page.evaluate(() => {
    const canvas = document.querySelector('[data-testid="ink-canvas"]') as HTMLCanvasElement;
    const context = canvas.getContext('2d')!;
    const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
    let count = 0;
    for (let index = 0; index < pixels.length; index += 4) {
      if (pixels[index + 2] - pixels[index] > 40) count += 1;
    }
    return count;
  });
}

/// Commit a page of handwriting-like pressure strokes: rows of short wavy
/// "words", each a v2 stroke whose pressure swells and fades along its length.
async function seedDensePage(page: Page, pageId: string, ticket: string): Promise<void> {
  const batches = Array.from({ length: BATCHES }, (_, batchIndex) => ({
    type: 'commit-batch',
    client_batch_id: `dense-render-${batchIndex}`,
    strokes: Array.from({ length: STROKES_PER_BATCH }, (_, strokeIndex) => {
      const originX = 60 + strokeIndex * 90;
      const originY = 80 + batchIndex * 90;
      return {
        id: `stroke-dense-${batchIndex}-${strokeIndex}-${crypto.randomUUID()}`,
        style: {
          tool_kind: 'solid_round',
          style_version: 2,
          parameters: { color: '#1A237E', width: 6.0, cap_style: 'round', join_style: 'round' },
        },
        points: Array.from({ length: POINTS_PER_STROKE }, (_, pointIndex) => {
          const fraction = pointIndex / (POINTS_PER_STROKE - 1);
          return {
            x: originX + fraction * 70,
            y: originY + Math.sin(fraction * Math.PI * 4) * 18,
            t: pointIndex * 8,
            pressure: Math.round(Math.sin(fraction * Math.PI) * 1000) / 1000,
          };
        }),
      };
    }),
  }));

  const committed = await page.evaluate(
    async ({ pageId, ticket, batches }) => {
      const url = `${location.origin.replace(/^http/, 'ws')}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`;
      const ws = new WebSocket(url);
      let committedBatches = 0;
      const errors: string[] = [];
      ws.addEventListener('message', (event) => {
        try {
          const message = JSON.parse(event.data);
          if (message.type === 'stroke-batch') committedBatches += 1;
          if (message.type === 'error') errors.push(message.message);
        } catch {
          /* ignore non-JSON frames */
        }
      });
      await new Promise<void>((resolve, reject) => {
        ws.addEventListener('open', () => resolve());
        ws.addEventListener('error', () => reject(new Error('socket failed')));
      });
      const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
      ws.send(JSON.stringify({ type: 'subscribe', from_seq: 0 }));
      await pause(50);
      ws.send(JSON.stringify({ type: 'acquire-lease' }));
      await pause(100);
      for (const batch of batches) {
        ws.send(JSON.stringify(batch));
        await pause(20);
      }
      const deadline = Date.now() + 20_000;
      while (committedBatches < batches.length && errors.length === 0 && Date.now() < deadline) {
        await pause(50);
      }
      ws.close();
      return { committedBatches, errors };
    },
    { pageId, ticket, batches },
  );
  expect(committed).toEqual({ committedBatches: BATCHES, errors: [] });
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
