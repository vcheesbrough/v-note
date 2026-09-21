import { expect, request, test } from '@playwright/test';

/**
 * #354 — client telemetry, end to end.
 *
 * Nothing here is mocked. The stack runs the **production**
 * `deploy/alloy/client-telemetry.alloy` in a real Alloy, exporting to a real
 * Tempo and a real Loki at the digests mini-config's monitoring stack runs. So
 * every assertion below is about what those backends actually stored, which is
 * the only thing that makes the interesting claim testable: a mock collector
 * would happily "receive" a span carrying the forged `service.name` the
 * pipeline was supposed to have overwritten.
 *
 * Two things this suite deliberately does not cover, so they are not mistaken
 * for gaps:
 *
 *  - **The kill switch.** This stack runs with ingest on, because that is the
 *    path worth exercising in a browser. The off path (`/otlp` → 404, and the
 *    404 arriving before the 401) is `crates/server/tests/telemetry.rs`.
 *  - **A real WASM panic.** No reachable code path in the SPA panics, and
 *    adding one to make a test pass would be adding a bug. The panic hook's
 *    route to Loki is covered here by posting a record of exactly the shape it
 *    produces; that the hook is installed and formats that shape is
 *    `frontend/src/telemetry.rs`.
 */

const TEMPO_URL = process.env.TEMPO_URL;
const LOKI_URL = process.env.LOKI_URL;
const EXPECTED_ENV = process.env.CLIENT_TELEMETRY_EXPECTED_ENV;
const EXPECTED_VERSION = process.env.CLIENT_TELEMETRY_EXPECTED_VERSION;

// Loud rather than skipped, for the reason spelled out in sql-console.spec.ts:
// `playwright` hard-depends on these services, so an unset variable is a broken
// stack, and a skip would quietly retire the only proof the pipeline works.
if (!TEMPO_URL || !LOKI_URL || !EXPECTED_ENV || !EXPECTED_VERSION) {
  throw new Error(
    'TEMPO_URL / LOKI_URL / CLIENT_TELEMETRY_EXPECTED_ENV / CLIENT_TELEMETRY_EXPECTED_VERSION are unset — ' +
      'the telemetry backends are part of e2e/docker-compose.test.yml, so this is a broken stack, not an opt-out',
  );
}

/** Lowercase hex, as OTLP/JSON requires (not the base64 protobuf would use). */
function hex(bytes: number): string {
  return Array.from({ length: bytes }, () =>
    Math.floor(Math.random() * 256)
      .toString(16)
      .padStart(2, '0'),
  ).join('');
}

const nowNanos = () => `${Date.now()}000000`;

/**
 * A resource that claims to be something else entirely, including a unique
 * `service.instance.id` — which Loki's OTLP ingest promotes to an index label,
 * so it is a stream-cardinality attack and not merely a lie.
 */
function forgedResource(marker: string) {
  return {
    attributes: [
      { key: 'service.name', value: { stringValue: 'totally-not-v-note' } },
      { key: 'deployment.environment', value: { stringValue: 'forged-env' } },
      { key: 'service.version', value: { stringValue: '9.9.9-forged' } },
      { key: 'service.instance.id', value: { stringValue: `cardinality-bomb-${marker}` } },
      // Allow-listed, so this one is expected to survive untouched — which is
      // what shows the pipeline is filtering rather than simply dropping
      // everything it did not write itself.
      { key: 'telemetry.sdk.name', value: { stringValue: 'v-note-spa-otlp' } },
    ],
  };
}

function traceBody(marker: string, traceId: string, spanId: string) {
  return {
    resourceSpans: [
      {
        resource: forgedResource(marker),
        scopeSpans: [
          {
            scope: { name: 'e2e' },
            spans: [
              {
                traceId,
                spanId,
                name: marker,
                kind: 1,
                startTimeUnixNano: nowNanos(),
                endTimeUnixNano: nowNanos(),
              },
            ],
          },
        ],
      },
    ],
  };
}

function logBody(marker: string, traceId: string, spanId: string, body: string) {
  return {
    resourceLogs: [
      {
        resource: forgedResource(marker),
        scopeLogs: [
          {
            scope: { name: 'e2e' },
            logRecords: [
              {
                timeUnixNano: nowNanos(),
                severityNumber: 17,
                severityText: 'ERROR',
                body: { stringValue: body },
                traceId,
                spanId,
                attributes: [{ key: 'exception.type', value: { stringValue: 'panic' } }],
              },
            ],
          },
        ],
      },
    ],
  };
}

/** Polls until `check` returns a value, or the deadline passes. */
async function eventually<T>(
  what: string,
  check: () => Promise<T | null>,
  timeoutMs = 30_000,
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

type Attributes = Record<string, string>;

function flattenAttributes(raw: Array<{ key: string; value: Record<string, string> }>): Attributes {
  return Object.fromEntries(
    (raw ?? []).map(({ key, value }) => [key, Object.values(value ?? {})[0] as string]),
  );
}

let backends: import('@playwright/test').APIRequestContext;

test.beforeAll(async () => {
  // The hook's own limit, which defaults to 30s and would otherwise cut the two
  // waits below off before either could time out on its own terms.
  test.setTimeout(150_000);
  backends = await request.newContext({ ignoreHTTPSErrors: true });
  // Tempo's ring takes a few seconds to settle after the process starts, and
  // neither image can carry a container healthcheck (both are distroless), so
  // the wait lives here rather than in compose.
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

/** The resource attributes Tempo stored for a trace, once it has one. */
async function tempoResource(traceId: string): Promise<Attributes> {
  return eventually(`trace ${traceId} in tempo`, async () => {
    const res = await backends.get(`${TEMPO_URL}/api/traces/${traceId}`);
    if (!res.ok()) return null;
    const body = await res.json();
    const resource = body?.batches?.[0]?.resource?.attributes;
    return resource ? flattenAttributes(resource) : null;
  });
}

async function lokiStreams(query: string, match?: (line: string) => boolean) {
  return eventually(`loki streams for ${query}`, async () => {
    const end = Date.now() * 1e6;
    const start = end - 15 * 60 * 1e9;
    const res = await backends.get(`${LOKI_URL}/loki/api/v1/query_range`, {
      params: { query, start: `${start}`, end: `${end}`, limit: '100' },
    });
    if (!res.ok()) return null;
    const body = await res.json();
    const results = (body?.data?.result ?? []) as Array<{
      stream: Attributes;
      values: Array<[string, string]>;
    }>;
    const hit = match
      ? results.filter((r) => r.values.some(([, line]) => match(line)))
      : results;
    return hit.length > 0 ? hit : null;
  });
}

// ---------------------------------------------------------------------------
// Resource-attribute integrity — the reason the sidecar exists
// ---------------------------------------------------------------------------

test.describe('the sidecar owns what client telemetry says about itself', () => {
  test('a forged service.name on a span is replaced with the sidecar\'s', async ({ request }) => {
    const marker = `spa-forgery-${hex(4)}`;
    const traceId = hex(16);

    const res = await request.post('/otlp/spa/v1/traces', {
      data: traceBody(marker, traceId, hex(8)),
    });
    expect(res.status()).toBe(200);

    const resource = await tempoResource(traceId);
    expect(resource['service.name']).toBe('v-note-spa');
    expect(resource['deployment.environment']).toBe(EXPECTED_ENV);
    expect(resource['service.version']).toBe(EXPECTED_VERSION);
    // Not overwritten — *dropped*. An overwrite-only pipeline would leave this
    // one through, and it is the one that costs Loki a stream per request.
    expect(resource['service.instance.id']).toBeUndefined();
    // The SDK's own description is allow-listed, so it survives.
    expect(resource['telemetry.sdk.name']).toBe('v-note-spa-otlp');
  });

  test('the two receivers give the two clients distinct identities', async ({ request }) => {
    const spaTrace = hex(16);
    const androidTrace = hex(16);

    expect(
      (await request.post('/otlp/spa/v1/traces', { data: traceBody('spa', spaTrace, hex(8)) }))
        .status(),
    ).toBe(200);
    expect(
      (
        await request.post('/otlp/android/v1/traces', {
          data: traceBody('android', androidTrace, hex(8)),
        })
      ).status(),
    ).toBe(200);

    expect((await tempoResource(spaTrace))['service.name']).toBe('v-note-spa');
    expect((await tempoResource(androidTrace))['service.name']).toBe('v-note-android');
  });

  test('a forged identity on a log is replaced before Loki indexes it', async ({ request }) => {
    const marker = `log-forgery-${hex(4)}`;
    const traceId = hex(16);
    const spanId = hex(8);

    const res = await request.post('/otlp/spa/v1/logs', {
      data: logBody(marker, traceId, spanId, `wasm panic: ${marker}`),
    });
    expect(res.status()).toBe(200);

    const streams = await lokiStreams('{service_name="v-note-spa"}', (line) =>
      line.includes(marker),
    );
    const stream = streams[0].stream;
    expect(stream.service_name).toBe('v-note-spa');
    expect(stream.deployment_environment).toBe(EXPECTED_ENV);
    // Trace correlation is the whole point of shipping logs over OTLP: this is
    // what Grafana turns into a link from the log line to the trace.
    expect(stream.trace_id).toBe(traceId);
    expect(stream.span_id).toBe(spanId);
    expect(stream.severity_text).toBe('ERROR');

    // …and nothing was ever stored under the name the client claimed.
    const res2 = await backends.get(`${LOKI_URL}/loki/api/v1/query_range`, {
      params: {
        query: '{service_name="totally-not-v-note"}',
        start: `${Date.now() * 1e6 - 15 * 60 * 1e9}`,
        end: `${Date.now() * 1e6}`,
      },
    });
    expect((await res2.json()).data.result).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// The front door
// ---------------------------------------------------------------------------

test.describe('the /otlp ingress', () => {
  test('rejects an export with no credentials, and forwards nothing', async () => {
    const anonymous = await request.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: {},
    });
    try {
      const res = await anonymous.post('/otlp/spa/v1/traces', {
        data: traceBody('anon', hex(16), hex(8)),
      });
      expect(res.status()).toBe(401);
    } finally {
      await anonymous.dispose();
    }
  });

  /**
   * The SPA's entire auth story: a same-origin `fetch` carries the `HttpOnly`
   * session cookie by itself, so the browser never holds a token. This context
   * has the cookie and *no* Authorization header.
   */
  test('accepts an export authenticated by the session cookie alone', async ({ browser }) => {
    const context = await browser.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      extraHTTPHeaders: {},
    });
    try {
      const traceId = hex(16);
      const res = await context.request.post('/otlp/spa/v1/traces', {
        data: traceBody('cookie-only', traceId, hex(8)),
      });
      expect(res.status()).toBe(200);
      expect((await tempoResource(traceId))['service.name']).toBe('v-note-spa');
    } finally {
      await context.close();
    }
  });

  test('rejects a valid token that lacks the required scope', async () => {
    const tokenUrl = process.env.OIDC_TOKEN_URL;
    test.skip(!tokenUrl, 'OIDC_TOKEN_URL required to mint a wrong-scope token');

    const idp = await request.newContext({ ignoreHTTPSErrors: true });
    const token = await idp
      .post(tokenUrl!, {
        form: {
          grant_type: 'client_credentials',
          client_id: 'v-note-test-wrong',
          client_secret: 'test-secret',
          scope: 'openid profile email v-note:wrong:access',
        },
      })
      .then((res) => res.json())
      .then((body) => body.access_token as string);
    await idp.dispose();

    const wrongScope = await request.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: { Authorization: `Bearer ${token}` },
    });
    try {
      const res = await wrongScope.post('/otlp/spa/v1/traces', {
        data: traceBody('wrong-scope', hex(16), hex(8)),
      });
      // 403, not 401: this is the same `auth_middleware` as every other route,
      // which distinguishes "who are you" from "not allowed".
      expect(res.status()).toBe(403);
    } finally {
      await wrongScope.dispose();
    }
  });

  test('rejects a body over the 1 MiB cap with 413', async ({ request }) => {
    const res = await request.post('/otlp/spa/v1/traces', {
      headers: { 'content-type': 'application/json' },
      data: 'a'.repeat(1024 * 1024 + 1),
    });
    expect(res.status()).toBe(413);
  });

  /**
   * `/otlp/spa/v1/metrics` is the case with teeth: client metrics were dropped
   * from #354, a client could plausibly send them, and the sidecar has no
   * pipeline for them. 404 is the honest answer; silently accepting and
   * discarding them is what this asserts does not happen.
   *
   * The rest matter because the SPA's catch-all serves `index.html` with a
   * **200** for any unrecognised path — so "not 404" here would not be a
   * harmless miss, it would be the SPA's HTML answering an export.
   */
  test('answers anything that is not a known client and signal with 404', async ({ request }) => {
    for (const path of [
      '/otlp/spa/v1/metrics',
      '/otlp/ios/v1/traces',
      '/otlp/v1/traces',
      '/otlp/spa/v2/traces',
      '/otlp/',
    ]) {
      const res = await request.post(path, { data: traceBody('nope', hex(16), hex(8)) });
      expect(res.status(), `POST ${path}`).toBe(404);
    }

    // The same paths by GET, which is how the catch-all would be reached. By
    // body as well as status: the SPA fallback serves index.html *with a 404*,
    // so a status check alone passes on exactly the escape it is looking for —
    // which is how `/otlp/` slipped past the Rust tests until this caught it.
    for (const path of ['/otlp', '/otlp/', '/otlp/spa/v1/traces', '/otlp/anything']) {
      const res = await request.get(path);
      expect(res.ok(), `GET ${path} must not succeed`).toBe(false);
      expect(await res.text(), `GET ${path} must not be answered by the SPA`).not.toContain(
        '<html',
      );
    }
  });
});

// ---------------------------------------------------------------------------
// A real browser
// ---------------------------------------------------------------------------

test.describe('the SPA in a real browser', () => {
  /**
   * The acceptance criterion the whole card exists for: one trace that starts
   * in the browser and reaches Postgres.
   *
   * The trace id is taken from the `traceparent` the SPA puts on its own
   * requests — the real header, read off the real request, rather than anything
   * the test chose. Tempo is then asked for that id, and has to hold both the
   * browser's span and the server's.
   */
  test('a page load produces one trace spanning browser, server and database', async ({
    page,
  }) => {
    const traceparents: string[] = [];
    page.on('request', (req) => {
      const header = req.headers()['traceparent'];
      if (header && req.url().includes('/api/')) {
        traceparents.push(header);
      }
    });

    await page.goto('/', { waitUntil: 'load' });
    await expect(page.locator('summary[aria-label="Open main menu"]')).toBeVisible({
      timeout: 15_000,
    });

    expect(traceparents.length, 'the SPA must send traceparent on its API calls').toBeGreaterThan(
      0,
    );
    // `00-<32 hex>-<16 hex>-01`: version 00, and always sampled — a `-00` here
    // would switch off the server's spans too, since Traefik samples on parent.
    for (const header of traceparents) {
      expect(header).toMatch(/^00-[0-9a-f]{32}-[0-9a-f]{16}-01$/);
    }
    // Every call from one screen shares the screen's trace.
    const traceIds = new Set(traceparents.map((header) => header.split('-')[1]));
    expect(traceIds.size, 'one screen is one trace').toBe(1);
    const traceId = [...traceIds][0];

    // The browser's spans are exported on a tick, so the trace fills in over a
    // few seconds; `eventually` covers both that and Tempo's ingestion.
    const names = await eventually(`browser and server spans in trace ${traceId}`, async () => {
      const res = await backends.get(`${TEMPO_URL}/api/traces/${traceId}`);
      if (!res.ok()) return null;
      const body = await res.json();
      const all = (body?.batches ?? []).flatMap((batch: any) =>
        (batch.scopeSpans ?? []).flatMap((scope: any) =>
          (scope.spans ?? []).map((span: any) => span.name as string),
        ),
      );
      // Wait for the browser's own span, which arrives last.
      return all.includes('http.client') ? all : null;
    }, 45_000);

    // The browser's client span, the server's request span, and the database
    // work underneath it — one trace, three tiers.
    expect(names).toContain('http.client');
    expect(names).toContain('http.request');
    expect(names.some((name) => name.startsWith('db.'))).toBe(true);
  });

  /** The browser's span must be the root, or the trace is not the user's. */
  test('the browser span is the root of the trace', async ({ page }) => {
    let traceparent: string | undefined;
    page.on('request', (req) => {
      traceparent ??= req.headers()['traceparent'];
    });

    await page.goto('/', { waitUntil: 'load' });
    await expect(page.locator('summary[aria-label="Open main menu"]')).toBeVisible({
      timeout: 15_000,
    });
    expect(traceparent).toBeDefined();
    const [, traceId] = traceparent!.split('-');

    const spans = await eventually(`the root span of ${traceId}`, async () => {
      const res = await backends.get(`${TEMPO_URL}/api/traces/${traceId}`);
      if (!res.ok()) return null;
      const body = await res.json();
      const all = (body?.batches ?? []).flatMap((batch: any) => {
        const service = flattenAttributes(batch.resource?.attributes)['service.name'];
        return (batch.scopeSpans ?? []).flatMap((scope: any) =>
          (scope.spans ?? []).map((span: any) => ({
            name: span.name as string,
            parent: (span.parentSpanId ?? '') as string,
            service,
          })),
        );
      });
      return all.some((span: any) => span.service === 'v-note-spa') ? all : null;
    }, 45_000);

    const roots = spans.filter((span: any) => !span.parent);
    expect(roots.length, 'exactly one root').toBe(1);
    expect(roots[0].service).toBe('v-note-spa');
  });

  /**
   * A log the SPA itself decided to write, reaching Loki by the real path —
   * not one posted by the test. Opening a deep link to a page that does not
   * exist is a genuine, reachable failure path, and the SPA logs it at `info`.
   */
  test('a failure the SPA notices reaches Loki, correlated with its trace', async ({ page }) => {
    const missing = `page_does_not_exist_${hex(4)}`;
    await page.goto(`/p/${missing}`, { waitUntil: 'load' });
    // The app recovers: it falls back to the library and says why.
    await expect(page.getByText('That page is not in your library.')).toBeVisible({
      timeout: 15_000,
    });

    const streams = await lokiStreams('{service_name="v-note-spa"}', (line) =>
      line.includes('deep link named a page not in the library'),
    );
    const stream = streams[0].stream;
    expect(stream.severity_text).toBe('INFO');
    expect(stream.deployment_environment).toBe(EXPECTED_ENV);
    // Correlated: the line names the trace it happened in.
    expect(stream.trace_id).toMatch(/^[0-9a-f]{32}$/);
  });

  /**
   * The rule the module is built around: telemetry must never cost the product
   * anything. Asserted the only way that means something — by breaking the
   * collector and checking the app does not care.
   */
  test('the SPA stays fully usable when the sidecar refuses everything', async ({ browser }) => {
    const context = await browser.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
    });
    try {
      const page = await context.newPage();
      // Every export fails, as it would with the sidecar stopped.
      await page.route('**/otlp/**', (route) => route.abort('connectionrefused'));

      await page.goto('/', { waitUntil: 'load' });
      await expect(page.locator('summary[aria-label="Open main menu"]')).toBeVisible({
        timeout: 15_000,
      });
      // Past the first export tick (5s), so failures have actually happened.
      await page.waitForTimeout(7_000);

      // The app is still live: the library renders and the menu still works.
      await page.locator('summary[aria-label="Open main menu"]').click();
      await expect(page.getByRole('link', { name: 'Sign out' })).toBeVisible();
      // And nothing about telemetry reached the user.
      await expect(page.locator('.alert')).toHaveCount(0);
    } finally {
      await context.close();
    }
  });
});
