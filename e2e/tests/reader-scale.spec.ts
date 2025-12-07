import { expect, test } from '@playwright/test';

// Isolated read-only fixture: start reader-scale-server.mjs and point the reader
// PUBLIC_KOMIKA_API at its /api/graphql. Ordinary/live runs do not use these data.
const base = process.env.PW_READER_SCALE_URL;
test.skip(!base, 'Requires the local reader scale fixture');

for (const width of [1440, 390]) {
	test(`600-title library keeps offscreen controls and shelf menus usable at ${width}px`, async ({
		page,
	}) => {
		await page.setViewportSize({ width, height: 900 });
		await page.addInitScript(() => localStorage.setItem('komika-token', 'visual-fixture'));
		await page.goto(`${base}/library`);
		await expect(page.locator('.lib-card')).toHaveCount(600);
		const card = page.locator('.lib-card').nth(580);
		const trigger = card.getByRole('button', { name: /^Change shelf:/ });
		await trigger.focus();
		await expect(trigger).toBeFocused();
		await page.keyboard.press('ArrowDown');
		const menu = card.getByRole('menu', { name: 'Library shelf' });
		await expect(menu).toBeVisible();
		await expect(menu.locator('[role=menuitemradio][aria-checked=true]')).toBeFocused();
		await page.keyboard.press('End');
		await expect(menu.getByRole('menuitemradio').last()).toBeFocused();
		const unclipped = await menu
			.getByRole('menuitemradio')
			.last()
			.evaluate((item) => {
				const box = item.getBoundingClientRect();
				return item.contains(
					document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2),
				);
			});
		expect(unclipped).toBe(true);
		await page.keyboard.press('Escape');
		await expect(menu).toHaveCount(0);
		await expect(trigger).toBeFocused();
		await expect(card.locator('.cover-img')).toHaveAttribute('src', /cover\.svg/);
		expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(
			true,
		);
	});

	test(`legacy attachment reserves space while its image loads at ${width}px`, async ({ page }) => {
		await page.setViewportSize({ width, height: 900 });
		let release!: () => void;
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		await page.route('**/legacy.svg', async (route) => {
			await gate;
			await route.fulfill({
				contentType: 'image/svg+xml',
				body: '<svg xmlns="http://www.w3.org/2000/svg" width="800" height="1200"><rect width="800" height="1200" fill="#395953"/></svg>',
			});
		});
		await page.goto(`${base}/series/w_fixture_0`, { waitUntil: 'domcontentloaded' });
		const attachment = page.getByRole('link', { name: 'Open comment attachment in a new tab' });
		await attachment.scrollIntoViewIfNeeded();
		const actionPosition = () =>
			attachment.evaluate(
				(node) =>
					node.parentElement!.querySelector('.c-actions')!.getBoundingClientRect().top + scrollY,
			);
		const before = await actionPosition();
		expect(
			await attachment.locator('img').evaluate((image: HTMLImageElement) => !image.complete),
		).toBe(true);
		release();
		await expect
			.poll(() =>
				attachment
					.locator('img')
					.evaluate((image: HTMLImageElement) => image.complete && image.naturalWidth > 0),
			)
			.toBe(true);
		expect(Math.abs((await actionPosition()) - before)).toBeLessThan(1);
		await expect(attachment.locator('img')).toHaveCSS('object-fit', 'contain');
	});
}
