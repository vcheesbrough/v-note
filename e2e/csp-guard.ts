// Every spec imports `test` and `expect` from here, not from
// '@playwright/test' (#444). The app serves the SPA with a real
// Content-Security-Policy; a refusal under it is a broken feature even when the
// page happens to look fine (a telemetry export or a WebSocket that silently
// never connects), so any violation, in any browser context a test opens,
// fails that test.
//
// Two listeners, because neither alone sees everything: the document's
// `securitypolicyviolation` event carries the directive and the blocked URL,
// and the console's "Refused to …" line also covers what has no document to
// fire on.
import {
  expect,
  test as base,
  type Browser,
  type BrowserContext,
} from '@playwright/test';

export * from '@playwright/test';

const BINDING = '__vnoteCspViolation';

const watched = new WeakSet<BrowserContext>();

async function watch(context: BrowserContext, found: string[]): Promise<void> {
  if (watched.has(context)) return;
  watched.add(context);
  await context.exposeBinding(BINDING, (_source, report: string) => {
    found.push(report);
  });
  await context.addInitScript((binding: string) => {
    document.addEventListener(
      'securitypolicyviolation',
      (event) => {
        const report =
          `${event.effectiveDirective} refused ${event.blockedURI || '(inline)'}` +
          ` at ${event.sourceFile || document.location.href}:${event.lineNumber}`;
        (window as unknown as Record<string, (r: string) => void>)[binding](report);
      },
      true,
    );
  }, BINDING);
  context.on('console', (message) => {
    if (message.type() === 'error' && /Content Security Policy/i.test(message.text())) {
      found.push(`console: ${message.text()}`);
    }
  });
}

export const test = base.extend<{ cspViolations: string[] }>({
  // Test-scoped and automatic, so it runs for every test whether or not the
  // test names it. Specs that open their own contexts (`browser.newContext`)
  // are covered by wrapping that call for the test's duration.
  cspViolations: [
    async ({ browser }, use) => {
      const found: string[] = [];
      const original = browser.newContext;
      (browser as Browser).newContext = async function (
        this: Browser,
        ...args: Parameters<Browser['newContext']>
      ) {
        const context = await original.apply(this, args);
        await watch(context, found);
        return context;
      };
      try {
        await use(found);
      } finally {
        browser.newContext = original;
      }
      expect(found, 'Content-Security-Policy violations').toEqual([]);
    },
    { auto: true },
  ],
  context: async ({ context, cspViolations }, use) => {
    await watch(context, cspViolations);
    await use(context);
  },
});

export { expect };
