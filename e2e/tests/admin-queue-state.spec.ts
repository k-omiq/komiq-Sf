import { test, expect } from '@playwright/test';
import { installAdminQueueFixtures } from '../fixtures/admin-queues.mjs';
test.beforeEach(async ({ page }) => {
	await installAdminQueueFixtures(page);
});
for (const route of ['users', 'updates', 'bugs', 'review', 'reports']) {
	test(`${route} pagination reload and history preserve context`, async ({ page }) => {
		await page.goto(`/${route}?page=2&context=keep`);
		await expect(page.getByText('Page 2', { exact: false }).first()).toBeVisible();
		await page.getByRole('button', { name: /Next/ }).click();
		await expect(page).toHaveURL(new RegExp(`/${route}\\?.*page=3.*context=keep`));
		await expect(page.getByText('Page 3', { exact: false }).first()).toBeVisible();
		await page.reload();
		await expect(page.getByText('Page 3', { exact: false }).first()).toBeVisible();
		await page.goBack();
		await expect(page.getByText('Page 2', { exact: false }).first()).toBeVisible();
		await page.goForward();
		await expect(page.getByText('Page 3', { exact: false }).first()).toBeVisible();
	});
}
test('report URL filters and same-route note drafts', async ({ page }) => {
	await page.goto('/reports?context=keep');
	await page.getByRole('button', { name: 'Read', exact: true }).first().click();
	await page.getByLabel('Add a note (readers never see it)').fill('Retained draft');
	await page.getByRole('button', { name: 'Resolved', exact: true }).click();
	await expect(page).toHaveURL(/status=RESOLVED/);
	await page.getByRole('combobox', { name: 'Kind', exact: true }).selectOption('COMMENT_ABUSE');
	await expect(page).toHaveURL(/kind=COMMENT_ABUSE/);
	await page.getByRole('button', { name: /^Open/ }).click();
	await page.getByRole('combobox', { name: 'Kind', exact: true }).selectOption('');

	await expect(page.getByLabel('Add a note (readers never see it)')).toHaveValue('Retained draft');
	page.once('dialog', (dialog) => dialog.dismiss());
	await page
		.getByRole('navigation', { name: 'Admin navigation' })
		.getByRole('link', { name: 'Users', exact: true })
		.click();
	await expect(page).toHaveURL(/\/reports/);
	await expect(page.getByLabel('Add a note (readers never see it)')).toHaveValue('Retained draft');
});
test('source extension filters persist and unrelated context survives tab changes', async ({
	page,
}) => {
	await page.goto('/sources?context=keep');
	await page.getByRole('button', { name: /Installed \(/ }).click();
	await page.getByLabel('Filter by language').selectOption('en');
	await page.getByLabel('Filter extensions by name').fill('Fixture');
	await expect(page).toHaveURL(/extQuery=Fixture/);
	await page.reload();
	await expect(page.getByLabel('Filter by language')).toHaveValue('en');
	await expect(page.getByLabel('Filter extensions by name')).toHaveValue('Fixture');
	await page.getByRole('tab', { name: /Scan health/ }).click();
	await expect(page).toHaveURL(/context=keep/);
	await expect(page.getByRole('table', { name: 'Per-source scan health' })).toBeVisible();
});
test('long report health and ingest states fit mobile', async ({ page }) => {
	await page.setViewportSize({ width: 390, height: 844 });
	for (const path of [
		'/reports',
		'/sources?tab=health',
		'/sources?tab=catalogue&source=fixture-0',
	]) {
		await page.goto(path);
		if (path.includes('health'))
			await expect(page.getByRole('table', { name: 'Per-source scan health' })).toBeVisible();
		else if (path.includes('sources'))
			await expect(page.getByText('Add all from source fixture-0')).toBeVisible();
		else await expect(page.getByRole('table', { name: 'Reader reports' })).toBeVisible();
		expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(
			true,
		);
	}
});
test('health loading failure and retry are available', async ({ page }) => {
	const state = { delayField: 'sourceScanHealth', errorField: 'sourceScanHealth' };
	await installAdminQueueFixtures(page, state);
	await page.goto('/sources?tab=health');
	await expect(page.getByRole('status')).toContainText('Loading scan health');
	await expect(page.getByRole('alert')).toContainText('Fixture queue unavailable');
	state.errorField = '';
	state.delayField = '';
	await page.getByRole('button', { name: 'Retry', exact: true }).click();
	await expect(page.getByRole('table', { name: 'Per-source scan health' })).toBeVisible();
});
for (const [field, path] of [
	['extensions', '/sources'],
	['sources', '/sources?tab=catalogue&source=fixture-0&context=keep'],
] as const) {
	test(`${field} initial failure remains visible with explicit retry`, async ({ page }) => {
		const state = { errorField: field };
		await installAdminQueueFixtures(page, state);
		await page.goto(path);
		await expect(page.getByRole('alert')).toContainText('Fixture queue unavailable');
		if (field === 'sources') await expect(page).toHaveURL(/source=fixture-0/);
		state.errorField = '';
		await page.getByRole('button', { name: 'Retry', exact: true }).click();
		if (field === 'sources')
			await expect(page.getByLabel('Pick a source')).toHaveValue('fixture-0');
		else await expect(page.getByRole('table', { name: 'Extensions' })).toBeVisible();
	});
}
test('ingestion poll failures retain progress and provide retry', async ({ page }) => {
	const state = { errorField: '' };
	await installAdminQueueFixtures(page, state);
	await page.goto('/sources?tab=catalogue&source=fixture-0');
	await expect(page.getByText('Add all from source fixture-0')).toBeVisible();
	state.errorField = 'sourceIngestJobs';
	await expect(page.getByText(/progress updates paused after repeated failures/)).toBeVisible({
		timeout: 20000,
	});
	await expect(page.getByRole('button', { name: 'Retry', exact: true })).toBeVisible();
	state.errorField = '';
	await page.getByRole('button', { name: 'Retry', exact: true }).click();
	await expect(page.getByText(/progress updates paused after repeated failures/)).toHaveCount(0);
	await expect(page.getByText('Add all from source fixture-0')).toBeVisible();
});

test('report status and kind survive reload without drafting', async ({ page }) => {
	await page.goto('/reports?status=REJECTED&kind=OTHER&context=keep');
	await expect(page.getByRole('button', { name: 'Rejected', exact: true })).toHaveAttribute(
		'aria-pressed',
		'true',
	);
	await page.reload();
	await expect(page.getByRole('button', { name: 'Rejected', exact: true })).toHaveAttribute(
		'aria-pressed',
		'true',
	);
	await expect(page.getByRole('combobox', { name: 'Kind', exact: true })).toHaveValue('OTHER');
});

// Invalid URL state must never be sent as an out-of-range GraphQL Int.
test('invalid queue pages safely use the first page', async ({ page }) => {
	for (const value of ['-1', '1.5', '2147483648', '9007199254740992']) {
		await page.goto(`/users?page=${value}&context=keep`);
		await expect(page.getByText('Page 1', { exact: false }).first()).toBeVisible();
		await page.getByRole('button', { name: /Next/ }).click();
		await expect(page).toHaveURL(/page=2&context=keep/);
	}
});
