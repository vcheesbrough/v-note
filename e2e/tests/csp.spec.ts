import { expect, test } from '../csp-guard';
import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import * as path from 'node:path';

// #444: since #439 the SPA holds an OIDC access token in JavaScript, so the app
// serves the SPA with a Content-Security-Policy that admits only its own script
// and only the endpoints the page talks to. The guard in ../csp-guard.ts fails
// any spec whose page trips the policy; this file checks the policy itself.

function directives(header: string): Map<string, string[]> {
  return new Map(
    header
      .split(';')
      .map((part) => part.trim().split(/\s+/))
      .filter((tokens) => tokens[0])
      .map(([name, ...sources]) => [name, sources]),
  );
}

function inlineScriptHashes(html: string): string[] {
  return [...html.matchAll(/<script(\s[^>]*)?>([\s\S]*?)<\/script>/gi)]
    .filter(([, attributes]) => !/\ssrc\s*=/i.test(attributes ?? ''))
    .map(([, , body]) => `'sha256-${createHash('sha256').update(body, 'utf8').digest('base64')}'`);
}

test('the SPA document, its deep links and its assets carry the policy', async ({ request }) => {
  const index = await request.get('/');
  expect(index.status()).toBe(200);
  const header = index.headers()['content-security-policy'];
  expect(header, 'Content-Security-Policy on /').toBeTruthy();

  const policy = directives(header);
  expect(policy.get('default-src')).toEqual(["'self'"]);
  expect(policy.get('object-src')).toEqual(["'none'"]);
  expect(policy.get('base-uri')).toEqual(["'none'"]);
  expect(policy.get('frame-ancestors')).toEqual(["'self'"]);

  // Script: the app's own files, wasm compilation, and exactly the inline
  // bootstrap Trunk wrote into this build's index.html — never blanket inline
  // script or JavaScript eval.
  const scriptSrc = policy.get('script-src') ?? [];
  const bootstrap = inlineScriptHashes(await index.text());
  expect(bootstrap.length, 'Trunk writes one inline bootstrap').toBe(1);
  expect(scriptSrc).toEqual(["'self'", "'wasm-unsafe-eval'", ...bootstrap]);

  // The e2e ingest is cross-origin, so its origin — derived by the app from the
  // same `client-telemetry.endpoint` it hands the SPA — is allowed besides 'self'.
  const ingest = process.env.INGEST_URL;
  expect(ingest, 'INGEST_URL').toBeTruthy();
  expect(policy.get('connect-src')).toEqual(["'self'", new URL(ingest!).origin]);

  // A deep link is served index.html by the fallback, and assets are served by
  // the same static service: both carry the same policy.
  const deepLink = await request.get('/pages/00000000-0000-0000-0000-000000000000');
  expect(deepLink.headers()['content-security-policy']).toBe(header);
  const stylesheet = (await index.text()).match(/href="(\/styles-[^"]+\.css)"/)?.[1];
  expect(stylesheet, 'hashed stylesheet link').toBeTruthy();
  const asset = await request.get(stylesheet!);
  expect(asset.status()).toBe(200);
  expect(asset.headers()['content-security-policy']).toBe(header);
});

test('the SPA boots under the policy and injected inline script is refused', async ({
  page,
  cspViolations,
}) => {
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'v-note' })).toBeVisible();
  // Set by Trunk's inline bootstrap: proves the hash admitted it.
  expect(await page.evaluate(() => typeof (window as any).wasmBindings)).toBe('object');
  expect(cspViolations).toEqual([]);

  // What the policy is for: script that finds its way into the page does not run.
  await page.evaluate(() => {
    const script = document.createElement('script');
    script.textContent = 'window.__injected = true;';
    document.head.appendChild(script);
  });
  // One refusal reaches the guard twice, from the violation event and from the
  // console, on independent channels. Wait for both before clearing, or the
  // later one lands after the clear and fails this test in teardown.
  await expect
    .poll(() => ({
      event: cspViolations.some((report) => !report.startsWith('console:')),
      console: cspViolations.some((report) => report.startsWith('console:')),
    }))
    .toEqual({ event: true, console: true });
  expect(await page.evaluate(() => (window as any).__injected)).toBeUndefined();
  expect(cspViolations.join('\n')).toMatch(/script-src/);

  // Expected here, so it must not fail this test in the guard's teardown —
  // which is also what shows the guard sees a real refusal.
  cspViolations.length = 0;
});

// The guard only protects specs that use it.
test('every spec runs under the CSP guard', () => {
  const specs = fs.readdirSync(__dirname).filter((name) => name.endsWith('.spec.ts'));
  expect(specs.length).toBeGreaterThan(1);
  for (const spec of specs) {
    const source = fs.readFileSync(path.join(__dirname, spec), 'utf8');
    expect(source, spec).not.toMatch(/from ['"]@playwright\/test['"]/);
    expect(source, spec).toMatch(/from ['"]\.\.\/csp-guard['"]/);
  }
});
