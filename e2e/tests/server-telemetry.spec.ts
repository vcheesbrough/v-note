import { expect, request, test, type APIRequestContext, type Page } from '@playwright/test';

/**
 * #417 — the server's OWN logs, end to end.
 *
 * The server exports log records over OTLP next to its spans, to the
 * `monitor-alloy` stand-in (e2e/alloy/monitor-alloy.alloy), which marks them
 * `log_source="otlp"` and writes them to a real Loki; the same stand-in scrapes
 * the container's stdout as `log_source="docker"`, as the shared Alloy does.
 * So every assertion here is about what Loki and Tempo actually stored:
 *
 *  - a mutation's line reaches Loki over OTLP, with the resource attributes
 *    the spans carry;
 *  - its `trace_id` names a real trace in Tempo holding the request's
 *    `http.request` and `db.query` spans — the log → trace pivot;
 *  - the stdout copy of the same line carries the same `trace_id` and
 *    `span_id` as body fields — the crash-safe path is correlated too.
 *
 * What a green run does NOT prove: that mini-config's shared Alloy has the
 * logs pipeline the stand-in has. That is dev's to show (see the fixture).
 */

const TEMPO_URL = process.env.TEMPO_URL;
const LOKI_URL = process.env.LOKI_URL;

// Loud rather than skipped, as in client-telemetry.spec.ts: the backends are
// part of the stack, so an unset variable is a broken stack, not an opt-out.
if (!TEMPO_URL || !LOKI_URL) {
  throw new Error(
    'TEMPO_URL / LOKI_URL are unset — the telemetry backends are part of ' +
      'e2e/docker-compose.test.yml, so this is a broken stack, not an opt-out',
  );
}

/** The `app` service's `VNOTE__OBSERVABILITY__ENVIRONMENT`. */
const EXPECTED_ENV = 'test';

type Labels = Record<string, string>;
type Stream = { stream: Labels; values: Array<[string, string]> };

/** Polls until `check` returns a value, or the deadline passes. */
async function eventually<T>(
  what: string,
  check: () => Promise<T | null>,
  timeoutMs = 45_000,
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  for (;;) {
    try {
      const result = await check();
      if (result !== null && result !== undefined) {
        return result;
      }
    } catch (error) {
      last = error;
    }
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for ${what}${last ? `: ${last}` : ''}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
}

let backends: APIRequestContext;

test.beforeAll(async () => {
  test.setTimeout(150_000);
  backends = await request.newContext({ ignoreHTTPSErrors: true });
  // Neither image can carry a healthcheck (both distroless), and Tempo's ring
  // takes a while to settle, so the wait lives here.
  await eventually('tempo to be ready', async () => {
    const res = await backends.get(`${TEMPO_URL}/ready`);
    return res.ok() ? true : null;
  }, 60_000);
  await eventually('loki to be ready', async () => {
    const res = await backends.get(`${LOKI_URL}/ready`);
    return res.ok() ? true : null;
  }, 60_000);
});

test.afterAll(async () => {
  await backends.dispose();
});

/**
 * The streams a LogQL query matches, once it matches any. Without the
 * categorize-labels flag Loki folds structured metadata — where OTLP log
 * attributes and the record's trace context land — into each stream's labels,
 * which is what lets the assertions read `trace_id` off `stream`.
 */
async function lokiStreams(query: string): Promise<Stream[]> {
  return eventually(`loki streams for ${query}`, async () => {
    const end = Date.now() * 1e6;
    const res = await backends.get(`${LOKI_URL}/loki/api/v1/query_range`, {
      params: { query, start: `${end - 15 * 60 * 1e9}`, end: `${end}`, limit: '100' },
    });
    if (!res.ok()) return null;
    const results = ((await res.json())?.data?.result ?? []) as Stream[];
    return results.length > 0 ? results : null;
  });
}

type Span = { name: string; attributes: Labels };

/** Every span Tempo stored for a trace, once it has them all. */
async function tempoSpans(traceId: string, expected: (spans: Span[]) => boolean): Promise<Span[]> {
  return eventually(`trace ${traceId} in tempo`, async () => {
    const res = await backends.get(`${TEMPO_URL}/api/traces/${traceId}`);
    if (!res.ok()) return null;
    const body = await res.json();
    const spans: Span[] = (body?.batches ?? []).flatMap((batch: any) =>
      (batch.scopeSpans ?? batch.instrumentationLibrarySpans ?? []).flatMap((scope: any) =>
        (scope.spans ?? []).map((span: any) => ({
          name: span.name,
          attributes: Object.fromEntries(
            (span.attributes ?? []).map(({ key, value }: any) => [
              key,
              String(Object.values(value ?? {})[0]),
            ]),
          ),
        })),
      ),
    );
    return expected(spans) ? spans : null;
  });
}

/** Tempo returns ids as lowercase hex; Loki's structured metadata does too. */
function normalise(id: string | undefined): string {
  return (id ?? '').toLowerCase();
}

function uniqueTitle(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

test.describe('the server exports its own logs, trace-correlated', () => {
  test('a page mutation reaches Loki over OTLP and pivots to its trace', async ({ request }) => {
    const meta = await request.get('/api/meta');
    expect(meta.ok()).toBeTruthy();
    const version = (await meta.json()).app_version as string;

    const created = await request.post('/api/pages', { data: { title: uniqueTitle('server-telemetry') } });
    expect(created.status()).toBe(201);
    const pageId = (await created.json()).page.id as string;

    // --- the OTLP path ---------------------------------------------------
    const [otlp] = await lokiStreams(
      `{service_name="v-note", log_source="otlp"} | page_id="${pageId}" |= "page created"`,
    );
    expect(otlp.values[0][1]).toBe('page created');
    expect(otlp.stream.deployment_environment).toBe(EXPECTED_ENV);
    expect(otlp.stream.service_version).toBe(version);
    const traceId = normalise(otlp.stream.trace_id);
    const spanId = normalise(otlp.stream.span_id);
    expect(traceId, 'the OTLP record carries its trace id').toMatch(/^[0-9a-f]{32}$/);
    expect(spanId, 'the OTLP record carries its span id').toMatch(/^[0-9a-f]{16}$/);
    expect(traceId).not.toBe('0'.repeat(32));

    // --- the pivot: that trace is the request that wrote the line --------
    const spans = await tempoSpans(
      traceId,
      (all) =>
        all.some((span) => span.name === 'http.request') &&
        all.some((span) => span.name === 'db.query' && span.attributes['db.query_name'] === 'create_page'),
    );
    expect(spans.find((span) => span.name === 'http.request')?.attributes.route).toBe('/api/pages');

    // --- the stdout path: same line, same correlation, as body fields -----
    const docker = await lokiStreams(
      `{service_name="v-note", log_source="docker"} |= "${pageId}" |= "page created"`,
    );
    const lines = docker
      .flatMap((stream) => stream.values.map(([, line]) => line))
      .map((line) => JSON.parse(line))
      .filter((line) => line.message === 'page created' && line.page_id === pageId);
    expect(lines, 'the stdout copy of the line reached Loki through the Docker scrape').toHaveLength(1);
    expect(normalise(lines[0].trace_id)).toBe(traceId);
    expect(normalise(lines[0].span_id)).toBe(spanId);
    expect(lines[0].level).toBe('INFO');
  });

  test('a commit-batch is followed from the socket to Loki and its trace', async ({ page, request }) => {
    const created = await request.post('/api/pages', { data: { title: uniqueTitle('server-telemetry-ink') } });
    expect(created.status()).toBe(201);
    const pageId = (await created.json()).page.id as string;
    const ticketRes = await request.post('/api/realtime-ticket');
    expect(ticketRes.status()).toBe(200);
    const ticket = (await ticketRes.json()).ticket as string;
    const clientBatchId = `batch-${Date.now()}-${Math.random().toString(16).slice(2)}`;

    const echoed = await commitOneBatch(page, pageId, ticket, clientBatchId);
    expect(echoed, 'the batch was committed and fanned back').toBeTruthy();

    const [otlp] = await lokiStreams(
      `{service_name="v-note", log_source="otlp"} | client_batch_id="${clientBatchId}" |= "stroke batch committed"`,
    );
    expect(otlp.stream.submitted).toBe('1');
    expect(otlp.stream.visible).toBe('1');
    const traceId = normalise(otlp.stream.trace_id);
    expect(traceId).toMatch(/^[0-9a-f]{32}$/);

    // The message is its own trace root (see handle_page_client_message), and
    // the insert that stored the batch is in it.
    await tempoSpans(
      traceId,
      (all) =>
        all.some((span) => span.name === 'handle_page_client_message') &&
        all.some(
          (span) => span.name === 'db.query' && span.attributes['db.query_name'] === 'persist_batch_insert',
        ),
    );
  });
});

/** Opens the page socket in the browser, commits one stroke, and returns the echo. */
async function commitOneBatch(page: Page, pageId: string, ticket: string, clientBatchId: string) {
  await page.goto('/', { waitUntil: 'load' });
  return page.evaluate(
    async ({ pageId, ticket, clientBatchId }) => {
      const url = `${location.origin.replace(/^http/, 'ws')}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`;
      const ws = new WebSocket(url);
      const messages: any[] = [];
      ws.addEventListener('message', (event) => messages.push(JSON.parse(event.data)));
      await new Promise<void>((resolve, reject) => {
        ws.addEventListener('open', () => resolve());
        ws.addEventListener('error', () => reject(new Error('page socket did not open')));
      });
      const send = async (message: unknown) => {
        ws.send(JSON.stringify(message));
        await new Promise((resolve) => setTimeout(resolve, 150));
      };
      await send({ type: 'subscribe', from_seq: 0 });
      await send({ type: 'acquire-lease' });
      await send({
        type: 'commit-batch',
        client_batch_id: clientBatchId,
        strokes: [
          {
            id: `stroke-${crypto.randomUUID()}`,
            style: {
              tool_kind: 'solid_round',
              style_version: 2,
              parameters: { color: '#006400', width: 2.0, cap_style: 'round', join_style: 'round' },
            },
            points: [
              { x: 5.0, y: 6.0, t: 0 },
              { x: 7.0, y: 8.0, t: 12 },
            ],
          },
        ],
      });
      const deadline = Date.now() + 5_000;
      let echoed: any;
      while (!echoed && Date.now() < deadline) {
        echoed = messages.find((m) => m.type === 'stroke-batch' && m.client_batch_id === clientBatchId);
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
      ws.close();
      return echoed ?? null;
    },
    { pageId, ticket, clientBatchId },
  );
}
