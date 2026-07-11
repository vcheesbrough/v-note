import { expect, test } from '@playwright/test';

const APK_BASE = process.env.ANDROID_APK_BASE_URL ?? 'http://v-note-android-apk';

test('APK download route returns the release-versioned APK without caching', async ({ request }) => {
  const res = await request.get(`${APK_BASE}/dl/apk?release=0.13.1`);
  expect(res.ok()).toBeTruthy();
  expect(res.headers()['content-type']).toContain('application/vnd.android.package-archive');
  expect(res.headers()['content-disposition']).toBe(
    'attachment; filename="v-note-0.13.1-dev-debug.apk"',
  );
  // Stable URL, changing contents — must never be served stale from a cache.
  expect(res.headers()['cache-control']).toContain('no-store');
  expect(res.headers()['etag']).toBeUndefined();
});
