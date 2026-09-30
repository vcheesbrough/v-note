import { expect, test } from '../csp-guard';
import { LEFTOVER_MIN_AGE_MS, isAbandonedSmokePage, smokeTitle } from './leftovers';

// Pure checks of the sweep filter (PR #67 review): overlapping smoke runs must
// never delete each other's page. Runs with the live smoke, before it — a
// filter that would sweep a live page fails the step before any page is
// touched.

const now = 1_800_000_000_000;

test.describe('live smoke leftover sweep', () => {
  test('a concurrent run’s fresh page is never swept', () => {
    expect(isAbandonedSmokePage(smokeTitle(now, 'abc'), now)).toBe(false);
    expect(isAbandonedSmokePage(smokeTitle(now - 60_000, 'abc'), now)).toBe(false);
    expect(isAbandonedSmokePage(smokeTitle(now - LEFTOVER_MIN_AGE_MS + 1, 'abc'), now)).toBe(false);
  });

  test('a page older than the minimum age is swept', () => {
    expect(isAbandonedSmokePage(smokeTitle(now - LEFTOVER_MIN_AGE_MS, 'abc'), now)).toBe(true);
    expect(isAbandonedSmokePage(smokeTitle(now - 24 * 60 * 60 * 1000, 'abc'), now)).toBe(true);
  });

  test('a timestamp ahead of our clock counts as fresh', () => {
    expect(isAbandonedSmokePage(smokeTitle(now + 5 * 60_000, 'abc'), now)).toBe(false);
  });

  test('only smoke-prefixed pages are ever candidates', () => {
    expect(isAbandonedSmokePage('my notes', now)).toBe(false);
    expect(isAbandonedSmokePage(`x-${smokeTitle(0, 'abc')}`, now)).toBe(false);
    // Prefixed but not in the format any run creates: cannot be a live run's.
    expect(isAbandonedSmokePage('live-smoke-manual', now)).toBe(true);
  });

  test('the minimum age outlasts a whole run with its retry', () => {
    // playwright.live.config.ts: 120 s per attempt, one retry in CI.
    expect(LEFTOVER_MIN_AGE_MS).toBeGreaterThan(2 * 120_000 * 2);
  });
});
