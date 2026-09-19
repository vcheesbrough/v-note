import { request } from '@playwright/test';
import * as fs from 'fs';
import * as path from 'path';

const STORAGE_STATE = path.resolve(__dirname, '.auth-state.json');

export default async function globalSetup() {
  const baseURL = process.env.BASE_URL;
  if (!baseURL) {
    throw new Error('BASE_URL environment variable is required');
  }

  await waitForApp(baseURL);

  const tokenUrl = required('OIDC_TOKEN_URL');
  const clientId = required('OIDC_CLIENT_ID');
  // The app itself is a public PKCE client and holds no secret (#274). This is
  // purely the mock IdP's `client_credentials` shortcut for minting a test token.
  const clientSecret = required('MOCK_OIDC_CLIENT_SECRET');
  const requiredScope = required('REQUIRED_SCOPE');

  await waitForMock(tokenUrl);

  const token = await fetchToken(tokenUrl, clientId, clientSecret, requiredScope);
  console.log(`Acquired test token (${token.length} chars) from ${tokenUrl}`);

  const baseHost = new URL(baseURL).hostname;
  const storage = {
    cookies: [
      {
        name: 'auth',
        value: token,
        domain: baseHost,
        path: '/',
        expires: -1,
        httpOnly: true,
        secure: true,
        sameSite: 'Lax' as const,
      },
    ],
    origins: [],
  };
  fs.writeFileSync(STORAGE_STATE, JSON.stringify(storage));
  process.env.AUTH_TOKEN = token;
}

function required(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} env var is required for e2e auth setup`);
  }
  return value;
}

async function waitForApp(baseURL: string): Promise<void> {
  const ctx = await request.newContext({ baseURL, ignoreHTTPSErrors: true });
  const maxAttempts = 60;
  const intervalMs = 500;
  for (let i = 0; i < maxAttempts; i += 1) {
    try {
      const res = await ctx.get('/health');
      if (res.ok()) {
        await ctx.dispose();
        console.log(`App ready at ${baseURL}`);
        return;
      }
    } catch {
      // not ready yet
    }
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
  await ctx.dispose();
  throw new Error(`App at ${baseURL} did not become ready within ${(maxAttempts * intervalMs) / 1000}s`);
}

async function waitForMock(tokenUrl: string): Promise<void> {
  const issuer = tokenUrl.replace(/\/token$/, '');
  const discoveryUrl = `${issuer}/.well-known/openid-configuration`;
  const ctx = await request.newContext({ ignoreHTTPSErrors: true });
  const maxAttempts = 60;
  const intervalMs = 500;
  for (let i = 0; i < maxAttempts; i += 1) {
    try {
      const res = await ctx.get(discoveryUrl);
      if (res.ok()) {
        await ctx.dispose();
        console.log(`Mock OIDC ready at ${issuer}`);
        return;
      }
    } catch {
      // not ready
    }
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
  await ctx.dispose();
  throw new Error(`Mock OIDC at ${discoveryUrl} did not become ready`);
}

async function fetchToken(
  tokenUrl: string,
  clientId: string,
  clientSecret: string,
  scope: string,
): Promise<string> {
  const ctx = await request.newContext({ ignoreHTTPSErrors: true });
  try {
    const res = await ctx.post(tokenUrl, {
      form: {
        grant_type: 'client_credentials',
        client_id: clientId,
        client_secret: clientSecret,
        scope,
      },
    });
    if (!res.ok()) {
      throw new Error(`token request failed: ${res.status()} ${await res.text()}`);
    }
    const body = await res.json();
    if (!body.access_token || typeof body.access_token !== 'string') {
      throw new Error(`token response missing access_token: ${JSON.stringify(body)}`);
    }
    return body.access_token;
  } finally {
    await ctx.dispose();
  }
}
