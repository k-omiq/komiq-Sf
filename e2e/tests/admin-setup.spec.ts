import { test, expect } from '@playwright/test';
import { GOOD_SETUP_CODE, installAdminSetupFixtures } from '../fixtures/admin-setup.mjs';

// Runs against the EMBEDDED admin build (served under /admin); see admin-setup.config.ts.

test('a fresh server opens setup, refuses a wrong code, creates the owner and closes sign-up', async ({
	page,
}) => {
	const state = await installAdminSetupFixtures(page);

	// Any console page on a server without an owner lands on the wizard.
	await page.goto('/admin/');
	await expect(page).toHaveURL(/\/admin\/setup$/);
	await expect(page.getByRole('heading', { name: 'Set up your server' })).toBeVisible();
	await page.getByRole('button', { name: 'Get started' }).click();

	// Owner account: inline validation first, then a wrong code from the server.
	await expect(page.getByRole('heading', { name: 'Create the owner account' })).toBeVisible();
	await page.getByRole('button', { name: 'Create account' }).click();
	await expect(page.getByText('At least 3 characters.')).toBeVisible();
	await page.getByLabel(/^Username/).fill('owner');
	await page.getByLabel(/^Email/).fill('owner@example.invalid');
	await page.getByLabel(/^Password/).fill('made-up-password');
	await page.getByLabel(/^Confirm/).fill('made-up-password');
	await page.getByLabel(/^Setup code/).fill('WRONG-CODE-0000');
	await page.getByRole('button', { name: 'Create account' }).click();
	await expect(page.getByText('Invalid setup code')).toBeVisible();
	expect(state.owner).toBeNull();

	await page.getByLabel(/^Setup code/).fill(GOOD_SETUP_CODE);
	await page.getByRole('button', { name: 'Create account' }).click();

	// Access: only my devices (the internet path needs Cloudflare, covered elsewhere).
	await expect(page.getByRole('heading', { name: 'Who should reach this server?' })).toBeVisible();
	await page.getByRole('radio', { name: /Only my devices/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect.poll(() => state.accessMode).toBe('local');

	// Accounts: keep sign-up closed and add one person with a temporary password.
	await expect(page.getByRole('heading', { name: 'Who can sign in?' })).toBeVisible();
	expect(state.owner).toMatchObject({ username: 'owner' });
	await expect(page.getByRole('radio', { name: /Only people I add/ })).toHaveAttribute(
		'aria-checked',
		'true',
	);
	await page.getByPlaceholder('Username').fill('reader1');
	await page.getByPlaceholder('Email').fill('reader1@example.invalid');
	await page.getByRole('button', { name: 'Add', exact: true }).click();
	await expect(page.getByRole('listitem').filter({ hasText: 'reader1' })).toBeVisible();
	await expect(
		page.getByRole('button', { name: 'Copy temporary password for reader1' }),
	).toBeVisible();
	expect(state.users.map((u) => u.username)).toEqual(['reader1']);

	await page.getByRole('button', { name: 'Continue' }).click();

	// Sources: MangaDex on, one repo, one extension installed from the picker.
	await expect(page.getByRole('heading', { name: 'Sources' })).toBeVisible();
	await expect(page.getByRole('switch', { name: 'Use MangaDex' })).toHaveAttribute(
		'aria-checked',
		'false',
	);
	await page.getByRole('switch', { name: 'Use MangaDex' }).click();
	await page.getByLabel('Repo index address').fill('https://repo.example.invalid/index.min.json');
	await page.getByRole('button', { name: 'Add', exact: true }).click();
	await expect(page.getByRole('listitem').filter({ hasText: 'Test Repo' })).toBeVisible();
	await page.getByLabel('Language').selectOption('all');
	await page.getByLabel('Search extensions').fill('beta');
	await expect(page.getByRole('listitem').filter({ hasText: 'Alpha Scans' })).toHaveCount(0);
	await page
		.getByRole('listitem')
		.filter({ hasText: 'Beta Comics' })
		.getByRole('button', { name: 'Install' })
		.click();
	await expect(page.getByRole('listitem').filter({ hasText: 'Beta Comics' })).toContainText(
		'Installed',
	);
	expect(state.mangadexEnabled).toBe(false); // applied on Continue, not on toggle
	await page.getByRole('button', { name: 'Continue' }).click();

	// Done: the choices reached the server and the console opens.
	await expect(page.getByRole('heading', { name: 'You are all set' })).toBeVisible();
	expect(state.registrationOpen).toBe(false);
	expect(state.mangadexEnabled).toBe(true);
	await expect(page.getByText('MangaDex on · 1 extension')).toBeVisible();
	await page.getByRole('button', { name: 'Open the catalog' }).click();
	await expect(page).toHaveURL(/\/admin\/$/);
	await expect(page.getByRole('navigation', { name: 'Admin navigation' })).toBeVisible();
});

test('a server that already has an owner never shows setup', async ({ page }) => {
	await installAdminSetupFixtures(page, {
		owner: { id: 'owner-1', username: 'owner', email: 'owner@example.invalid' },
	});
	await page.goto('/admin/setup');
	await expect(page).not.toHaveURL(/\/setup$/);
	await expect(page.getByRole('heading', { name: 'Set up your server' })).toHaveCount(0);
});
