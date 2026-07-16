import { test, expect, type Page } from '@playwright/test';

const raster = Buffer.from(
	'iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAFElEQVQImWNkYGD4z8DAwMDEAAUADikBA/2Z+7oAAAAASUVORK5CYII=',
	'base64',
);

async function fixture(page: Page, admin: boolean) {
	await page.addInitScript(() => localStorage.setItem('komika-token', 'fixture-token'));
	let retries = 0;
	let stored = true;
	await page.route(/\/graphql(?:\?|$)/, async (route) => {
		const request = route.request();
		if (request.method() === 'OPTIONS')
			return route.fulfill({
				status: 204,
				headers: {
					'access-control-allow-origin': '*',
					'access-control-allow-methods': 'GET, POST, OPTIONS',
					'access-control-allow-headers': 'content-type, authorization',
				},
			});
		const params = new URL(request.url()).searchParams;
		const input =
			request.method() === 'POST'
				? request.postDataJSON()
				: {
						query: params.get('query') ?? '',
						variables: JSON.parse(params.get('variables') ?? '{}'),
					};
		const query = input.query as string;
		let data: Record<string, unknown> = {};
		if (/query Session\b/.test(query))
			data = {
				session: {
					token: 'fixture-token',
					user: { id: 'admin-fixture', username: 'fixture', isAdmin: admin },
				},
			};
		else if (/query SeriesPage\b/.test(query))
			data = {
				seriesPage: {
					workId: input.variables.workId,
					series: {
						id: input.variables.workId,
						title: 'Admin cover fixture',
						type: 'MANGA',
						coverUrl: 'http://localhost:8080/covers/original.webp',
						genres: [],
						altTitles: [],
						rating: { average: 0, count: 0 },
						status: 'ONGOING',
						chapterCount: 0,
					},
					spineChapters: [],
					sources: [],
					aggregated: [],
				},
			};
		else if (/query SeriesAdminMeta\b/.test(query))
			data = {
				seriesAdminMeta: {
					workId: 'w_admin',
					isNsfwOverride: null,
				},
			};
		else if (/mutation RetryCover\b/.test(query)) {
			expect(input.variables.workId).toBe('w_admin');
			expect(request.headers().authorization).toBe('Bearer fixture-token');
			retries += 1;
			data = { retryCover: stored };
		} else
			data = {
				comments: { items: [], hasNextPage: false, total: 0 },
				notifications: { items: [], unreadCount: 0 },
			};
		await route.fulfill({ headers: { 'access-control-allow-origin': '*' }, json: { data } });
	});
	await page.route('**/covers/*.webp*', (route) =>
		route.fulfill({ contentType: 'image/png', body: raster }),
	);
	return {
		retries: () => retries,
		fail: () => {
			stored = false;
		},
	};
}

test('admin Fetch cover is beside Merge and Split even with a healthy cover, and repairs the stored work', async ({
	page,
}) => {
	const state = await fixture(page, true);
	await page.goto('/series/w_admin');
	const button = page.getByRole('button', { name: 'Fetch cover', exact: true });
	await expect(button).toBeVisible();
	await expect(page.getByRole('button', { name: 'Merge', exact: true })).toBeVisible();
	await expect(page.getByRole('button', { name: 'Split', exact: true })).toBeVisible();
	const poster = page.locator('img[alt="Admin cover fixture"]');
	await expect
		.poll(() => poster.evaluate((img: HTMLImageElement) => img.complete && img.naturalWidth > 1))
		.toBe(true);
	await button.click();
	await expect(page.getByRole('status').filter({ hasText: 'Cover fetched.' })).toBeVisible();
	await expect(poster).toHaveAttribute('src', /\/covers\/w_admin\.webp\?retry=1&_coverRetry=/);
	await expect
		.poll(() => poster.evaluate((img: HTMLImageElement) => img.complete && img.naturalWidth > 1))
		.toBe(true);
	expect(state.retries()).toBe(1);
	state.fail();
	await button.click();
	await expect(
		page.getByRole('alert').filter({ hasText: 'Could not fetch a source cover' }),
	).toBeVisible();
	await expect(button).toBeEnabled();
	await expect(poster).toHaveAttribute('src', /\/covers\/w_admin\.webp\?retry=1&_coverRetry=/);
});

test('ordinary readers do not see the admin Fetch cover action', async ({ page }) => {
	const state = await fixture(page, false);
	await page.goto('/series/w_admin');
	await expect(
		page.getByRole('heading', { name: 'Admin cover fixture', exact: true }),
	).toBeVisible();
	await expect(page.getByRole('button', { name: 'Fetch cover', exact: true })).toHaveCount(0);
	expect(state.retries()).toBe(0);
});
