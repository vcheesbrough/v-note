import { expect, test } from '@playwright/test';

test('health endpoint returns ok', async ({ request }) => {
  const res = await request.get('/health');
  expect(res.ok()).toBeTruthy();
  expect(await res.json()).toEqual({ status: 'ok' });
});

test('metrics endpoint exposes Prometheus scrape output', async ({ request }) => {
  const res = await request.get('http://app:9090/metrics');
  expect(res.ok()).toBeTruthy();
  expect(res.headers()['content-type']).toContain('text/plain');

  const body = await res.text();
  expect(body).toContain('v_note_build_info');
  expect(body).toContain('v_note_http_requests_total');
});

test('request id is echoed as correlation metadata', async ({ request }) => {
  const res = await request.get('/health', {
    headers: { 'X-Request-Id': 'e2e-request-198' },
  });
  expect(res.ok()).toBeTruthy();
  expect(res.headers()['x-request-id']).toBe('e2e-request-198');
  expect(res.headers()['x-correlation-id']).toBe('e2e-request-198');
});
