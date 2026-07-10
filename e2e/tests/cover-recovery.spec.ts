import { test, expect } from '@playwright/test';

const series = (id: string, title: string, coverUrl: string) => ({
	id,
	title,
	coverUrl,
	altTitles: [],
	author: '',
	artist: '',
	description: '',
	genres: [],
	type: 'MANHWA',
	status: 'ONGOING',
	sourceId: 'source',
	chapterCount: 3,
	isMarked: true,
	isNsfw: false,
	isFavorite: true,
	libraryStatus: 'READING',
	rating: { average: 0, count: 0, distribution: [] },
	scan: {},
	createdAt: '',
	updatedAt: '',
	latestChapterAt: '',
	detectedAt: '',
});

test('failed and missing library covers can be fetched without navigating away', async ({
	page,
}) => {
	const imageUrl = 'http://localhost:8080/covers/w_cover.webp';
	const entries = [
		series('w_cover', 'Broken cover fixture', imageUrl),
		series('w_missing', 'Missing cover fixture', ''),
	];
	let repaired = false;
	let metadataFetches = 0;
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
		const url = new URL(request.url());
		const input =
			request.method() === 'POST'
				? request.postDataJSON()
				: {
						query: url.searchParams.get('query') ?? '',
						variables: JSON.parse(url.searchParams.get('variables') ?? '{}'),
					};
		const query = input.query as string;
		let data: Record<string, unknown> = {};
		if (/query LibraryProgress\b/.test(query)) data = { libraryProgress: [] };
		else if (/query Library\b/.test(query)) data = { library: entries };
		else if (/query SeriesPage\b/.test(query)) {
			metadataFetches += 1;
			data = {
				seriesPage: {
					workId: input.variables.workId,
					series: null,
					spineChapters: [],
					sources: [
						{
							source: { sourceType: 'suwayomi', sourceKey: '123' },
							chapters: [],
							series: series('123', 'Recovered cover', imageUrl),
						},
					],
				},
			};
		} else if (/query Session\b/.test(query)) data = { session: null };
		else data = { notifications: { items: [], unreadCount: 0 } };
		await route.fulfill({ headers: { 'access-control-allow-origin': '*' }, json: { data } });
	});
	await page.route('**/covers/w_cover.webp**', async (route) => {
		if (!repaired) return route.fulfill({ status: 503 });
		// A real two-pixel raster; 1x1 warming placeholders are intentionally rejected.
		await route.fulfill({
			contentType: 'image/png',
			body: Buffer.from(
				'iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAFElEQVQImWNkYGD4z8DAwMDEAAUADikBA/2Z+7oAAAAASUVORK5CYII=',
				'base64',
			),
		});
	});
	await page.goto('/library');
	await expect(page.getByText('Broken cover fixture', { exact: true })).toBeVisible();
	const broken = page.getByRole('button', { name: 'Fetch cover for Broken cover fixture' });
	await expect(broken).toBeVisible();
	repaired = true;
	await broken.click();
	const recovered = page.locator('img[alt="Broken cover fixture"]');
	await expect(recovered).toHaveAttribute('src', /_coverRetry=/);
	await expect
		.poll(() => recovered.evaluate((img: HTMLImageElement) => img.complete && img.naturalWidth > 1))
		.toBe(true);
	await page.getByRole('button', { name: 'Fetch cover for Missing cover fixture' }).click();
	const missing = page.locator('img[alt="Missing cover fixture"]');
	await expect
		.poll(() => missing.evaluate((img: HTMLImageElement) => img.complete && img.naturalWidth > 1))
		.toBe(true);
	expect(metadataFetches).toBe(2);
	await expect(page).toHaveURL(/\/library$/);
});
