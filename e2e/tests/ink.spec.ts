import { expect, request, test, type APIRequestContext, type Page } from '@playwright/test';

// Ink (page channel) e2e. Drives the per-page WSS from a real browser context
// using a short-lived realtime ticket (the SPA auth path) — no extra npm deps.
// Covers commit + sequencing, persistence/snapshot, gap-fill, the single-editor
// edit lease, owner isolation, and SPA Canvas2D live replay.

test.beforeEach(async ({ page }) => {
  await page.goto('/', { waitUntil: 'load' });
});

test.describe('ink page channel', () => {
  test('opens a pre-v3 page after deterministic legacy-stroke migration', async ({ page, request }) => {
    const pageId = 'page_legacy_v2';
    const ticket = await realtimeTicket(request);
    const snapshot = await driveSocket(page, {
      pageId,
      ticket,
      actions: [{ delayMs: 50, message: { type: 'subscribe', from_seq: 0 } }],
      settleMs: 500,
    });
    const migrated = snapshot.messages.find((message) => message.type === 'stroke-batch');
    expect(migrated, 'legacy batch loads through the v3 page channel').toBeTruthy();
    expect(migrated.strokes[0].id).toMatch(/^stroke_legacy_[0-9a-f]{32}$/);
    expect(migrated.strokes[0].style).toEqual({
      tool_kind: 'solid_round',
      style_version: 1,
      parameters: {
        color: '#006400',
        width: 4.0,
        cap_style: 'round',
        join_style: 'round',
      },
    });

    const openPage = page.getByRole('button', { name: 'Open Legacy protocol 2 page', exact: true });
    await expect(openPage).toBeVisible();
    await openPage.click();
    await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();
    await expect(page.getByText(/Live · seq 1|Synced · seq 1/)).toBeVisible({ timeout: 5_000 });
  });

  test('generates revisioned thumbnails and updates the SPA library', async ({ page, request }) => {
    const title = uniqueTitle('thumbnail');
    const pageId = await createPage(request, title);
    await page.reload({ waitUntil: 'load' });
    await expect(page.getByRole('button', { name: `Open ${title}`, exact: true })).toBeVisible();

    const firstStrokeId = `stroke-thumbnail-${crypto.randomUUID()}`;
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
            client_batch_id: 'thumb-1',
            strokes: sampleViewerStrokes(firstStrokeId),
          },
        },
      ],
      settleMs: 500,
    });

    await expect.poll(async () => {
      const response = await request.get('/api/pages');
      const summary = (await response.json()).pages.find((item: any) => item.id === pageId);
      return summary?.thumbnail?.status;
    }).toBe('available');
    const firstSummary = (await (await request.get('/api/pages')).json()).pages.find((item: any) => item.id === pageId);
    expect(firstSummary.thumbnail.source_seq).toBe(1);
    await expect(page.locator(`img.page-preview[src="${firstSummary.thumbnail.url}"]`)).toBeVisible();

    const firstImage = await request.get(firstSummary.thumbnail.url);
    expect(firstImage.status()).toBe(200);
    expect(firstImage.headers()['content-type']).toBe('image/png');
    expect(firstImage.headers()['cache-control']).toContain('immutable');
    const firstImageBody = await firstImage.body();
    expect(firstImageBody.subarray(1, 4).toString()).toBe('PNG');

    const other = await otherOwnerContext();
    expect((await other.get(firstSummary.thumbnail.url)).status()).toBe(403);
    await other.dispose();

    const ticket2 = await realtimeTicket(request);
    await driveSocket(page, {
      pageId,
      ticket: ticket2,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 100,
          message: {
            type: 'commit-tombstones',
            client_mutation_id: 'thumb-2',
            stroke_ids: [firstStrokeId],
          },
        },
      ],
      settleMs: 500,
    });
    await expect.poll(async () => {
      const summary = (await (await request.get('/api/pages')).json()).pages.find((item: any) => item.id === pageId);
      return summary?.thumbnail?.source_seq;
    }).toBe(2);
    const secondSummary = (await (await request.get('/api/pages')).json()).pages.find((item: any) => item.id === pageId);
    const secondImage = await request.get(secondSummary.thumbnail.url);
    expect(secondImage.status()).toBe(200);
    expect((await secondImage.body()).equals(firstImageBody)).toBeFalsy();
    const immutableFirstImage = await request.get(firstSummary.thumbnail.url);
    expect(immutableFirstImage.status()).toBe(200);
    expect((await immutableFirstImage.body()).equals(firstImageBody)).toBeTruthy();

    const ticket3 = await realtimeTicket(request);
    await driveSocket(page, {
      pageId,
      ticket: ticket3,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        ...Array.from({ length: 10 }, (_, index) => ({
          delayMs: 60,
          message: {
            type: 'commit-batch',
            client_batch_id: `thumb-${index + 3}`,
            strokes: sampleStrokes(),
          },
        })),
      ],
      settleMs: 1_000,
    });
    await expect.poll(async () => {
      const summary = (await (await request.get('/api/pages')).json()).pages.find((item: any) => item.id === pageId);
      return summary?.thumbnail?.source_seq;
    }).toBe(12);
    expect((await request.get(firstSummary.thumbnail.url)).status()).toBe(410);
  });

  test('a dense page of short pressure strokes renders as legible ink, not circles', async ({ page, request }) => {
    // Regression coverage for iteration 21 (#281): the thumbnail renderer once
    // collapsed every letter-sized v2 stroke on a dense page into a filled
    // circle, and a follow-up bug double-scaled stroke width so lines washed
    // out to a near-invisible hairline instead. Both only reproduce once real
    // ink spans a wide enough world extent to shrink the server's fitted
    // preview scale — a single short stroke alone on a page does not trigger
    // either bug.
    //
    // Two single-point "anchor" taps at opposite corners fix that scale
    // without drawing a line through the canvas (a line anchor would risk
    // visually overlapping a letter and letting its ink alone satisfy the
    // assertions below). Three short "letter" strokes sit well inside those
    // corners; each is checked in its own cropped region so a regression
    // that makes the letters vanish or shrink to dots can't hide behind
    // the anchors or the other letters.
    const title = uniqueTitle('dense-ink');
    const pageId = await createPage(request, title);
    const ticket = await realtimeTicket(request);

    const pressureStyle = {
      tool_kind: 'solid_round',
      style_version: 2,
      parameters: { color: '#006400', width: 4.0, cap_style: 'round', join_style: 'round' },
    };
    const anchors = [
      [0, 0],
      [1500, 1000],
    ].map(([x, y], index) => ({
      id: `stroke-anchor-${index}-${crypto.randomUUID()}`,
      style: pressureStyle,
      points: [{ x, y, t: 0, pressure: 0.7 }],
    }));
    const letterOrigins: [number, number][] = [
      [200, 100],
      [700, 400],
      [1200, 850],
    ];
    const letters = letterOrigins.map(([x, y], index) => ({
      id: `stroke-letter-${index}-${crypto.randomUUID()}`,
      style: pressureStyle,
      points: [
        { x, y, t: 0, pressure: 0.7 },
        { x: x + 30, y, t: 1, pressure: 0.7 },
      ],
    }));

    await driveSocket(page, {
      pageId,
      ticket,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 100,
          message: { type: 'commit-batch', client_batch_id: 'dense-ink-1', strokes: [...anchors, ...letters] },
        },
      ],
      settleMs: 500,
    });

    await expect
      .poll(async () => {
        const summary = (await (await request.get('/api/pages')).json()).pages.find(
          (item: any) => item.id === pageId,
        );
        return summary?.thumbnail?.status;
      })
      .toBe('available');
    const summary = (await (await request.get('/api/pages')).json()).pages.find(
      (item: any) => item.id === pageId,
    );
    const image = await request.get(summary.thumbnail.url);
    expect(image.status()).toBe(200);
    const pngBase64 = (await image.body()).toString('base64');

    // Mirrors the fitted-scale/offset math in crates/server/src/thumbnails.rs
    // render(): world bounds come from the two corner anchors (0,0) and
    // (1500,1000), which dominate the three inset letters.
    const THUMB_WIDTH = 240;
    const THUMB_HEIGHT = 160;
    const PADDING = 12;
    const boundsW = 1500;
    const boundsH = 1000;
    const scale = Math.min((THUMB_WIDTH - PADDING * 2) / boundsW, (THUMB_HEIGHT - PADDING * 2) / boundsH);
    const offsetX = (THUMB_WIDTH - boundsW * scale) / 2;
    const offsetY = (THUMB_HEIGHT - boundsH * scale) / 2;
    // Per-letter crop window in device px, generous enough to survive
    // antialiasing/round-cap spread but well short of reaching a neighbor.
    const MARGIN = 12;
    const letterWindows = letterOrigins.map(([x, y]) => {
      const cx = (x + 15) * scale + offsetX;
      const cy = y * scale + offsetY;
      return {
        minX: Math.max(0, Math.round(cx - MARGIN)),
        maxX: Math.min(THUMB_WIDTH - 1, Math.round(cx + MARGIN)),
        minY: Math.max(0, Math.round(cy - MARGIN)),
        maxY: Math.min(THUMB_HEIGHT - 1, Math.round(cy + MARGIN)),
      };
    });

    // Decode via the browser's own PNG decoder (canvas), so no extra e2e
    // dependency is needed — the page under test already runs in a real
    // browser context.
    const perLetter = await page.evaluate(
      ({ base64, windows }) =>
        new Promise<{ darkestGreen: number; width: number; height: number }[]>((resolve, reject) => {
          const img = new Image();
          img.onload = () => {
            const canvas = document.createElement('canvas');
            canvas.width = img.width;
            canvas.height = img.height;
            const ctx = canvas.getContext('2d')!;
            ctx.drawImage(img, 0, 0);
            const results = windows.map((w: any) => {
              const { data } = ctx.getImageData(w.minX, w.minY, w.maxX - w.minX + 1, w.maxY - w.minY + 1);
              const regionW = w.maxX - w.minX + 1;
              let darkestGreen = 255;
              let minX = regionW;
              let maxX = 0;
              let minY = w.maxY - w.minY + 1;
              let maxY = 0;
              for (let py = 0; py < w.maxY - w.minY + 1; py++) {
                for (let px = 0; px < regionW; px++) {
                  const i = (py * regionW + px) * 4;
                  const [r, g, b] = [data[i], data[i + 1], data[i + 2]];
                  if (g > r && g > b) {
                    darkestGreen = Math.min(darkestGreen, g);
                    minX = Math.min(minX, px);
                    maxX = Math.max(maxX, px);
                    minY = Math.min(minY, py);
                    maxY = Math.max(maxY, py);
                  }
                }
              }
              return { darkestGreen, width: maxX >= minX ? maxX - minX + 1 : 0, height: maxY >= minY ? maxY - minY + 1 : 0 };
            });
            resolve(results);
          };
          img.onerror = () => reject(new Error('thumbnail PNG failed to decode'));
          img.src = `data:image/png;base64,${base64}`;
        }),
      { base64: pngBase64, windows: letterWindows },
    );

    perLetter.forEach((result, index) => {
      // Canonical ink (#006400) has green=100; a near-invisible wash (the
      // width-double-scaling bug) never gets ink dark enough.
      expect(result.darkestGreen, `letter ${index} should be solid ink, not a faint wash`).toBeLessThanOrEqual(150);
      expect(result.width, `letter ${index} should render as visible ink`).toBeGreaterThan(0);
      // The circle-collapse bug drew a ~20px circle (roughly as tall as
      // wide); a correctly rendered short horizontal stroke is elongated.
      expect(result.width, `letter ${index} should be elongated, not round (w=${result.width} h=${result.height})`).toBeGreaterThan(
        result.height,
      );
    });
  });

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

  test('replays tombstones on fresh load and reconnect without resurrecting ink', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('ink-tombstone-replay'));
    const strokeId = `stroke-tombstone-replay-${crypto.randomUUID()}`;

    const mutation = await driveSocket(page, {
      pageId,
      ticket: await realtimeTicket(request),
      actions: [
        { delayMs: 50, message: { type: 'subscribe', from_seq: 0 } },
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 150,
          message: {
            type: 'commit-batch',
            client_batch_id: 'batch_tombstone_replay',
            strokes: sampleStrokes(strokeId),
          },
        },
        {
          delayMs: 150,
          message: {
            type: 'commit-tombstones',
            client_mutation_id: 'erase_tombstone_replay',
            stroke_ids: [strokeId],
          },
        },
      ],
      settleMs: 800,
    });
    expect(mutation.messages.some((message) =>
      message.type === 'tombstone-batch' && message.stroke_ids.includes(strokeId),
    )).toBeTruthy();

    const fresh = await driveSocket(page, {
      pageId,
      ticket: await realtimeTicket(request),
      actions: [{ delayMs: 50, message: { type: 'subscribe', from_seq: 0 } }],
      settleMs: 700,
    });
    expect(replayedStrokeIds(fresh.messages)).not.toContain(strokeId);
    expect(fresh.messages.some((message) =>
      message.type === 'tombstone-batch' && message.stroke_ids.includes(strokeId),
    )).toBeTruthy();

    const reconnect = await driveSocket(page, {
      pageId,
      ticket: await realtimeTicket(request),
      actions: [{ delayMs: 50, message: { type: 'subscribe', from_seq: 1 } }],
      settleMs: 700,
    });
    expect(replayedStrokeIds(reconnect.messages)).not.toContain(strokeId);
    expect(reconnect.messages.some((message) =>
      message.type === 'tombstone-batch' && message.stroke_ids.includes(strokeId),
    )).toBeTruthy();
  });

  test('re-adding an erased stroke id stays deleted (delete-wins on live add)', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('ink-add-after-delete'));
    const strokeId = `stroke-add-after-delete-${crypto.randomUUID()}`;

    const result = await driveSocket(page, {
      pageId,
      ticket: await realtimeTicket(request),
      actions: [
        { delayMs: 50, message: { type: 'subscribe', from_seq: 0 } },
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 150,
          message: {
            type: 'commit-batch',
            client_batch_id: 'batch_before_erase',
            strokes: sampleStrokes(strokeId),
          },
        },
        {
          delayMs: 150,
          message: {
            type: 'commit-tombstones',
            client_mutation_id: 'erase_before_readd',
            stroke_ids: [strokeId],
          },
        },
        {
          delayMs: 200,
          message: {
            type: 'commit-batch',
            client_batch_id: 'batch_after_erase',
            strokes: sampleStrokes(strokeId),
          },
        },
      ],
      settleMs: 900,
    });

    // The erase is acknowledged, and the later re-add of the tombstoned id is
    // never broadcast as a visible stroke batch.
    expect(result.messages.some((message) =>
      message.type === 'tombstone-batch' && message.stroke_ids.includes(strokeId),
    )).toBeTruthy();
    expect(result.messages.filter((message) =>
      message.type === 'stroke-batch' && message.client_batch_id === 'batch_after_erase',
    )).toHaveLength(0);

    // A fresh full snapshot never resurrects the erased stroke either.
    const fresh = await driveSocket(page, {
      pageId,
      ticket: await realtimeTicket(request),
      actions: [{ delayMs: 50, message: { type: 'subscribe', from_seq: 0 } }],
      settleMs: 700,
    });
    expect(replayedStrokeIds(fresh.messages)).not.toContain(strokeId);
  });

  test('opening a page records realtime frame sizes and replay cost', async ({ page, request }) => {
    const title = uniqueTitle('ink-metrics');
    await createPage(request, title);
    const before = await scrapeMetrics(request);

    const openPage = page.getByRole('button', { name: `Open ${title}`, exact: true });
    await openPage.waitFor({ state: 'visible', timeout: 5_000 }).catch(async () => {
      await page.reload({ waitUntil: 'load' });
      await expect(openPage).toBeVisible({ timeout: 5_000 });
    });
    await openPage.click();
    await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();
    await expect(page.getByText(/Synced · seq 0|Connected · seq 0|Live · seq 0/)).toBeVisible({ timeout: 5_000 });

    // /metrics is process-wide and other workers share the app, so assert that
    // opening this page moved the counters rather than pinning absolute values.
    const pageFrame = (messageType: string) => ({ channel: 'page', message_type: messageType });
    await expect
      .poll(async () => metricValue(await scrapeMetrics(request), 'v_note_realtime_replay_frames_count'))
      .toBeGreaterThan(metricValue(before, 'v_note_realtime_replay_frames_count'));
    const after = await scrapeMetrics(request);
    for (const messageType of ['welcome', 'synced']) {
      const name = 'v_note_realtime_message_bytes_count';
      expect(metricValue(after, name, pageFrame(messageType)), `${messageType} frames counted`).toBeGreaterThan(
        metricValue(before, name, pageFrame(messageType)),
      );
      expect(metricValue(after, 'v_note_realtime_message_bytes_sum', pageFrame(messageType))).toBeGreaterThan(0);
    }
    expect(metricValue(after, 'v_note_realtime_replay_bytes_sum')).toBeGreaterThan(0);
    expect(
      metricValue(after, 'v_note_realtime_message_handling_seconds_count', { message_type: 'subscribe' }),
    ).toBeGreaterThan(metricValue(before, 'v_note_realtime_message_handling_seconds_count', { message_type: 'subscribe' }));
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
      return greenPixels > 5;
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

  test('SPA library re-sorts live when a page is edited', async ({ page, request }) => {
    const titleOlder = uniqueTitle('resort-older');
    const titleNewer = uniqueTitle('resort-newer');
    // Older created first, newer second — the newer page starts on top of the
    // recent-first (updated_at DESC) library.
    const olderId = await createPage(request, titleOlder);
    await createPage(request, titleNewer);

    const labelOlder = `Open ${titleOlder}`;
    const labelNewer = `Open ${titleNewer}`;

    await page.reload({ waitUntil: 'load' });
    await expect(page.getByRole('button', { name: labelOlder, exact: true })).toBeVisible({ timeout: 5_000 });
    await expect(page.getByRole('button', { name: labelNewer, exact: true })).toBeVisible({ timeout: 5_000 });

    // DOM order of the two tiles among the whole library (indices are relative,
    // so other owner pages in the list do not perturb the assertion).
    const order = async () => {
      const labels = await page
        .locator('ul.page-grid li.page-tile button.page-preview-button')
        .evaluateAll((nodes) => nodes.map((node) => node.getAttribute('aria-label')));
      return { older: labels.indexOf(labelOlder), newer: labels.indexOf(labelNewer) };
    };

    const initial = await order();
    expect(initial.newer).toBeGreaterThanOrEqual(0);
    expect(initial.newer, 'newer page starts ahead of older').toBeLessThan(initial.older);

    // Edit the older page over the realtime channel. No reload after this — the
    // re-sort must be driven live by the page-updated library event alone.
    const ticket = await realtimeTicket(request);
    const commit = await driveSocket(page, {
      pageId: olderId,
      ticket,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        { delayMs: 100, message: { type: 'commit-batch', client_batch_id: 'resort-edit', strokes: sampleStrokes() } },
      ],
      settleMs: 250,
    });
    expect(commit.messages.some((m) => m.type === 'stroke-batch' && m.client_batch_id === 'resort-edit')).toBeTruthy();

    await expect.poll(async () => {
      const current = await order();
      return current.older >= 0 && current.older < current.newer;
    }, { timeout: 5_000 }).toBeTruthy();
  });

  test('SPA library re-sorts live when a page\'s strokes are erased', async ({ page, request }) => {
    const titleTarget = uniqueTitle('erase-target');
    const titleOther = uniqueTitle('erase-other');
    const targetId = await createPage(request, titleTarget);
    const otherId = await createPage(request, titleOther);

    const labelTarget = `Open ${titleTarget}`;
    const labelOther = `Open ${titleOther}`;

    await page.reload({ waitUntil: 'load' });
    await expect(page.getByRole('button', { name: labelTarget, exact: true })).toBeVisible({ timeout: 5_000 });
    await expect(page.getByRole('button', { name: labelOther, exact: true })).toBeVisible({ timeout: 5_000 });

    const order = async () => {
      const labels = await page
        .locator('ul.page-grid li.page-tile button.page-preview-button')
        .evaluateAll((nodes) => nodes.map((node) => node.getAttribute('aria-label')));
      return { target: labels.indexOf(labelTarget), other: labels.indexOf(labelOther) };
    };

    const strokeId = `stroke-erase-${crypto.randomUUID()}`;

    // Seed a stroke on the target, then bump the other page so the target is no
    // longer on top immediately before the erase.
    await driveSocket(page, {
      pageId: targetId,
      ticket: await realtimeTicket(request),
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        { delayMs: 100, message: { type: 'commit-batch', client_batch_id: 'erase-seed', strokes: sampleStrokes(strokeId) } },
      ],
      settleMs: 250,
    });
    await driveSocket(page, {
      pageId: otherId,
      ticket: await realtimeTicket(request),
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        { delayMs: 100, message: { type: 'commit-batch', client_batch_id: 'erase-bump', strokes: sampleStrokes() } },
      ],
      settleMs: 250,
    });
    await expect.poll(async () => {
      const current = await order();
      return current.other >= 0 && current.other < current.target;
    }, { timeout: 5_000 }).toBeTruthy();

    // Erase the target's stroke over the realtime channel. No reload after this —
    // the live re-sort must be driven by the page-updated event on the tombstone
    // (erase) commit path.
    const erase = await driveSocket(page, {
      pageId: targetId,
      ticket: await realtimeTicket(request),
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        { delayMs: 100, message: { type: 'commit-tombstones', client_mutation_id: 'erase-resort', stroke_ids: [strokeId] } },
      ],
      settleMs: 250,
    });
    expect(erase.messages.some((m) => m.type === 'tombstone-batch' && m.stroke_ids.includes(strokeId))).toBeTruthy();

    await expect.poll(async () => {
      const current = await order();
      return current.target >= 0 && current.target < current.other;
    }, { timeout: 5_000 }).toBeTruthy();
  });

  test('SPA replays a v2 pressure stroke with variable width', async ({ page, request }) => {
    const title = uniqueTitle('ink-spa-pressure');
    const pageId = await createPage(request, title);

    const openPage = page.getByRole('button', { name: `Open ${title}`, exact: true });
    await openPage.waitFor({ state: 'visible', timeout: 5_000 }).catch(async () => {
      await page.reload({ waitUntil: 'load' });
      await expect(openPage).toBeVisible({ timeout: 5_000 });
    });
    await openPage.click();
    await expect(page.getByLabel('Read-only ink canvas')).toBeVisible();

    const ticket = await realtimeTicket(request);
    const result = await driveSocket(page, {
      pageId,
      ticket,
      actions: [
        { delayMs: 50, message: { type: 'subscribe', from_seq: 0 } },
        { delayMs: 50, message: { type: 'acquire-lease' } },
        {
          delayMs: 150,
          message: { type: 'commit-batch', client_batch_id: 'batch_spa_pressure', strokes: pressureRampStroke() },
        },
      ],
      settleMs: 300,
    });
    expect(result.messages.some((m) => m.type === 'stroke-batch' && m.client_batch_id === 'batch_spa_pressure')).toBeTruthy();

    // The SPA opens zoomed out (fixed MIN_CANVAS_SCALE), so the ramp is only a
    // few px tall — per-column thickness would be antialiasing noise. Instead sum
    // green *area* in the low-pressure third vs the high-pressure third of the
    // stroke: the high third covers more ink, integrated over many columns.
    const handle = await page.waitForFunction(() => {
      const canvas = document.querySelector<HTMLCanvasElement>('[data-testid="ink-canvas"]');
      if (!canvas) return null;
      const context = canvas.getContext('2d');
      if (!context) return null;
      const { width, height } = canvas;
      const data = context.getImageData(0, 0, width, height).data;
      const isGreen = (i: number) =>
        data[i + 3] > 0 && data[i] < 40 && data[i + 1] > 60 && data[i + 1] < 140 && data[i + 2] < 40;
      const perColumn = new Map<number, number>();
      let minX = Infinity;
      let maxX = -Infinity;
      let total = 0;
      for (let y = 0; y < height; y += 1) {
        for (let x = 0; x < width; x += 1) {
          if (isGreen((y * width + x) * 4)) {
            perColumn.set(x, (perColumn.get(x) ?? 0) + 1);
            if (x < minX) minX = x;
            if (x > maxX) maxX = x;
            total += 1;
          }
        }
      }
      const span = maxX - minX;
      if (span < 20 || total < 20) return null; // wait until the whole ramp is drawn
      const third = span / 3;
      let low = 0;
      let high = 0;
      for (const [x, count] of perColumn) {
        if (x <= minX + third) low += count;
        else if (x >= maxX - third) high += count;
      }
      return { low, high };
    }, null, { timeout: 3_000 });
    const { low, high } = (await handle.jsonValue()) as { low: number; high: number };
    expect(low, 'low-pressure third rendered some ink').toBeGreaterThan(3);
    expect(high, `high-pressure third area (${high}px) must exceed low third (${low}px)`).toBeGreaterThan(low * 1.25);
  });

  test('server rejects pressure on a v1 stroke and out-of-range v2 pressure', async ({ page, request }) => {
    const pageId = await createPage(request, uniqueTitle('ink-pressure-reject'));
    const ticket = await realtimeTicket(request);
    const badV1 = {
      id: `stroke-${crypto.randomUUID()}`,
      style: { tool_kind: 'solid_round', style_version: 1, parameters: { color: '#006400', width: 4.0, cap_style: 'round', join_style: 'round' } },
      points: [{ x: 0.0, y: 0.0, t: 0, pressure: 0.5 }, { x: 10.0, y: 10.0, t: 5, pressure: 0.5 }],
    };
    const badV2 = {
      id: `stroke-${crypto.randomUUID()}`,
      style: { tool_kind: 'solid_round', style_version: 2, parameters: { color: '#006400', width: 4.0, cap_style: 'round', join_style: 'round' } },
      points: [{ x: 0.0, y: 0.0, t: 0, pressure: 1.5 }],
    };
    const result = await driveSocket(page, {
      pageId,
      ticket,
      actions: [
        { delayMs: 50, message: { type: 'acquire-lease' } },
        { delayMs: 100, message: { type: 'commit-batch', client_batch_id: 'bad-v1-pressure', strokes: [badV1] } },
        { delayMs: 100, message: { type: 'commit-batch', client_batch_id: 'bad-v2-pressure', strokes: [badV2] } },
      ],
      settleMs: 400,
    });
    const errors = result.messages.filter((m) => m.type === 'error' && m.code === 'invalid_stroke_style');
    expect(errors.length, 'both invalid-pressure commits are rejected').toBeGreaterThanOrEqual(2);
    // Neither rejected batch is sequenced/persisted.
    expect(result.messages.some((m) => m.type === 'stroke-batch' && (m.client_batch_id === 'bad-v1-pressure' || m.client_batch_id === 'bad-v2-pressure'))).toBeFalsy();
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

function replayedStrokeIds(messages: any[]): string[] {
  return messages
    .filter((message) => message.type === 'stroke-batch')
    .flatMap((message) => message.strokes.map((stroke: any) => stroke.id));
}

function sampleStrokes(id = `stroke-${crypto.randomUUID()}`) {
  return [
    {
      id,
      style: {
        tool_kind: 'solid_round',
        style_version: 1,
        parameters: { color: '#006400', width: 2.0, cap_style: 'round', join_style: 'round' },
      },
      points: [
        { x: 5.0, y: 6.0, t: 0 },
        { x: 7.0, y: 8.0, t: 12 },
      ],
    },
  ];
}

// A long horizontal v2 stroke whose pressure ramps 0 → 1 across many coalesced
// samples; wide enough that the low- vs high-pressure width difference is
// unmistakable once the SPA rasterizes it.
function pressureRampStroke(id = `stroke-${crypto.randomUUID()}`) {
  const samples = 33;
  const points = Array.from({ length: samples }, (_, index) => {
    const fraction = index / (samples - 1);
    return { x: 40.0 + 720.0 * fraction, y: 140.0, t: index, pressure: fraction };
  });
  return [
    {
      id,
      style: {
        tool_kind: 'solid_round',
        style_version: 2,
        parameters: { color: '#006400', width: 32.0, cap_style: 'round', join_style: 'round' },
      },
      points,
    },
  ];
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

// The internal metrics listener — on the compose network only, never via Traefik.
async function scrapeMetrics(ctx: APIRequestContext): Promise<string> {
  const res = await ctx.get('http://app:9090/metrics');
  expect(res.ok()).toBeTruthy();
  return res.text();
}

// Sum of every sample of `name` whose labels include all of `labels`.
function metricValue(body: string, name: string, labels: Record<string, string> = {}): number {
  let total = 0;
  for (const line of body.split('\n')) {
    if (!line.startsWith(`${name}{`) && !line.startsWith(`${name} `)) continue;
    if (!Object.entries(labels).every(([key, value]) => line.includes(`${key}="${value}"`))) continue;
    total += Number(line.slice(line.lastIndexOf(' ') + 1));
  }
  return total;
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
