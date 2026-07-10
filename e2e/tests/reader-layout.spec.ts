import { test, expect } from '@playwright/test';

test('long-strip manga forces scroll progress while ordinary manga retains page preferences', async ({
	page,
}) => {
	await page.addInitScript(() =>
		localStorage.setItem('komiq-reader', JSON.stringify({ mode: 'page', bar: 'segments' })),
	);
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
		const id = input.variables?.id ?? input.variables?.seriesId ?? '42';
		let data: Record<string, unknown> = {};
		if (/query Series\b/.test(query))
			data = {
				series: {
					id,
					title: 'Layout fixture',
					type: 'MANGA',
					genres: id === '42' ? ['Long Strip'] : [],
					metadata: null,
					rating: { average: 0, count: 0 },
					status: 'ONGOING',
					chapterCount: 1,
				},
			};
		else if (/query Chapters\b/.test(query))
			data = {
				chapters: [
					{
						id: `${id}0`,
						seriesId: id,
						number: 1,
						label: '1',
						title: 'Fixture',
						read: false,
						lastPageRead: 0,
						pageCount: 2,
						externalUrl: null,
						scanlator: null,
					},
				],
			};
		else if (/query Pages\b/.test(query))
			data = {
				pages: [0, 1].map((index) => ({
					index,
					sourceUrl: `http://localhost:8080/layout-${index}.png`,
					width: 600,
					height: 2000,
				})),
			};
		else if (/query Session\b/.test(query)) data = { session: null };
		else data = { comments: { items: [], hasNextPage: false, total: 0 } };
		await route.fulfill({ headers: { 'access-control-allow-origin': '*' }, json: { data } });
	});
	await page.route('**/layout-*.png', (route) =>
		route.fulfill({
			contentType: 'image/png',
			body: Buffer.from(
				'iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAFElEQVQImWNkYGD4z8DAwMDEAAUADikBA/2Z+7oAAAAASUVORK5CYII=',
				'base64',
			),
		}),
	);
	await page.goto('/read/42?ch=420');
	await expect(page.getByRole('slider', { name: 'Chapter progress' })).toBeVisible();
	await page.getByRole('button', { name: 'Menu', exact: true }).click();
	await expect(page.getByText('Long strip', { exact: true })).toBeVisible();
	await expect(page.getByText('Single page', { exact: true })).toHaveCount(0);
	await expect(page.getByText('Double page', { exact: true })).toHaveCount(0);
	await page.goto('/read/43?ch=430');
	await expect(page.getByRole('slider', { name: 'Chapter progress' })).toHaveCount(0);
	await expect(page.locator('.pbar')).toBeVisible();
	await page.getByRole('button', { name: 'Menu', exact: true }).click();
	await expect(page.getByText('Single page', { exact: true })).toBeVisible();
	expect(await page.evaluate(() => JSON.parse(localStorage.getItem('komiq-reader')!).mode)).toBe(
		'page',
	);
});
