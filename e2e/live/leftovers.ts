// Which smoke pages the live smoke may sweep (#179, PR #67 review).
//
// Several PRs deploy dev, so smoke runs can overlap. Each run's page is titled
// `live-smoke-<epoch ms>-<random>`; a run must only ever delete *abandoned*
// pages — ones old enough that no live run can still be using them — never a
// concurrent run's page mid-test. Its own page it deletes itself, in `finally`.

export const TITLE_PREFIX = 'live-smoke-';

/// Comfortably longer than a whole run, retry included (the live config allows
/// 120 s per attempt and one retry), so an overlapping run's page is never
/// old enough.
export const LEFTOVER_MIN_AGE_MS = 15 * 60 * 1000;

export function smokeTitle(now: number, random: string): string {
  return `${TITLE_PREFIX}${now}-${random}`;
}

/// True when `title` is a smoke page abandoned for at least `minAgeMs`.
///
/// A smoke-prefixed title without a parseable timestamp is not something any
/// run of this code creates, so it cannot belong to a live run: stale. A
/// timestamp in the future (another runner's clock ahead of ours) is treated as
/// fresh — skipping a sweep costs one leftover page, deleting a live one costs
/// a red pipeline.
export function isAbandonedSmokePage(title: string, now: number, minAgeMs = LEFTOVER_MIN_AGE_MS): boolean {
  if (!title.startsWith(TITLE_PREFIX)) return false;
  const match = /^live-smoke-(\d+)-/.exec(title);
  if (!match) return true;
  const createdAt = Number(match[1]);
  if (!Number.isSafeInteger(createdAt)) return true;
  return now - createdAt >= minAgeMs;
}
