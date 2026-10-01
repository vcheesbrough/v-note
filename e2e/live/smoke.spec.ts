import { expect, test, type BrowserContext, type Page } from '../csp-guard';
import { isAbandonedSmokePage, smokeTitle } from './leftovers';

// Live post-deploy smoke (#179). Runs against a DEPLOYED host through the REAL
// IdP — config playwright.live.config.ts, driver scripts/smoke-web-live.sh,
// pipeline steps smoke-web-live-* in .woodpecker/verify-tag-deploy.yml.
//
// One test, in order, because each step needs the session the previous one
// built and the point is the whole journey a user makes: sign in → /api/me →
// create a page → commit a stroke → both survive a reload → delete → sign out.
//
// The account is the blueprint's dedicated smoke user (authentik/blueprint-dev.yaml):
// its pages are disposable. The page this creates is deleted before sign-out,
// and in `finally` if the test fails first. `live-smoke-*` leftovers of a run
// that was killed outright are swept at the start — only once they are old
// enough that no overlapping run can still be using them (./leftovers.ts):
// several PRs deploy dev, so smoke runs do overlap.
//
// Nothing here may print the password: no logging of env, and the config keeps
// tracing off because a trace records every `fill`.

const username = required('V_NOTE_SMOKE_USERNAME');
const password = required('V_NOTE_SMOKE_PASSWORD');
const expectedEmail = required('V_NOTE_SMOKE_EMAIL');
const baseURL = required('BASE_URL');
const appHost = new URL(baseURL).hostname;

test('a real user signs in, inks a page that persists, and signs out', async ({ page, context }) => {
  let pageId: string | null = null;
  let deleted = false;
  try {
    await test.step('sign in through the real IdP', async () => {
      await signIn(page);
      await expect(page.locator('summary[aria-label="Open main menu"]')).toBeVisible({ timeout: 15_000 });
    });

    await test.step('GET /api/me returns the smoke user', async () => {
      const me = await context.request.get('/api/me');
      expect(me.status(), '/api/me with the session the real login set').toBe(200);
      const body = await me.json();
      expect(typeof body.sub === 'string' && body.sub.length > 0, 'a subject').toBeTruthy();
      expect(body.email, 'the identity is the blueprint smoke user').toBe(expectedEmail);
    });

    await test.step('sweep pages abandoned by a killed run', async () => {
      const now = Date.now();
      for (const leftover of await listPages(context)) {
        if (isAbandonedSmokePage(leftover.title, now)) {
          const res = await context.request.delete(`/api/pages/${leftover.id}`);
          expect([204, 404]).toContain(res.status());
        }
      }
    });

    const title = smokeTitle(Date.now(), Math.random().toString(16).slice(2, 10));
    await test.step('create a page; it is in the library after a reload', async () => {
      const created = await context.request.post('/api/pages', { data: { title } });
      expect(created.status()).toBe(201);
      pageId = (await created.json()).page.id as string;

      await page.reload({ waitUntil: 'load' });
      await expect(page.getByRole('button', { name: `Open ${title}`, exact: true })).toBeVisible({
        timeout: 15_000,
      });
    });

    const strokeId = `stroke-${crypto.randomUUID()}`;
    await test.step('commit a stroke over the page channel', async () => {
      const ticket = await realtimeTicket(context);
      const result = await driveSocket(page, pageId!, ticket, [
        { type: 'subscribe', from_seq: 0 },
        { type: 'acquire-lease' },
        { type: 'commit-batch', client_batch_id: 'live-smoke-batch', strokes: stroke(strokeId) },
      ]);
      expect(result.opened, 'the page channel WebSocket opened').toBeTruthy();
      expect(result.messages.some((m) => m.type === 'lease-granted'), 'edit lease granted').toBeTruthy();
      const echoed = result.messages.find(
        (m) => m.type === 'stroke-batch' && m.client_batch_id === 'live-smoke-batch',
      );
      expect(echoed, `committed batch echoed with a seq; got ${summarise(result.messages)}`).toBeTruthy();
      expect(echoed.seq).toBeGreaterThanOrEqual(1);
    });

    await test.step('the stroke survives a reload (server round-trip)', async () => {
      await page.reload({ waitUntil: 'load' });
      await page.getByRole('button', { name: `Open ${title}`, exact: true }).click();
      await expect(page.getByLabel('Read-only ink canvas')).toBeVisible({ timeout: 15_000 });
      await expect(page.getByText(/(Synced|Live|Connected) · seq 1\b/)).toBeVisible({ timeout: 15_000 });
      await expect
        .poll(() => inkedPixels(page), { timeout: 10_000, message: 'the stroke is drawn on the canvas' })
        .toBeGreaterThan(5);

      // And a fresh subscription replays it from storage, not from the SPA's memory.
      const ticket = await realtimeTicket(context);
      const replay = await driveSocket(page, pageId!, ticket, [{ type: 'subscribe', from_seq: 0 }]);
      expect(replayedStrokeIds(replay.messages)).toContain(strokeId);
    });

    await test.step('delete the page', async () => {
      const res = await context.request.delete(`/api/pages/${pageId}`);
      expect(res.status()).toBe(204);
      deleted = true;
      expect((await listPages(context)).some((p) => p.id === pageId)).toBe(false);
    });

    await test.step('sign out ends the session', async () => {
      await page.goto('/', { waitUntil: 'load' });
      await page.locator('summary[aria-label="Open main menu"]').click();
      // The logout response is what clears the cookie; where it redirects next
      // (the IdP's end-session page) is the IdP's business.
      const [logout] = await Promise.all([
        page.waitForResponse((res) => new URL(res.url()).pathname === '/auth/logout', { timeout: 30_000 }),
        page.getByRole('link', { name: 'Sign out' }).click(),
      ]);
      expect(logout.status(), '/auth/logout redirects').toBe(303);
      const session = (await context.cookies(baseURL)).find((c) => c.name === 'auth' && c.value !== '');
      expect(session, 'the session cookie is cleared').toBeFalsy();
      const me = await context.request.get('/api/me');
      expect(me.status(), '/api/me after sign-out').toBe(401);
    });
  } finally {
    // Cleanup on failure. Best effort: the assertion that failed is the one to
    // report, not a cleanup error on top of it.
    if (pageId && !deleted) {
      await context.request.delete(`/api/pages/${pageId}`).catch(() => undefined);
    }
  }
});

function required(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required for the live smoke`);
  }
  return value;
}

function onApp(page: Page): boolean {
  const url = new URL(page.url());
  return url.hostname === appHost && !url.pathname.startsWith('/auth/');
}

// Authentik's default flow is two stages: identification (`uidField`), then
// password. Both are web components with open shadow roots, which Playwright's
// CSS locators pierce. A flow that asks for anything else (MFA, consent, a
// password change) never reaches the app, and the poll below says where it
// stopped. An IdP that signs in without a form (the local mock stack, see
// docs/DEPLOY.md) is already back on the app after the first navigation.
async function signIn(page: Page): Promise<void> {
  await page.goto('/auth/login', { waitUntil: 'load' });
  if (!onApp(page)) {
    const uid = page.locator('input[name="uidField"]');
    await uid.waitFor({ state: 'visible', timeout: 30_000 });
    await uid.fill(username);
    await uid.press('Enter');

    const secret = page.locator('input[name="password"]');
    await secret.waitFor({ state: 'visible', timeout: 30_000 });
    await secret.fill(password);
    await secret.press('Enter');
  }
  await expect
    .poll(() => onApp(page), {
      timeout: 60_000,
      message: 'the login flow returns to the app',
    })
    .toBe(true)
    .catch(async (error: Error) => {
      // Host and path only: the query of an authorize or flow URL carries state.
      const url = new URL(page.url());
      const stages = await currentFlowStages(page).catch(() => []);
      throw new Error(
        `${error.message}\nsign-in stopped at ${url.origin}${url.pathname}` +
          (stages.length ? ` in stage ${stages.join(' > ')}` : ''),
      );
    });
}

// The authentik stage components on screen (`ak-stage-password` = rejected
// password, `ak-stage-authenticator-validate` = MFA, …), through shadow roots:
// the one fact that tells a broken credential from a changed flow.
async function currentFlowStages(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const found: string[] = [];
    const walk = (root: Document | ShadowRoot) => {
      for (const el of Array.from(root.querySelectorAll('*'))) {
        const tag = el.tagName.toLowerCase();
        if (tag.startsWith('ak-stage-') && !found.includes(tag)) found.push(tag);
        if (el.shadowRoot) walk(el.shadowRoot);
      }
    };
    walk(document);
    return found;
  });
}

async function listPages(context: BrowserContext): Promise<Array<{ id: string; title: string }>> {
  const res = await context.request.get('/api/pages');
  expect(res.status()).toBe(200);
  return (await res.json()).pages;
}

async function realtimeTicket(context: BrowserContext): Promise<string> {
  const res = await context.request.post('/api/realtime-ticket');
  expect(res.status()).toBe(200);
  return (await res.json()).ticket;
}

function stroke(id: string) {
  // The dark green e2e/tests/ink.spec.ts looks for, wide enough to be visible
  // at any viewport.
  return [
    {
      id,
      style: {
        tool_kind: 'solid_round',
        style_version: 2,
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

// Drives the page channel from inside the page, so the socket carries the
// browser's own origin and the ticket the session minted — the SPA's auth path.
async function driveSocket(
  page: Page,
  pageId: string,
  ticket: string,
  messages: object[],
): Promise<{ opened: boolean; messages: any[] }> {
  return page.evaluate(
    async ({ pageId, ticket, messages }) => {
      const url = `${location.origin.replace(/^http/, 'ws')}/api/pages/${pageId}/realtime?ticket=${encodeURIComponent(ticket)}`;
      const ws = new WebSocket(url);
      const received: any[] = [];
      ws.addEventListener('message', (event) => {
        try {
          received.push(JSON.parse(event.data));
        } catch {
          /* ignore non-JSON frames */
        }
      });
      const opened = await new Promise<boolean>((resolve) => {
        ws.addEventListener('open', () => resolve(true));
        ws.addEventListener('error', () => resolve(false));
        setTimeout(() => resolve(false), 10_000);
      });
      if (opened) {
        for (const message of messages) {
          // Generous against a real network: the server answers each in turn.
          await new Promise((resolve) => setTimeout(resolve, 250));
          ws.send(JSON.stringify(message));
        }
      }
      await new Promise((resolve) => setTimeout(resolve, 2_000));
      try {
        ws.close();
      } catch {
        /* already closed */
      }
      return { opened, messages: received };
    },
    { pageId, ticket, messages },
  );
}

// Either replay shape (`realtime.coalesce-replay` on or off): one `page-replay`
// frame, or a `stroke-batch` per stored batch.
function replayedStrokeIds(messages: any[]): string[] {
  const batches = messages.flatMap((m) =>
    m.type === 'page-replay' ? m.batches : m.type === 'stroke-batch' ? [m] : [],
  );
  return batches.flatMap((batch: any) => batch.strokes.map((s: any) => s.id));
}

function summarise(messages: any[]): string {
  return JSON.stringify(messages.map((m) => (m.type === 'error' ? m : m.type)));
}

async function inkedPixels(page: Page): Promise<number> {
  return page.evaluate(() => {
    const canvas = document.querySelector<HTMLCanvasElement>('[data-testid="ink-canvas"]');
    const context = canvas?.getContext('2d');
    if (!canvas || !context) return 0;
    const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
    let green = 0;
    for (let i = 0; i < pixels.length; i += 4) {
      const [red, g, blue, alpha] = [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]];
      if (alpha > 0 && red < 20 && g > 70 && g < 130 && blue < 20) green += 1;
    }
    return green;
  });
}
