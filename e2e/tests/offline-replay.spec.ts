import { expect, test, type APIRequestContext, type Page } from '../csp-guard';

// Offline mode's server half (#159), against the running stack. Android
// creates pages offline under ids it mints itself and replays its outbox over
// the page channel whenever it reconnects — from new sessions, possibly many
// times. Driven here through the real HTTP API and real page sockets: a create
// replayed under the same client id is one page (`201`, then `200`), and a
// stroke batch committed from two separate sockets is stored once, as a third
// socket's replay shows.

test.describe('offline replay', () => {
  test('a client-minted create and a replayed batch each land exactly once', async ({ page, request }) => {
    const pageId = clientPageId();

    // The body Android sends for a page created offline.
    const first = await request.post('/api/pages', { data: { id: pageId, title: '', paper: 'none' } });
    expect(first.status(), 'the first create makes the page').toBe(201);
    const created = (await first.json()).page;
    expect(created.id).toBe(pageId);

    const again = await request.post('/api/pages', { data: { id: pageId, title: '', paper: 'none' } });
    expect(again.status(), 'a replayed create returns the page it made').toBe(200);
    expect((await again.json()).page).toEqual(created);
    expect((await listedIds(request)).filter((id) => id === pageId)).toHaveLength(1);

    const invalid = await request.post('/api/pages', { data: { id: 'page_NOT-HEX', paper: 'none' } });
    expect(invalid.status(), 'a malformed client id is refused').toBe(400);

    await page.goto('/', { waitUntil: 'load' });

    // Socket A commits and goes away; socket B, a new session, replays the
    // same outbox entry. Both are acknowledged with the same seq.
    await openSocket(page, request, 'a', pageId);
    const echoA = await sendAndAwait(page, 'a', commit('offline-batch', 'offline-stroke'), isEcho('offline-batch'));
    await closeSocket(page, 'a');
    await openSocket(page, request, 'b', pageId);
    const echoB = await sendAndAwait(page, 'b', commit('offline-batch', 'offline-stroke'), isEcho('offline-batch'));
    expect(echoB.seq, 'the replay is the same batch').toBe(echoA.seq);
    await closeSocket(page, 'b');

    // A third session's replay holds the batch, and its stroke, once.
    await openSocket(page, request, 'c', pageId);
    const replay = await sendAndAwait(page, 'c', { type: 'subscribe', from_seq: 0 }, (m) => m.type === 'page-replay');
    expect(replay.batches.map((batch: any) => batch.client_batch_id)).toEqual(['offline-batch']);
    expect(replay.batches.flatMap((batch: any) => batch.strokes.map((s: any) => s.id))).toEqual(['offline-stroke']);
    expect(replay.last_seq).toBe(echoA.seq);
    await closeSocket(page, 'c');

    await request.delete(`/api/pages/${pageId}`);
  });
});

type Message = Record<string, any>;

/// A page id as Android mints them: `page_` and 32 lowercase hex digits. The
/// compose stack's database outlives a single test, so each run mints afresh.
function clientPageId(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return `page_${Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')}`;
}

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
          parameters: { color: '#006400', width: 6.0, cap_style: 'round', join_style: 'round' },
        },
        points: [
          { x: 20.0, y: 20.0, t: 0 },
          { x: 120.0, y: 80.0, t: 12 },
        ],
      },
    ],
  };
}

function isEcho(batch: string): (message: Message) => boolean {
  return (m) => (m.type === 'stroke-batch' && m.client_batch_id === batch) || m.type === 'error';
}

/// Opens a page channel named `name` in the page's own context (the SPA's
/// ticket path), collecting its messages on `window.__replayMessages[name]`.
async function openSocket(page: Page, request: APIRequestContext, name: string, pageId: string) {
  const ticket = await realtimeTicket(request);
  const opened = await page.evaluate(
    async ({ name, pageId, ticket }) => {
      const w = window as any;
      w.__replaySockets ??= {};
      w.__replayMessages ??= {};
      w.__replayMessages[name] = [];
      const ws = new WebSocket(
        `${location.origin.replace(/^http/, 'ws')}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`,
      );
      ws.addEventListener('message', (event) => w.__replayMessages[name].push(JSON.parse(event.data)));
      w.__replaySockets[name] = ws;
      return new Promise<boolean>((resolve) => {
        ws.addEventListener('open', () => resolve(true));
        ws.addEventListener('error', () => resolve(false));
      });
    },
    { name, pageId, ticket },
  );
  expect(opened, `page channel ${name} opens`).toBe(true);
  await awaitMessage(page, name, (m) => m.type === 'welcome');
}

async function closeSocket(page: Page, name: string) {
  await page.evaluate((name) => (window as any).__replaySockets[name].close(), name);
}

/// The first message on socket `name` that `matches` accepts, removed from its
/// queue so a later wait sees the next one.
async function awaitMessage(page: Page, name: string, matches: (message: Message) => boolean): Promise<Message> {
  let found: Message | undefined;
  await expect
    .poll(async () => {
      const messages: Message[] = await page.evaluate((name) => (window as any).__replayMessages[name], name);
      const index = messages.findIndex(matches);
      if (index < 0) return false;
      found = messages[index];
      await page.evaluate(({ name, index }) => (window as any).__replayMessages[name].splice(index, 1), { name, index });
      return true;
    }, { timeout: 10_000 })
    .toBe(true);
  return found!;
}

async function sendAndAwait(
  page: Page,
  name: string,
  message: Message,
  matches: (message: Message) => boolean,
): Promise<Message> {
  await page.evaluate(({ name, text }) => (window as any).__replaySockets[name].send(text), {
    name,
    text: JSON.stringify(message),
  });
  const reply = await awaitMessage(page, name, matches);
  expect(reply.type, `socket ${name}: ${JSON.stringify(reply)}`).not.toBe('error');
  return reply;
}

async function listedIds(request: APIRequestContext): Promise<string[]> {
  const response = await request.get('/api/pages');
  expect(response.status()).toBe(200);
  return (await response.json()).pages.map((p: { id: string }) => p.id);
}

async function realtimeTicket(ctx: APIRequestContext): Promise<string> {
  const res = await ctx.post('/api/realtime-ticket');
  expect(res.status()).toBe(200);
  return (await res.json()).ticket;
}
