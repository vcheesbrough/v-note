import { expect, test } from '@playwright/test';

const APK_BASE = process.env.ANDROID_APK_BASE_URL ?? 'http://v-note-android-apk';

test('APK download route returns correct MIME type', async ({ request }) => {
  const res = await request.get(`${APK_BASE}/dl/apk`);
  expect(res.ok()).toBeTruthy();
  expect(res.headers()['content-type']).toContain('application/vnd.android.package-archive');
});
