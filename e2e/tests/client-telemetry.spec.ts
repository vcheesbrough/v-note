import { expect, request, test, type APIRequestContext } from '@playwright/test';
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as zlib from 'node:zlib';

/**
 * #439 — client telemetry through `otlp-collector-oidc`, end to end.
 *
 * Nothing here is mocked. The stack runs the **pinned** ingest image the
 * deployment runs (e2e/docker-compose.test.yml), forwarding to a stand-in for
 * the shared Alloy and to a real Tempo and a real Loki at the digests
 * mini-config runs. Every assertion is about what those backends stored, or
 * about what the ingest itself answered.
 *
 * The image is unproven — v-note is its first real deployment — so each of its
 * behaviours v-note relies on is asserted here against the pinned tag rather
 * than taken from its docs: the refusals and their reasons, identity stamping
 * over a forged payload, the environment and path marker, the bounds on
 * `service.name` and timestamps, client metrics dropped, the decompressed body
 * cap, and the self-metrics the dashboard charts. A gap found here is fixed
 * upstream and re-pinned, never worked around in v-note.
 *
 * What this stack cannot show: the Traefik route that makes the SPA's export
 * same-origin in the deployment (here the ingest is a second origin, admitted
 * by its CORS_ALLOWED_ORIGINS). That is checked on dev.
 */

const INGEST_URL = process.env.INGEST_URL;
const INGEST_METRICS_URL = process.env.INGEST_METRICS_URL;
const TEMPO_URL = process.env.TEMPO_URL;
const LOKI_URL = process.env.LOKI_URL;
const EXPECTED_ENV = process.env.CLIENT_TELEMETRY_EXPECTED_ENV;
const OIDC_TOKEN_URL = process.env.OIDC_TOKEN_URL;

// Loud rather than skipped, for the reason spelled out in sql-console.spec.ts:
// `playwright` hard-depends on these services, so an unset variable is a broken
// stack, and a skip would quietly retire the only proof the pipeline works.
if (!INGEST_URL || !INGEST_METRICS_URL || !TEMPO_URL || !LOKI_URL || !EXPECTED_ENV || !OIDC_TOKEN_URL) {
  throw new Error(
    'INGEST_URL / INGEST_METRICS_URL / TEMPO_URL / LOKI_URL / CLIENT_TELEMETRY_EXPECTED_ENV / OIDC_TOKEN_URL are unset — ' +
      'the telemetry services are part of e2e/docker-compose.test.yml, so this is a broken stack, not an opt-out',
  );
}

/** The mock IdP's identity for the suite's token (`v-note-test`). */
const USER = {
  id: 'v-note-test-service-account',
  name: 'test-user',
  email: 'test@example.com',
  fullName: 'Test User',
};

/** Lowercase hex, as OTLP/JSON requires (not the base64 protobuf would use). */
function hex(bytes: number): string {
  return Array.from({ length: bytes }, () =>
    Math.floor(Math.random() * 256)
      .toString(16)
      .padStart(2, '0'),
  ).join('');
}

const nanos = (ms: number) => `${ms}000000`;
const nowNanos = () => nanos(Date.now());

type KV = { key: string; value: { stringValue: string } };
const kv = (key: string, value: string): KV => ({ key, value: { stringValue: value } });

function traceBody(opts: {
  name: string;
  traceId: string;
  spanId: string;
  resource: KV[];
  attributes?: KV[];
  startMs?: number;
}) {
  const start = opts.startMs ?? Date.now();
  return {
    resourceSpans: [
      {
        resource: { attributes: opts.resource },
        scopeSpans: [
          {
            scope: { name: 'e2e' },
            spans: [
              {
                traceId: opts.traceId,
                spanId: opts.spanId,
                name: opts.name,
                kind: 1,
                startTimeUnixNano: nanos(start),
                endTimeUnixNano: nanos(start + 1),
                attributes: opts.attributes ?? [],
              },
            ],
          },
        ],
      },
    ],
  };
}

function logBody(opts: {
  body: string;
  traceId: string;
  spanId: string;
  resource: KV[];
  attributes?: KV[];
}) {
  return {
    resourceLogs: [
      {
        resource: { attributes: opts.resource },
        scopeLogs: [
          {
            scope: { name: 'e2e' },
            logRecords: [
              {
                timeUnixNano: nowNanos(),
                severityNumber: 13,
                severityText: 'WARN',
                body: { stringValue: opts.body },
                traceId: opts.traceId,
                spanId: opts.spanId,
                attributes: opts.attributes ?? [],
              },
            ],
          },
        ],
      },
    ],
  };
}

/**
 * What a client might claim about itself to pass as something it is not: the
 * environment, the server's own path marker, and an identity. Every one of
 * these must be overwritten or removed by the ingest.
 */
function forgedClaims(): KV[] {
  return [
    kv('deployment.environment.name', 'forged-env'),
    kv('telemetry_source', 'otlp'),
    kv('user.id', 'forged-user-on-resource'),
  ];
}

const forgedIdentityAttributes = (): KV[] => [
  kv('user.id', 'forged-user'),
  kv('user.name', 'forged-name'),
  kv('user.email', 'forged@example.com'),
];

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

let backends: APIRequestContext;
let suiteToken: string;

async function mintToken(clientId: string, tokenUrl = OIDC_TOKEN_URL!): Promise<string> {
  const res = await backends.post(tokenUrl, {
    form: {
      grant_type: 'client_credentials',
      client_id: clientId,
      client_secret: 'test-secret',
      scope: 'openid',
    },
  });
  expect(res.ok(), `mint a token for ${clientId}`).toBe(true);
  return (await res.json()).access_token as string;
}

/** POST straight to the ingest, as a client would. */
async function ingest(
  signal: 'traces' | 'logs' | 'metrics',
  body: unknown,
  token: string | null,
  extraHeaders: Record<string, string> = {},
) {
  const headers: Record<string, string> = { 'Content-Type': 'application/json', ...extraHeaders };
  if (token !== null) headers.Authorization = `Bearer ${token}`;
  return backends.post(`${INGEST_URL}/v1/${signal}`, {
    headers,
    data: typeof body === 'string' || Buffer.isBuffer(body) ? body : JSON.stringify(body),
  });
}

test.beforeAll(async () => {
  // The hook's own limit, which defaults to 30s and would otherwise cut the
  // waits below off before any could time out on its own terms.
  test.setTimeout(180_000);
  // No inherited credentials: the config's `use` hands every context the suite
  // token and cookie, and a request here must carry exactly what it says.
  backends = await request.newContext({
    ignoreHTTPSErrors: true,
    extraHTTPHeaders: {},
    storageState: { cookies: [], origins: [] },
  });
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
  suiteToken = await mintToken('v-note-test');
  // The ingest's HEALTHCHECK says the process is up, not that it has loaded
  // the provider's keys: until it has, every export is `503 not ready`.
  await eventually('the ingest to have loaded the OIDC provider', async () => {
    const res = await ingest('traces', { resourceSpans: [] }, suiteToken);
    return res.status() === 200 ? true : null;
  }, 60_000);
});

test.afterAll(async () => {
  await backends.dispose();
});

/** The batches Tempo stored for a trace, once it has one with `service`. */
async function tempoBatches(traceId: string, service?: string, timeoutMs = 30_000) {
  return eventually(`trace ${traceId} in tempo`, async () => {
    const res = await backends.get(`${TEMPO_URL}/api/traces/${traceId}`);
    if (!res.ok()) return null;
    const batches = ((await res.json())?.batches ?? []) as any[];
    if (batches.length === 0) return null;
    if (
      service &&
      !batches.some(
        (batch) => flattenAttributes(batch.resource?.attributes)['service.name'] === service,
      )
    ) {
      return null;
    }
    return batches;
  }, timeoutMs);
}

async function lokiStreams(query: string, match?: (line: string) => boolean, timeoutMs = 30_000) {
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
    const hit = match ? results.filter((r) => r.values.some(([, line]) => match(line))) : results;
    return hit.length > 0 ? hit : null;
  }, timeoutMs);
}

/** How many traces a TraceQL search finds right now — no polling; for negatives. */
async function tempoSearchCount(traceql: string): Promise<number> {
  const res = await backends.get(`${TEMPO_URL}/api/search`, {
    params: { q: traceql, limit: '100' },
  });
  expect(res.ok(), `tempo search ${traceql}`).toBe(true);
  return (((await res.json()).traces ?? []) as unknown[]).length;
}

/** The ingest's own Prometheus metrics, summed over labels, by name. */
async function ingestMetrics(): Promise<Map<string, number>> {
  const res = await backends.get(INGEST_METRICS_URL!);
  expect(res.ok(), 'the ingest serves its own metrics').toBe(true);
  const totals = new Map<string, number>();
  for (const line of (await res.text()).split('\n')) {
    const match = /^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{[^}]*\})?\s+(\S+)/.exec(line);
    if (!match) continue;
    totals.set(match[1], (totals.get(match[1]) ?? 0) + Number(match[2]));
  }
  return totals;
}

// ---------------------------------------------------------------------------
// The ingest, driven directly (as a simulated Android client, and to prove
// each behaviour of the pinned image)
// ---------------------------------------------------------------------------

test.describe('otlp-collector-oidc (pinned) as v-note deploys it', () => {
  /**
   * Every refusal of a token is `401`, with the failed rule in the body — the
   * client contract's one rule (refresh once, then stop) depends on it.
   */
  test('refuses every bad token with 401 and names the reason', async () => {
    const span = traceBody({
      name: 'refused',
      traceId: hex(16),
      spanId: hex(8),
      resource: [kv('service.name', 'v-note-spa')],
    });
    const cases: Array<[string, string | null, string]> = [
      ['no token', null, 'no token'],
      ['not a JWT', 'not-a-jwt', 'invalid token'],
      // The audience check is what keeps every other application's tokens out:
      // the issuer and the `telemetry:write` mapping are shared estate-wide.
      ['another application\'s token', await mintToken('v-note-other-app-test'), 'invalid token: wrong aud'],
      // Same provider and client, but a different issuer (the mock's second
      // issuer path stands in for another provider).
      [
        'another issuer\'s token',
        await mintToken('v-note-test', OIDC_TOKEN_URL!.replace('/default/', '/other/')),
        'invalid token',
      ],
      ['an expired token', await mintToken('v-note-expired-test'), 'invalid token'],
      ['no telemetry:write', await mintToken('v-note-no-telemetry-test'), 'missing scope: telemetry:write'],
      ['no preferred_username', await mintToken('v-note-no-username-test'), 'missing claim: preferred_username'],
    ];
    for (const [what, token, reason] of cases) {
      const res = await ingest('traces', span, token);
      expect(res.status(), what).toBe(401);
      expect(await res.text(), what).toContain(reason);
    }
  });

  /**
   * The acceptance's identity line, on a span: `user.*` comes from the token,
   * not the payload; the environment and path marker from the deployment; the
   * client's own `service.name` and `service.version` are kept.
   */
  test('stamps identity and environment over a forged SPA span and keeps its build', async () => {
    const traceId = hex(16);
    const name = `forged-spa-span-${hex(4)}`;
    const res = await ingest(
      'traces',
      traceBody({
        name,
        traceId,
        spanId: hex(8),
        resource: [
          kv('service.name', 'v-note-spa'),
          kv('service.version', '1.2.3-e2e-client'),
          ...forgedClaims(),
        ],
        attributes: forgedIdentityAttributes(),
      }),
      suiteToken,
    );
    expect(res.status()).toBe(200);

    const [batch] = await tempoBatches(traceId);
    const resource = flattenAttributes(batch.resource.attributes);
    expect(resource['service.name']).toBe('v-note-spa');
    // The client's own build — the old sidecar stamped the server's here.
    expect(resource['service.version']).toBe('1.2.3-e2e-client');
    expect(resource['deployment.environment.name']).toBe(EXPECTED_ENV);
    expect(resource['telemetry_source']).toBe('client');
    // Identity on the resource is removed, not merely left beside the real one.
    expect(resource['user.id']).toBeUndefined();

    const span = flattenAttributes(batch.scopeSpans[0].spans[0].attributes);
    expect(span['user.id']).toBe(USER.id);
    expect(span['user.name']).toBe(USER.name);
    expect(span['user.email']).toBe(USER.email);
    expect(span['user.full_name']).toBe(USER.fullName);

    // Searchable by the marker, and never under the server's own value.
    await eventually('the span under telemetry_source=client', async () =>
      (await tempoSearchCount(`{ resource.telemetry_source = "client" && name = "${name}" }`)) > 0
        ? true
        : null,
    );
    expect(await tempoSearchCount(`{ resource.telemetry_source = "otlp" && name = "${name}" }`)).toBe(0);
  });

  /** The Android half: a log, as the app sends it, reaching Loki stamped and correlated. */
  test('a simulated Android log reaches Loki attributed to the user, in this environment', async () => {
    const marker = `android-log-${hex(4)}`;
    const traceId = hex(16);
    const spanId = hex(8);
    const res = await ingest(
      'logs',
      logBody({
        body: `android: ${marker}`,
        traceId,
        spanId,
        resource: [
          kv('service.name', 'v-note-android'),
          kv('service.version', '0.58.0-e2e-android'),
          ...forgedClaims(),
        ],
        attributes: forgedIdentityAttributes(),
      }),
      suiteToken,
    );
    expect(res.status()).toBe(200);

    // Selected the way the dashboard's client-logs panel selects: the shared
    // Alloy (the stand-in mirrors mini-config's) turns the ingest's
    // `deployment.environment.name` and `telemetry_source=client` into the
    // indexed `deployment_environment` and `log_source="client"`.
    const streams = await lokiStreams(
      `{service_name="v-note-android", deployment_environment="${EXPECTED_ENV}", log_source="client"}`,
      (line) => line.includes(marker),
    );
    const stream = streams[0].stream;
    expect(stream.deployment_environment_name).toBe(EXPECTED_ENV);
    expect(stream.service_version).toBe('0.58.0-e2e-android');
    expect(stream.telemetry_source).toBe('client');
    expect(stream.user_id).toBe(USER.id);
    expect(stream.user_name).toBe(USER.name);
    // Trace correlation is the point of shipping logs over OTLP: this is what
    // Grafana turns into a link from the log line to the trace.
    expect(stream.trace_id).toBe(traceId);
    expect(stream.span_id).toBe(spanId);
    // …and never stored under the environment the client claimed.
    const forged = await backends.get(`${LOKI_URL}/loki/api/v1/query_range`, {
      params: {
        query: `{deployment_environment_name="forged-env"} |= "${marker}"`,
        start: `${(Date.now() - 15 * 60 * 1000) * 1e6}`,
        end: `${Date.now() * 1e6}`,
      },
    });
    expect(((await forged.json()).data?.result ?? []).length).toBe(0);
  });

  /**
   * The bounds: a `service.name` outside ALLOWED_SERVICE_NAMES and a span older
   * than MAX_PAST_AGE are answered 200 and dropped, and client metrics are
   * dropped (the allowlists are empty). "Dropped" is asserted the only way it
   * can be — the item is absent after a later sentinel has arrived — and
   * "counted" on the ingest's own metrics, which the dashboard charts.
   */
  test('drops and counts an unregistered service, an ancient span and client metrics', async () => {
    const before = await ingestMetrics();
    const stranger = `stranger-${hex(4)}`;
    const ancient = `ancient-${hex(4)}`;

    expect(
      (
        await ingest(
          'traces',
          traceBody({ name: stranger, traceId: hex(16), spanId: hex(8), resource: [kv('service.name', 'totally-not-v-note')] }),
          suiteToken,
        )
      ).status(),
    ).toBe(200);
    expect(
      (
        await ingest(
          'logs',
          logBody({ body: stranger, traceId: hex(16), spanId: hex(8), resource: [kv('service.name', 'totally-not-v-note')] }),
          suiteToken,
        )
      ).status(),
    ).toBe(200);
    expect(
      (
        await ingest(
          'traces',
          traceBody({
            name: ancient,
            traceId: hex(16),
            spanId: hex(8),
            resource: [kv('service.name', 'v-note-spa')],
            startMs: Date.now() - 72 * 3600 * 1000,
          }),
          suiteToken,
        )
      ).status(),
    ).toBe(200);
    expect(
      (
        await ingest(
          'metrics',
          {
            resourceMetrics: [
              {
                resource: { attributes: [kv('service.name', 'v-note-spa')] },
                scopeMetrics: [
                  { metrics: [{ name: 'vnote.client.e2e', gauge: { dataPoints: [{ asDouble: 1, timeUnixNano: nowNanos() }] } }] },
                ],
              },
            ],
          },
          suiteToken,
        )
      ).status(),
    ).toBe(200);

    // A sentinel after them all: once it is stored, anything that was going to
    // be stored has been.
    const sentinel = hex(16);
    await ingest(
      'traces',
      traceBody({ name: 'sentinel', traceId: sentinel, spanId: hex(8), resource: [kv('service.name', 'v-note-spa')] }),
      suiteToken,
    );
    await tempoBatches(sentinel);
    expect(await tempoSearchCount(`{ name = "${stranger}" }`)).toBe(0);
    expect(await tempoSearchCount(`{ name = "${ancient}" }`)).toBe(0);

    const after = await eventually('the drops to be counted', async () => {
      const now = await ingestMetrics();
      const grew = (name: string, by: number) => (now.get(name) ?? 0) - (before.get(name) ?? 0) >= by;
      return grew('otelcol_processor_filter_spans_filtered', 2) &&
        grew('otelcol_processor_filter_logs_filtered', 1) &&
        grew('otelcol_processor_filter_datapoints_filtered', 1)
        ? now
        : null;
    });
    expect(after.get('otelcol_processor_filter_spans_filtered')).toBeGreaterThan(0);
  });

  /** A gzip bomb is refused on its *decompressed* size, before it is parsed. */
  test('caps the decompressed body with 413', async () => {
    const bomb = zlib.gzipSync(Buffer.alloc(5 * 1024 * 1024, ' '));
    expect(bomb.length).toBeLessThan(64 * 1024);
    const res = await ingest('traces', bomb, suiteToken, { 'Content-Encoding': 'gzip' });
    expect(res.status()).toBe(413);
  });

  /**
   * The dashboard and the alert query these names (deploy/grafana/). They are
   * listed once, in deploy/grafana/otlp-collector-oidc-metrics.txt, which
   * scripts/test-grafana-dashboard.sh holds the dashboard to — and here the
   * pinned image is held to the list, so a rename upstream fails a test rather
   * than blanking a panel. Runs after the tests above have driven every event
   * the counters count.
   */
  test('exports every metric the dashboard and alert query', async () => {
    const listed = fs
      .readFileSync(path.resolve(__dirname, '../fixtures/otlp-collector-oidc-metrics.txt'), 'utf8')
      .split('\n')
      .map((line) => line.trim())
      .filter((line) => line && !line.startsWith('#'));
    expect(listed.length).toBeGreaterThan(5);
    const exported = await ingestMetrics();
    for (const name of listed) {
      expect(exported.has(name), `${name} exported by the pinned image`).toBe(true);
    }
  });
});

// ---------------------------------------------------------------------------
// The configuration route
// ---------------------------------------------------------------------------

test.describe('GET /api/telemetry/config', () => {
  test('hands a signed-in client the endpoint and its own token, uncached', async ({ request }) => {
    const res = await request.get('/api/telemetry/config');
    expect(res.status()).toBe(200);
    expect(res.headers()['cache-control']).toBe('no-store');
    const config = await res.json();
    expect(config.endpoint).toBe(INGEST_URL);
    expect(config.access_token).toBe(process.env.AUTH_TOKEN);
    expect(config.expires_at).toBeGreaterThan(Date.now() / 1000);
  });

  test('gives an anonymous caller nothing', async () => {
    const anonymous = await request.newContext({
      baseURL: process.env.BASE_URL,
      ignoreHTTPSErrors: true,
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: {},
    });
    try {
      const res = await anonymous.get('/api/telemetry/config');
      expect(res.status()).toBe(401);
      expect(await res.text()).not.toContain('endpoint');
    } finally {
      await anonymous.dispose();
    }
  });
});

// ---------------------------------------------------------------------------
// A real browser
// ---------------------------------------------------------------------------

const MENU = 'summary[aria-label="Open main menu"]';
const isExport = (url: string) => url.startsWith(`${INGEST_URL}/v1/`);

test.describe('the SPA in a real browser', () => {
  /**
   * The acceptance criterion #354 exists for, now through the ingest: one
   * trace that starts in the browser and reaches Postgres.
   */
  test('a page load produces one trace spanning browser, server and database', async ({ page }) => {
    // The browser exports on a 5 s tick, then Tempo or Loki ingest it.
    test.setTimeout(90_000);
    const traceparents: string[] = [];
    page.on('request', (req) => {
      const header = req.headers()['traceparent'];
      if (header && req.url().includes('/api/')) {
        traceparents.push(header);
      }
    });

    await page.goto('/', { waitUntil: 'load' });
    await expect(page.locator(MENU)).toBeVisible({ timeout: 15_000 });

    expect(traceparents.length, 'the SPA must send traceparent on its API calls').toBeGreaterThan(0);
    for (const header of traceparents) {
      expect(header).toMatch(/^00-[0-9a-f]{32}-[0-9a-f]{16}-01$/);
    }
    const traceIds = new Set(traceparents.map((header) => header.split('-')[1]));
    expect(traceIds.size, 'one screen is one trace').toBe(1);
    const traceId = [...traceIds][0];

    const batches = await tempoBatches(traceId, 'v-note-spa', 45_000);
    const names = batches.flatMap((batch: any) =>
      (batch.scopeSpans ?? []).flatMap((scope: any) => (scope.spans ?? []).map((span: any) => span.name as string)),
    );
    expect(names).toContain('http.client');
    expect(names).toContain('http.request');
    expect(names.some((name: string) => name.startsWith('db.'))).toBe(true);

    // The browser's spans carry the user the ingest stamped from the token.
    const spa = batches.find(
      (batch: any) => flattenAttributes(batch.resource?.attributes)['service.name'] === 'v-note-spa',
    );
    const resource = flattenAttributes(spa.resource.attributes);
    expect(resource['telemetry_source']).toBe('client');
    expect(resource['deployment.environment.name']).toBe(EXPECTED_ENV);
    const attributes = flattenAttributes(spa.scopeSpans[0].spans[0].attributes);
    expect(attributes['user.name']).toBe(USER.name);
  });

  /** The browser's span must be the root, or the trace is not the user's. */
  test('the browser span is the root of the trace', async ({ page }) => {
    // The browser exports on a 5 s tick, then Tempo or Loki ingest it.
    test.setTimeout(90_000);
    let traceparent: string | undefined;
    page.on('request', (req) => {
      traceparent ??= req.headers()['traceparent'];
    });

    await page.goto('/', { waitUntil: 'load' });
    await expect(page.locator(MENU)).toBeVisible({ timeout: 15_000 });
    expect(traceparent).toBeDefined();
    const [, traceId] = traceparent!.split('-');

    const batches = await tempoBatches(traceId, 'v-note-spa', 45_000);
    const spans = batches.flatMap((batch: any) => {
      const service = flattenAttributes(batch.resource?.attributes)['service.name'];
      return (batch.scopeSpans ?? []).flatMap((scope: any) =>
        (scope.spans ?? []).map((span: any) => ({
          name: span.name as string,
          id: span.spanId as string,
          parent: (span.parentSpanId ?? '') as string,
          service,
        })),
      );
    });

    const roots = spans.filter((span: any) => !span.parent);
    expect(roots.length, 'exactly one root').toBe(1);
    expect(roots[0].service).toBe('v-note-spa');
    const rootId = roots[0].id;
    const browserRequests = spans.filter(
      (span: any) => span.service === 'v-note-spa' && span.name === 'http.client',
    );
    expect(browserRequests.length, 'the page load makes several requests').toBeGreaterThan(1);
    for (const span of browserRequests) {
      expect(span.parent, `${span.name} ${span.id}`).toBe(rootId);
    }
  });

  /**
   * A log the SPA itself decided to write, reaching Loki by the real path, as
   * this build, attributed to the signed-in user.
   */
  test('a failure the SPA notices reaches Loki, attributed and correlated', async ({ page, request }) => {
    // The browser exports on a 5 s tick, then Tempo or Loki ingest it.
    test.setTimeout(90_000);
    const version = (await (await request.get('/api/meta')).json()).app_version as string;
    const missing = `page_does_not_exist_${hex(4)}`;
    await page.goto(`/p/${missing}`, { waitUntil: 'load' });
    await expect(page.getByText('That page is not in your library.')).toBeVisible({ timeout: 15_000 });

    const streams = await lokiStreams(
      `{service_name="v-note-spa", deployment_environment_name="${EXPECTED_ENV}"}`,
      (line) => line.includes('deep link named a page not in the library'),
    );
    const stream = streams[0].stream;
    expect(stream.severity_text).toBe('INFO');
    expect(stream.telemetry_source).toBe('client');
    expect(stream.user_name).toBe(USER.name);
    expect(stream.user_id).toBe(USER.id);
    // The SPA's own build, which is the server's here — the same image.
    expect(stream.service_version).toBe(version);
    expect(stream.trace_id).toMatch(/^[0-9a-f]{32}$/);
  });

  /**
   * The credential: the bearer the config route handed out, per request, and
   * never the session cookie — the ingest accepts only a bearer, and the
   * cookie has no business reaching it.
   */
  test('exports carry the bearer from the config route, and no cookie', async ({ page }) => {
    const exports: Array<Record<string, string>> = [];
    page.on('request', (req) => {
      if (isExport(req.url()) && req.method() === 'POST') exports.push(req.headers());
    });
    await page.goto('/', { waitUntil: 'load' });
    await expect(page.locator(MENU)).toBeVisible({ timeout: 15_000 });
    await expect.poll(() => exports.length, { timeout: 15_000 }).toBeGreaterThan(0);
    expect(exports[0]['authorization']).toBe(`Bearer ${process.env.AUTH_TOKEN}`);
    expect(exports[0]['cookie']).toBeUndefined();
    expect(exports[0]['content-type']).toContain('application/json');
  });

  /**
   * A browser cannot put a `traceparent` on a WebSocket upgrade, so the server
   * stores the trace of the `POST /api/realtime-ticket` request with the ticket
   * and parents the connection span to it when the ticket is redeemed.
   */
  test('each realtime connection span sits in the trace its ticket was requested in', async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const created = await request.post('/api/pages', { data: {} });
    expect(created.status()).toBe(201);
    const id: string = (await created.json()).page.id;

    const ticketTraces = new Set<string>();
    page.on('request', (req) => {
      if (req.method() === 'POST' && req.url().endsWith('/api/realtime-ticket')) {
        const header = req.headers()['traceparent'];
        if (header) ticketTraces.add(header.split('-')[1]);
      }
    });

    await page.goto(`/p/${id}`, { waitUntil: 'load' });
    await expect(page.getByLabel('Read-only ink canvas')).toBeVisible({ timeout: 15_000 });
    await expect(page.getByText(/Synced · seq 0|Connected · seq 0|Live · seq 0/)).toBeVisible({
      timeout: 15_000,
    });
    await expect.poll(() => ticketTraces.size, { timeout: 15_000 }).toBeGreaterThanOrEqual(2);
    await page.goto('about:blank');

    const found = await eventually('connection spans in the ticket traces', async () => {
      const names = new Set<string>();
      for (const traceId of ticketTraces) {
        const res = await backends.get(`${TEMPO_URL}/api/traces/${traceId}`);
        if (!res.ok()) continue;
        const body = await res.json();
        for (const batch of body?.batches ?? []) {
          for (const scope of batch.scopeSpans ?? []) {
            for (const span of scope.spans ?? []) names.add(span.name);
          }
        }
      }
      return names.has('handle_page_socket') && names.has('handle_library_socket') ? names : null;
    }, 45_000);

    expect(found.has('handle_page_socket')).toBe(true);
    expect(found.has('handle_library_socket')).toBe(true);
  });

  /**
   * An ordinary `fetch` started during `pagehide` is cancelled by the unload;
   * `sendBeacon` survives but cannot carry the bearer the ingest needs. So the
   * flush is `fetch(…, { keepalive: true })` with the bearer, and this asserts
   * it was *delivered* after the page that sent it was gone.
   */
  test('telemetry queued when the page goes away still arrives', async ({ page }) => {
    // The browser exports on a 5 s tick, then Tempo or Loki ingest it.
    test.setTimeout(90_000);
    let traceId: string | undefined;
    let configured = false;
    const exports: Array<Record<string, string>> = [];
    page.on('request', (req) => {
      const header = req.headers()['traceparent'];
      if (header && req.url().includes('/api/')) traceId ??= header.split('-')[1];
      if (req.url().endsWith('/api/telemetry/config')) configured = true;
      if (isExport(req.url()) && req.url().endsWith('/v1/traces') && req.method() === 'POST') {
        exports.push(req.headers());
      }
    });

    await page.goto('/', { waitUntil: 'load' });
    await expect(page.locator(MENU)).toBeVisible({ timeout: 15_000 });
    await expect.poll(() => configured, { timeout: 10_000 }).toBe(true);
    // Let the config response land, well before the first 5s tick.
    await page.waitForTimeout(1_000);
    // If an ordinary export has already gone, the spans below could have
    // arrived that way and this would prove nothing: inconclusive, not a pass.
    expect(exports, 'an ordinary export ran before the page was left').toHaveLength(0);
    await page.goto('about:blank');

    await expect.poll(() => exports.length, { timeout: 10_000 }).toBeGreaterThan(0);
    expect(exports[0]['authorization']).toBe(`Bearer ${process.env.AUTH_TOKEN}`);
    expect(traceId).toBeDefined();

    // Delivered, not merely sent: the page that sent it no longer exists.
    await tempoBatches(traceId!, 'v-note-spa');
  });

  /**
   * Telemetry must never cost the product anything: break the ingest and check
   * the app does not care.
   */
  test('the SPA stays fully usable when the ingest refuses everything', async ({ browser }) => {
    const context = await browser.newContext({ baseURL: process.env.BASE_URL, ignoreHTTPSErrors: true });
    try {
      const page = await context.newPage();
      await page.route(`${INGEST_URL}/**`, (route) => route.abort('connectionrefused'));

      await page.goto('/', { waitUntil: 'load' });
      await expect(page.locator(MENU)).toBeVisible({ timeout: 15_000 });
      await page.waitForTimeout(7_000);

      await page.locator(MENU).click();
      await expect(page.getByRole('link', { name: 'Sign out' })).toBeVisible();
      await expect(page.locator('.alert')).toHaveCount(0);
    } finally {
      await context.close();
    }
  });

  /**
   * No configuration, no telemetry: with the config route saying "off" the SPA
   * never initialises OTLP — not one request to the ingest — and says so once
   * on the console.
   */
  test('with no telemetry configuration the SPA sends nothing', async ({ browser }) => {
    const context = await browser.newContext({ baseURL: process.env.BASE_URL, ignoreHTTPSErrors: true });
    try {
      const page = await context.newPage();
      const console: string[] = [];
      page.on('console', (message) => console.push(message.text()));
      let exports = 0;
      page.on('request', (req) => {
        if (isExport(req.url())) exports += 1;
      });
      await page.route('**/api/telemetry/config', (route) => route.fulfill({ status: 204 }));

      await page.goto('/', { waitUntil: 'load' });
      await expect(page.locator(MENU)).toBeVisible({ timeout: 15_000 });
      await expect
        .poll(() => console.filter((line) => line.includes('client telemetry off for this page')).length, {
          timeout: 10_000,
        })
        .toBe(1);
      // Past two export ticks.
      await page.waitForTimeout(11_000);
      expect(exports).toBe(0);
      expect(console.filter((line) => line.includes('client telemetry off')).length, 'said once').toBe(1);
    } finally {
      await context.close();
    }
  });

  /**
   * `401` → one refresh (a fresh config fetch) → a second `401` stops telemetry
   * for the page load: no loop, and one console line saying so.
   */
  test('a refused token is refreshed once, then telemetry stops for the page', async ({ browser }) => {
    test.setTimeout(90_000);
    const context = await browser.newContext({ baseURL: process.env.BASE_URL, ignoreHTTPSErrors: true });
    try {
      const page = await context.newPage();
      const console: string[] = [];
      page.on('console', (message) => console.push(message.text()));
      let configFetches = 0;
      let exports = 0;
      page.on('request', (req) => {
        if (req.url().endsWith('/api/telemetry/config')) configFetches += 1;
      });
      await page.route(`${INGEST_URL}/**`, async (route) => {
        // The export is cross-origin in this stack, so the browser may ask
        // first; answer the preflight as the ingest's CORS config would.
        if (route.request().method() === 'OPTIONS') {
          await route.fulfill({
            status: 204,
            headers: {
              'access-control-allow-origin': 'https://app',
              'access-control-allow-methods': 'POST',
              'access-control-allow-headers': 'authorization,content-type',
            },
          });
          return;
        }
        if (route.request().method() === 'POST') exports += 1;
        await route.fulfill({
          status: 401,
          contentType: 'application/json',
          headers: { 'access-control-allow-origin': 'https://app' },
          body: '{"code":16,"message":"missing scope: telemetry:write"}',
        });
      });

      await page.goto('/', { waitUntil: 'load' });
      await expect(page.locator(MENU)).toBeVisible({ timeout: 15_000 });
      await expect
        .poll(() => console.some((line) => line.includes('client telemetry off for this page')), {
          timeout: 40_000,
        })
        .toBe(true);
      expect(configFetches, 'the first fetch and exactly one refresh').toBe(2);
      expect(exports, 'one export per token').toBe(2);

      // Stopped: nothing more, however long the page stays open.
      await page.waitForTimeout(11_000);
      expect(exports).toBe(2);
      expect(configFetches).toBe(2);
      expect(console.filter((line) => line.includes('export failing')).length, 'one line when failing starts').toBe(1);
    } finally {
      await context.close();
    }
  });
});
