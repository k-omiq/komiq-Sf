import { test, expect } from '@playwright/test';

/**
 * Reader smoke tests.
 *
 * The reader ships a mock-data fallback, so content is always present even
 * without the backend — selectors target stable structure (brand, nav, card
 * links, page headings) rather than specific titles.
 *
 * Structure confirmed from apps/reader/src:
 *   - Header: brand link "YOMU" (a.brand) + nav links Home/Browse/Updates/Library.
 *   - Home:   MangaCard renders <a class="card" href="/series/…"> rows.
 *   - Browse: <h1>Browse</h1> + a grid of the same series cards.
 *   - Series: <h1> with the series title.
 */

test.describe('reader smoke', () => {
	test('home renders brand, nav, and a card row', async ({ page }) => {
		await page.goto('/');

		// Brand + primary nav render.
		await expect(page.locator('a.brand')).toHaveText(/YOMU/i);
		await expect(page.getByRole('link', { name: 'Browse', exact: true })).toBeVisible();
		await expect(page.getByRole('link', { name: 'Library', exact: true })).toBeVisible();

		// At least one series card is present (mock fallback guarantees content).
		const cards = page.locator('a.card');
		await expect(cards.first()).toBeVisible();
		expect(await cards.count()).toBeGreaterThan(0);
	});

	test('browse shows results', async ({ page }) => {
		await page.goto('/');
		await page.getByRole('link', { name: 'Browse', exact: true }).click();

		await expect(page).toHaveURL(/\/browse/);
		await expect(page.getByRole('heading', { name: 'Browse' })).toBeVisible();

		// Browse renders the same card grid; assert results appear.
		const cards = page.locator('a.card');
		await expect(cards.first()).toBeVisible();
		expect(await cards.count()).toBeGreaterThan(0);
	});

	test('opening a card navigates to a series page', async ({ page }) => {
		await page.goto('/browse');

		const firstCard = page.locator('a.card').first();
		await expect(firstCard).toBeVisible();
		await firstCard.click();

		// Series detail route + a title heading render.
		await expect(page).toHaveURL(/\/series\//);
		await expect(page.locator('h1').first()).toBeVisible();
		await expect(page.locator('h1').first()).not.toBeEmpty();
	});
});
