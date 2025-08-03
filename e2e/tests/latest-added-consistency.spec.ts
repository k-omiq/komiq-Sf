import { test, expect } from '@playwright/test';

/** Real endpoint consistency: the Home preview must preserve Browse's first ten
 * work identities and their order, including distinct works sharing a title.
 * Run against a preview built with PUBLIC_KOMIKA_BACKEND=on and the same API.
 */
test('Latest Added home preview matches the Browse page-one prefix', async ({ page }) => {
	await page.goto('/');
	const rail = page
		.locator('section')
		.filter({ has: page.getByRole('heading', { name: 'Latest Added', exact: true }) });
	await expect(rail.locator('a.card').first()).toBeVisible({ timeout: 30_000 });
	const home = await rail
		.locator('a.card')
		.evaluateAll((cards) => cards.map((card) => card.getAttribute('href')));
	await rail.getByRole('link', { name: 'View all', exact: true }).click();
	await expect(page).toHaveURL(/\/browse\?sort=added/);
	await expect(page.locator('.grid a.card').first()).toBeVisible({ timeout: 30_000 });
	const browse = await page
		.locator('.grid a.card')
		.evaluateAll((cards) => cards.map((card) => card.getAttribute('href')));
	expect(home.length).toBeGreaterThan(0);
	expect(home).toEqual(browse.slice(0, home.length));
});
