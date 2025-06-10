/**
 * The reading path has had NO browser coverage, which is how a reader that rendered
 * zero pages shipped twice: the route is client-rendered, so every curl-based check
 * sees an identical 1.4 KB shell whether it works or not.
 *
 * Runs against PW_BASE_URL (default localhost:5173); point it at production with
 *   PW_BASE_URL=https://komiq.cc npx playwright test reader-loads-pages
 *
 * The assertion is deliberately about IMAGE REQUESTS, not just pixels: a blank
 * reader and a blocked image look the same to a user but are opposite bugs, and
 * telling them apart is the whole point.
 */
import { expect, test } from '@playwright/test';

/** Work w_436b98… ch=632413 — the reported failure. That chapter belongs to Asura
 *  (source 211) while the reader defaults to Arven (10423), so it exercises the
 *  cross-source lookup rather than the happy path. */
const REPORTED = '/read/w_436b98232d064477af20c526c1fd689b?ch=632413';

test('reader requests page images for a chapter from a non-default source', async ({ page }) => {
	const imageRequests: string[] = [];
	const failed: { url: string; reason: string }[] = [];
	const consoleErrors: string[] = [];

	page.on('request', (r) => {
		if (r.resourceType() === 'image') imageRequests.push(r.url());
	});
	page.on('requestfailed', (r) => {
		failed.push({ url: r.url(), reason: r.failure()?.errorText ?? '?' });
	});
	page.on('console', (m) => {
		if (m.type() === 'error') consoleErrors.push(m.text());
	});

	await page.goto(REPORTED, { waitUntil: 'domcontentloaded' });
	// The reader resolves its chapter client-side over several round trips.
	await page.waitForTimeout(15_000);

	console.log('--- image requests:', imageRequests.length);
	for (const u of imageRequests.slice(0, 5)) console.log('    ', u.slice(0, 120));
	console.log('--- failed requests:', failed.length);
	for (const f of failed.slice(0, 10)) console.log('    ', f.reason, f.url.slice(0, 110));
	console.log('--- console errors:', consoleErrors.length);
	for (const e of consoleErrors.slice(0, 10)) console.log('    ', e.slice(0, 200));

	// Zero image requests => the reader resolved no pages (the blank-reader bug).
	expect(imageRequests.length, 'reader requested no images at all').toBeGreaterThan(0);
});
