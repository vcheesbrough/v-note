import { request } from '@playwright/test';

export default async function globalSetup() {
  const baseURL = process.env.BASE_URL;
  if (!baseURL) {
    throw new Error('BASE_URL environment variable is required');
  }

  const ctx = await request.newContext({ baseURL, ignoreHTTPSErrors: true });
  const maxAttempts = 60;
  const intervalMs = 500;

  for (let i = 0; i < maxAttempts; i += 1) {
    try {
      const res = await ctx.get('/health');
      if (res.ok()) {
        await ctx.dispose();
        return;
      }
    } catch {
      // app not ready yet
    }
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }

  await ctx.dispose();
  throw new Error(`App at ${baseURL} did not become ready`);
}
