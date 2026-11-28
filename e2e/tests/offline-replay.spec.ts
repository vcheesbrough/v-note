import * as net from 'net';
import { expect, test, type APIRequestContext, type Page } from '../csp-guard';

// Offline mode's server half (#159), against the running stack. Android
// creates pages offline under ids it mints itself and replays its outbox over
// the page channel whenever it reconnects — from new sessions, possibly many
// times. Driven here through the real HTTP API and real page sockets: a create
// replayed under the same client id is one page (`201`, then `200`), and a
// stroke batch committed from two separate sockets is stored once, as a third
// socket's replay shows.
//
// #651: the same holds across an outage in the middle of a session. Android
// treats every failure alike — it keeps what it owes and sends it all again
// once the server answers — so whatever it had sent when the server went, and
// whatever it drew while it was away, must end up stored exactly once.

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

  test('edits owed across a server outage mid-session are each stored exactly once', async ({ page, request }) => {
    const pageId = clientPageId();
    const made = await request.post('/api/pages', { data: { id: pageId, title: '', paper: 'none' } });
    expect(made.status()).toBe(201);

    // The device reaches the server through an edge the test can take away:
    // while it is down every connection is refused and every open channel is
    // dropped, which is what a stopped server container is to a client. (The
    // container itself cannot be stopped from here: the suite runs under
    // `--abort-on-container-exit`, so that would end the run.)
    const app = new URL(process.env.BASE_URL!);
    const edge = new Edge({ host: app.hostname, port: Number(app.port) || 443 });
    await edge.up();
    try {
      await page.goto(`https://127.0.0.1:${edge.port}/health`, { waitUntil: 'load' });

      // Mid-session: one stroke acknowledged, a second sent as the server
      // goes — its answer, if there was one, is lost.
      await openSocket(page, request, 'before', pageId);
      const first = await sendAndAwait(page, 'before', commit('outage-batch-1', 'outage-stroke-1'), isEcho('outage-batch-1'));
      await page.evaluate(
        (text) => (window as any).__replaySockets.before.send(text),
        JSON.stringify(commit('outage-batch-2', 'outage-stroke-2')),
      );
      await edge.down();
      await expect
        .poll(() => page.evaluate(() => (window as any).__replaySockets.before.readyState), { timeout: 10_000 })
        .toBe(3 /* CLOSED */);

      // The server is away: nothing gets through, however often it is tried.
      for (const attempt of ['retry-1', 'retry-2']) {
        expect(await tryOpenSocket(page, request, attempt, pageId), `${attempt} while the server is away`).toBe(false);
      }

      // Back. The device does not know what the server stored, so it sends
      // everything it owes: the acknowledged batch too, the one in flight, and
      // one drawn while the server was away. Then again, from another session.
      await edge.up();
      const owed: Array<[string, string]> = [
        ['outage-batch-1', 'outage-stroke-1'],
        ['outage-batch-2', 'outage-stroke-2'],
        ['outage-batch-3', 'outage-stroke-3'],
      ];
      const seqs: Record<string, number[]> = {};
      for (const session of ['after-1', 'after-2']) {
        await openSocket(page, request, session, pageId);
        for (const [batch, stroke] of owed) {
          const echo = await sendAndAwait(page, session, commit(batch, stroke), isEcho(batch));
          (seqs[batch] ??= []).push(echo.seq);
        }
        await closeSocket(page, session);
      }
      expect(seqs['outage-batch-1'], 'the batch acknowledged before the outage is the same batch').toEqual([first.seq, first.seq]);
      for (const [batch] of owed) {
        expect(new Set(seqs[batch]).size, `${batch} was stored once, whichever attempt landed it`).toBe(1);
      }

      // The server ends with every stroke exactly once.
      await openSocket(page, request, 'check', pageId);
      const replay = await sendAndAwait(page, 'check', { type: 'subscribe', from_seq: 0 }, (m) => m.type === 'page-replay');
      const batches = replay.batches.map((batch: any) => batch.client_batch_id).sort();
      const strokes = replay.batches.flatMap((batch: any) => batch.strokes.map((s: any) => s.id)).sort();
      expect(batches).toEqual(owed.map(([batch]) => batch));
      expect(strokes).toEqual(owed.map(([, stroke]) => stroke));
      await closeSocket(page, 'check');
    } finally {
      await edge.down();
      await request.delete(`/api/pages/${pageId}`);
    }
  });
});

/// A TCP pass-through to the app that the test can take down and bring back
/// on the same port. Down, it refuses every connection and has dropped every
/// one that was open; TLS and WebSockets pass through it untouched.
class Edge {
  port = 0;
  private server?: net.Server;
  private readonly sockets = new Set<net.Socket>();

  constructor(private readonly target: { host: string; port: number }) {}

  async up(): Promise<void> {
    const server = net.createServer((client) => {
      const upstream = net.connect(this.target);
      client.pipe(upstream);
      upstream.pipe(client);
      for (const socket of [client, upstream]) {
        this.sockets.add(socket);
        socket.on('error', () => {});
        socket.on('close', () => {
          this.sockets.delete(socket);
          client.destroy();
          upstream.destroy();
        });
      }
    });
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject);
      server.listen(this.port, '127.0.0.1', resolve);
    });
    this.port = (server.address() as net.AddressInfo).port;
    this.server = server;
  }

  async down(): Promise<void> {
    const server = this.server;
    this.server = undefined;
    for (const socket of this.sockets) socket.destroy();
    if (server) await new Promise<void>((resolve) => server.close(() => resolve()));
  }
}

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
  expect(await tryOpenSocket(page, request, name, pageId), `page channel ${name} opens`).toBe(true);
  await awaitMessage(page, name, (m) => m.type === 'welcome');
}

/// Whether a page channel named `name` could be opened at all.
async function tryOpenSocket(page: Page, request: APIRequestContext, name: string, pageId: string): Promise<boolean> {
  const ticket = await realtimeTicket(request);
  return page.evaluate(
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
