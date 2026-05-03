import { expect, test, type Page } from '@playwright/test';

// Deterministic catalogue vocabulary: these cases exercise the filter UI without a
// live server, and deliberately include more tags than the compact preview shows.
const tags = [
	...[
		'Action',
		'Adventure',
		'Comedy',
		'Drama',
		'Fantasy',
		'Horror',
		'Mystery',
		'Romance',
		'Sci-Fi',
		'Slice of Life',
		'Sports',
		'Thriller',
	].map((name, index) => ({
		id: name.toLowerCase().replaceAll(' ', '-'),
		name,
		group: 'GENRE',
		count: 120 - index,
	})),
	{ id: 'school-life', name: 'School Life', group: 'THEME', count: 24 },
	{ id: 'oneshot', name: 'Oneshot', group: 'FORMAT', count: 14 },
	{ id: 'violence', name: 'Violence', group: 'CONTENT', count: 12 },
];

test.beforeEach(async ({ page }) => {
	await page.route(/\/graphql(?:\?|$)/, async (route) => {
		const request = route.request();
		const query =
			request.method() === 'POST'
				? (request.postDataJSON()?.query ?? '')
				: (new URL(request.url()).searchParams.get('query') ?? '');
		const data = /query Tags\b/.test(query)
			? { tags }
			: /query Search\b/.test(query)
				? { search: { items: [], page: 1, hasNextPage: false, total: 0 } }
				: {
						session: null,
						discovery: [],
						canonicalUpdates: { items: [], hasNextPage: false },
						canonicalRecent: { items: [], hasNextPage: false },
					};
		await route.fulfill({ headers: { 'access-control-allow-origin': '*' }, json: { data } });
	});
});

async function openBrowse(page: Page, query = '') {
	// Client navigation makes the vocabulary fetch interceptable even on the SSR
	// build. The production route and its load function are otherwise unmodified.
	await page.goto('/');
	const browse = page.getByRole('link', { name: 'Browse', exact: true }).first();
	if (query)
		await browse.evaluate((link, suffix) => link.setAttribute('href', `/browse?${suffix}`), query);
	await browse.click();
	await expect(page.getByRole('heading', { name: 'Browse', exact: true })).toBeVisible();
	await expect(
		page
			.locator('aside.panel')
			.getByRole('button', { name: 'Action, not filtered', exact: true, includeHidden: true }),
	).toBeAttached();
}

async function pagedCatalogue(page: Page) {
	await page.route(/\/graphql(?:\?|$)/, async (route) => {
		const request = route.request();
		const params = new URL(request.url()).searchParams;
		const payload =
			request.method() === 'POST'
				? request.postDataJSON()
				: {
						query: params.get('query'),
						variables: JSON.parse(params.get('variables') ?? '{}'),
					};
		if (!/query Search\b/.test(payload.query ?? '')) return route.fallback();
		const current = payload.variables?.page ?? 1;
		await route.fulfill({
			headers: { 'access-control-allow-origin': '*' },
			json: {
				data: {
					search: {
						items: Array.from({ length: 30 }, (_, index) => ({
							id: `catalogue-${current}-${index}`,
							title: `Catalogue ${current}-${index}`,
							genres: ['Action'],
							chapterCount: 10,
							latestChapter: '10',
							rating: { average: 8, count: 2 },
							status: 'ONGOING',
							type: 'MANGA',
							coverUrl: null,
							isNsfw: false,
						})),
						page: current,
						hasNextPage: current < 3,
						total: 90,
					},
				},
			},
		});
	});
}

test('tag categories, expansion and search retain draft selections and all three states', async ({
	page,
}) => {
	await openBrowse(page);
	const panel = page.locator('aside.panel');
	await expect(
		panel.getByRole('button', { name: 'Sports, not filtered', exact: true }),
	).toHaveCount(0);
	await panel.getByRole('button', { name: 'Show all 12 tags' }).click();
	await expect(
		panel.getByRole('button', { name: 'Sports, not filtered', exact: true }),
	).toBeVisible();
	await panel.getByRole('button', { name: 'Romance, not filtered', exact: true }).click();
	await panel.getByRole('button', { name: 'Theme', exact: true }).click();
	await expect(panel.getByRole('button', { name: 'Romance, included', exact: true })).toBeVisible();
	await panel.getByRole('button', { name: 'School Life, not filtered', exact: true }).click();
	await panel.getByRole('searchbox', { name: 'Search tags', exact: true }).fill('Sports');
	await expect(
		panel.getByRole('button', { name: 'School Life, included', exact: true }),
	).toBeVisible();
	await expect(
		panel.getByRole('button', { name: 'Sports, not filtered', exact: true }),
	).toBeVisible();
	await panel.getByRole('button', { name: 'Romance, included', exact: true }).click();
	await expect(panel.getByRole('button', { name: 'Romance, excluded', exact: true })).toBeVisible();
	await panel.getByRole('button', { name: 'Romance, excluded', exact: true }).click();
	await expect(panel.getByRole('button', { name: 'Romance, excluded', exact: true })).toHaveCount(
		0,
	);
	await panel.getByRole('searchbox', { name: 'Search tags', exact: true }).fill('School Life');
	await expect(
		panel.getByRole('button', { name: 'School Life, included', exact: true }),
	).toBeVisible();
	await expect(panel.getByText('No matching tags to add.', { exact: true })).toBeVisible();
	await expect(page).toHaveURL(/\/browse$/);
});

test('Apply commits the batch, Reset remains a draft, and Back restores the applied state', async ({
	page,
}) => {
	await openBrowse(page, 'q=test&sort=rating&page=3');
	const panel = page.locator('aside.panel');
	await panel.getByRole('checkbox', { name: 'Ongoing', exact: true }).check();
	await panel.getByText('Manga', { exact: true }).click();
	await expect(page).toHaveURL(/page=3/);
	await expect(panel.getByRole('status')).toHaveText('2 changes ready to apply');
	await panel.getByRole('button', { name: 'Apply filters', exact: true }).click();
	await expect(page).toHaveURL(/status=ongoing/);
	const params = new URL(page.url()).searchParams;
	expect(params.get('q')).toBe('test');
	expect(params.get('sort')).toBe('rating');
	expect(params.get('page')).toBeNull();
	await panel.getByRole('button', { name: 'Reset', exact: true }).click();
	await expect(panel.getByRole('checkbox', { name: 'Ongoing', exact: true })).not.toBeChecked();
	expect(new URL(page.url()).searchParams.get('status')).toBe('ongoing');
	await panel.getByRole('button', { name: 'Apply filters', exact: true }).click();
	await expect(page).not.toHaveURL(/status=/);
	await page.goBack();
	await expect(page).toHaveURL(/status=ongoing/);
	await expect(panel.getByRole('checkbox', { name: 'Ongoing', exact: true })).toBeChecked();
	await expect(panel.getByRole('checkbox', { name: 'Manga', exact: true })).toBeChecked();
});

test('secondary facets disclose shared-link selections and number inputs commit on blur', async ({
	page,
}) => {
	await openBrowse(page, 'demo=seinen&lang=ja&from=2000&minrating=7');
	const panel = page.locator('aside.panel');
	await expect(panel.getByRole('checkbox', { name: 'Seinen', exact: true })).toBeVisible();
	await expect(panel.getByRole('checkbox', { name: 'Japanese', exact: true })).toBeVisible();
	await expect(panel.getByRole('spinbutton', { name: 'From', exact: true })).toHaveValue('2000');
	await panel.getByRole('spinbutton', { name: 'From', exact: true }).fill('2020');
	await panel.getByRole('button', { name: 'Apply filters', exact: true }).click();
	await expect(page).toHaveURL(/from=2020/);
	await panel
		.locator('details.group')
		.filter({ hasText: 'Content rating' })
		.locator('summary')
		.click();
	await expect(panel.getByRole('checkbox', { name: 'Safe', exact: true })).toBeVisible();
	await expect(panel.getByRole('checkbox', { name: 'Erotica', exact: true })).toHaveCount(0);
});

test('match options retain radio keyboard behavior', async ({ page }) => {
	await openBrowse(page);
	const panel = page.locator('aside.panel');
	await panel.getByRole('button', { name: 'Action, not filtered', exact: true }).click();
	await panel.locator('.match-options summary').click();
	const include = panel.getByRole('radiogroup', { name: 'Include', exact: true });
	await include.getByRole('radio', { name: 'All', exact: true }).focus();
	await page.keyboard.press('ArrowRight');
	await expect(include.getByRole('radio', { name: 'Any', exact: true })).toBeFocused();
	await expect(include.getByRole('radio', { name: 'Any', exact: true })).toHaveAttribute(
		'aria-checked',
		'true',
	);
	await panel.getByRole('button', { name: 'Apply filters', exact: true }).click();
	await expect(page).toHaveURL(/tagmode=or/);
});

for (const theme of ['dark', 'light'] as const) {
	test(`${theme}: desktop columns scroll independently, and paging/Back restore results`, async ({
		page,
	}) => {
		await page.emulateMedia({ colorScheme: theme, reducedMotion: 'reduce' });
		await page.setViewportSize({ width: 1280, height: 800 });
		await pagedCatalogue(page);
		await openBrowse(page);
		const filters = page.locator('.panel-body');
		const results = page.getByRole('region', { name: 'Browse results', exact: true });
		await expect(results.locator('.grid > a')).toHaveCount(30);
		await expect(results).toHaveAttribute('aria-busy', 'false');
		const filterTop = await page
			.locator('aside.panel')
			.evaluate((el) => el.getBoundingClientRect().top);
		const resultsTop = await results.evaluate((el) => el.getBoundingClientRect().top);
		expect(await filters.evaluate((el) => getComputedStyle(el, '::-webkit-scrollbar').width)).toBe(
			'3px',
		);
		expect(await results.evaluate((el) => getComputedStyle(el, '::-webkit-scrollbar').width)).toBe(
			'3px',
		);
		await filters.hover();
		await page.mouse.wheel(0, 400);
		await expect.poll(() => filters.evaluate((el) => el.scrollTop)).toBeGreaterThan(0);
		expect(await results.evaluate((el) => el.scrollTop)).toBe(0);
		expect(await page.evaluate(() => window.scrollY)).toBe(0);
		await filters.evaluate((el) => (el.scrollTop = el.scrollHeight));
		const filterPosition = await filters.evaluate((el) => el.scrollTop);
		await page.mouse.wheel(0, 800);
		expect(await results.evaluate((el) => el.scrollTop)).toBe(0);
		expect(await page.evaluate(() => window.scrollY)).toBe(0);
		await results.focus();
		await page.keyboard.press('PageDown');
		await expect.poll(() => results.evaluate((el) => el.scrollTop)).toBeGreaterThan(0);
		expect(await filters.evaluate((el) => el.scrollTop)).toBe(filterPosition);
		expect(await page.evaluate(() => window.scrollY)).toBe(0);
		await expect(
			page.locator('aside.panel').getByRole('button', { name: 'Apply filters' }),
		).toBeInViewport({ ratio: 1 });
		const next = results.getByRole('link', { name: 'Next page, page 2', exact: true });
		await next.scrollIntoViewIfNeeded();
		const previousPosition = await results.evaluate((el) => el.scrollTop);
		await next.click();
		await expect(page).toHaveURL(/page=2/);
		await expect(results).toHaveAttribute('aria-busy', 'false');
		await expect.poll(() => results.evaluate((el) => el.scrollTop)).toBe(0);
		expect(await filters.evaluate((el) => el.scrollTop)).toBe(filterPosition);
		expect(await page.evaluate(() => window.scrollY)).toBe(0);
		await page.goBack();
		await expect(page).toHaveURL(/\/browse$/);
		await expect.poll(() => results.evaluate((el) => el.scrollTop)).toBe(previousPosition);
		expect(await page.locator('aside.panel').evaluate((el) => el.getBoundingClientRect().top)).toBe(
			filterTop,
		);
		expect(await results.evaluate((el) => el.getBoundingClientRect().top)).toBe(resultsTop);
		// A component remount (leaving Browse and returning) also restores its scroll.
		await page.getByRole('link', { name: 'Home', exact: true }).first().click();
		await expect(page).toHaveURL(new URL('/', page.url()).href);
		await expect(results).toHaveCount(0);
		await page.goBack();
		await expect(page).toHaveURL(/\/browse$/);
		await expect.poll(() => results.evaluate((el) => el.scrollTop)).toBe(previousPosition);
		await results.evaluate((el) => (el.scrollTop = 0));
		await filters.evaluate((el) => (el.scrollTop = 0));
		await page.screenshot({ path: `test-results/browse-scroll-${theme}-desktop.png` });
	});

	test(`${theme}: flat surfaces, visible Apply at laptop size, and mobile dismissal/focus`, async ({
		page,
	}) => {
		await page.emulateMedia({ colorScheme: theme, reducedMotion: 'reduce' });
		await page.setViewportSize({ width: 1280, height: 800 });
		await openBrowse(page);
		const panel = page.locator('aside.panel');
		const apply = panel.getByRole('button', { name: 'Apply filters', exact: true });
		await expect(apply).toBeInViewport({ ratio: 1 });
		await expect(panel.getByText('Manga', { exact: true })).toBeInViewport({ ratio: 1 });
		const surfaces = await panel.evaluate((node) =>
			[node, ...node.querySelectorAll('*')].every((element) => {
				const style = getComputedStyle(element);
				return (
					style.backgroundImage === 'none' &&
					style.maskImage === 'none' &&
					style.borderImageSource === 'none'
				);
			}),
		);
		expect(surfaces).toBe(true);
		await panel.screenshot({ path: `test-results/browse-filters-${theme}-desktop.png` });
		await page.setViewportSize({ width: 390, height: 844 });
		const trigger = page.getByRole('button', { name: 'Filters', exact: true });
		await trigger.click();
		await expect(panel).toHaveAttribute('role', 'dialog');
		const close = panel.getByRole('button', { name: 'Close filters', exact: true });
		await expect(close).toBeFocused();
		await apply.focus();
		await page.keyboard.press('Tab');
		await expect(close).toBeFocused();
		await panel.getByRole('checkbox', { name: 'Ongoing', exact: true }).check();
		await page.screenshot({ path: `test-results/browse-filters-${theme}-mobile.png` });
		await page.keyboard.press('Escape');
		await expect(trigger).toBeFocused();
		await expect(panel).toHaveAttribute('inert', '');
		await trigger.click();
		await expect(panel.getByRole('checkbox', { name: 'Ongoing', exact: true })).not.toBeChecked();
		await expect(apply).toBeInViewport({ ratio: 1 });
	});
}

test('mobile keeps document scrolling and paging clears the fixed header', async ({ page }) => {
	await page.setViewportSize({ width: 390, height: 844 });
	await pagedCatalogue(page);
	await openBrowse(page);
	const results = page.getByRole('region', { name: 'Browse results', exact: true });
	await expect(results.locator('.grid > a')).toHaveCount(30);
	const next = results.getByRole('link', { name: 'Next page, page 2', exact: true });
	await next.scrollIntoViewIfNeeded();
	const before = await page.evaluate(() => window.scrollY);
	expect(before).toBeGreaterThan(500);
	expect(await results.evaluate((el) => el.scrollTop)).toBe(0);
	await next.click();
	await expect(page).toHaveURL(/page=2/);
	await expect(results).toHaveAttribute('aria-busy', 'false');
	await expect
		.poll(() => results.evaluate((el) => el.getBoundingClientRect().top))
		.toBeGreaterThanOrEqual(60);
	await expect
		.poll(() => results.evaluate((el) => el.getBoundingClientRect().top))
		.toBeLessThanOrEqual(104);
	expect(await page.evaluate(() => window.scrollY)).toBeLessThan(before);
});
