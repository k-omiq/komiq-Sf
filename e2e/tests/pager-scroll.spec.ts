/**
 * A page step on /updates and /browse must land the viewer at the top of the paged
 * SECTION, on every engine.
 *
 * This had no coverage, and the gap was load-bearing: the reset used to be
 * SvelteKit's implicit post-navigation `scrollTo(0, 0)` — a single fire-and-forget
 * write issued while the document is collapsed onto skeletons (measured on /browse:
 * 6116px -> 2675px, viewer already clamped from y=5277 to y=1836) and several hundred
 * milliseconds before the rows land and it grows back. Whether that write survived the
 * shrink/regrow was up to the engine, so paging worked on desktop, iOS Safari and
 * Chrome Android and silently did nothing on other Chromium mobile builds (Brave).
 *
 * Mobile viewport on purpose — that is where the failure lived.
 *
 * Runs against PW_BASE_URL (default localhost:5173); point it at production with
 *   PW_BASE_URL=https://komiq.cc npx playwright test pager-scroll
 */
import { devices, expect, test } from '@playwright/test';

test.use({ ...devices['Pixel 7'] });

/**
 * The live API only sends CORS headers for the komiq.cc origin, so a run against a
 * local dev server would otherwise render an empty feed and skip both cases. Only
 * ever applied for a localhost base URL — a production run talks to the real API
 * with the real headers.
 */
test.beforeEach(async ({ context, baseURL }) => {
	if (!baseURL || !/^https?:\/\/(localhost|127\.0\.0\.1)[:/]/.test(baseURL)) return;
	await context.route('https://api.komiq.cc/**', async (route) => {
		const cors = {
			'access-control-allow-origin': '*',
			'access-control-allow-headers': '*',
			'access-control-allow-methods': 'GET,POST,OPTIONS',
		};
		if (route.request().method() === 'OPTIONS') {
			return route.fulfill({ status: 204, headers: cors, body: '' });
		}
		const res = await route.fetch();
		const headers = { ...res.headers(), ...cors };
		delete headers['content-encoding'];
		return route.fulfill({ response: res, headers });
	});
});

/** Sticky header height (--k-header-h), which the section's scroll-margin clears. */
const HEADER_H = 64;

for (const { name, path, section } of [
	{ name: 'updates', path: '/updates', section: 'updates-list' },
	{ name: 'browse', path: '/browse', section: 'browse-results' },
]) {
	test(`${name}: a page step aligns the section top, not the document top`, async ({ page }) => {
		await page.goto(path, { waitUntil: 'load' });
		// Both feeds resolve client-side over a couple of round trips.
		await page.waitForTimeout(4_000);

		const next = page.locator('a[rel="next"]').first();
		test.skip((await next.count()) === 0, 'feed fits in one page here — nothing to step to');

		await next.scrollIntoViewIfNeeded();
		const before = await page.evaluate(() => window.scrollY);
		expect(before, 'the pager should be well below the fold').toBeGreaterThan(500);

		await next.tap();
		// Long enough for the rows to land and the final layout to settle: the whole
		// point is that the position holds AFTER the content grows back, not just
		// during the skeleton frame.
		await page.waitForTimeout(5_000);

		const top = await page.evaluate(
			(id) => document.getElementById(id)!.getBoundingClientRect().top,
			section,
		);
		// Not `toBe`: the section clears the header by its own scroll-margin, which
		// differs per route, and sub-pixel layout is not worth asserting to the pixel.
		expect(top, 'section top should be parked just under the sticky header').toBeGreaterThanOrEqual(
			HEADER_H - 4,
		);
		expect(top).toBeLessThanOrEqual(HEADER_H + 40);
		// And it must be a real move, not "we happened to already be there".
		expect(await page.evaluate(() => window.scrollY)).toBeLessThan(before);
	});
}
