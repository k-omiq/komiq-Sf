import { test, expect, type Page } from '@playwright/test';
import {
	GOOD_SETUP_CODE,
	PLAIN_PREFLIGHT,
	installAdminSetupFixtures,
} from '../fixtures/admin-setup.mjs';

// Runs against the EMBEDDED admin build (served under /admin); see admin-setup.config.ts.

/** Welcome and the owner account: leaves the wizard on the Access step. */
async function reachAccessStep(page: Page): Promise<void> {
	await page.goto('/admin/setup');
	await page.getByRole('button', { name: 'Get started' }).click();
	await page.getByLabel(/^Username/).fill('owner');
	await page.getByLabel(/^Email/).fill('owner@example.invalid');
	await page.getByLabel(/^Password/).fill('made-up-password');
	await page.getByLabel(/^Confirm/).fill('made-up-password');
	await page.getByLabel(/^Setup code/).fill(GOOD_SETUP_CODE);
	await page.getByRole('button', { name: 'Create account' }).click();
	await expect(page.getByRole('heading', { name: 'Who should reach this server?' })).toBeVisible();
}

/** The permission rows in the "Open the token page" link, and the rest of its query. */
async function tokenPageLink(page: Page) {
	const link = page.getByRole('link', { name: 'Open the token page' });
	await expect(link).toHaveAttribute('target', '_blank');
	await expect(link).toHaveAttribute('rel', 'noreferrer');
	const url = new URL((await link.getAttribute('href')) ?? '');
	return {
		page: url.origin + url.pathname,
		keys: JSON.parse(url.searchParams.get('permissionGroupKeys') ?? 'null'),
		accountId: url.searchParams.get('accountId'),
		zoneId: url.searchParams.get('zoneId'),
		name: url.searchParams.get('name'),
	};
}

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

	// The empty catalog opens the Getting started card, ticked from what the server has:
	// the repo and the extension from the Sources step, the reader from Accounts, and
	// no series yet.
	const card = page.getByRole('region', { name: 'Getting started' });
	await expect(card).toContainText('3 of 4 done');
	await expect(card.getByRole('link', { name: /^Done: Add an extension repo/ })).toBeVisible();
	await expect(card.getByRole('link', { name: /^Done: Install an extension/ })).toBeVisible();
	await expect(card.getByRole('link', { name: /^Done: Add a reader/ })).toBeVisible();
	await expect(card.getByRole('link', { name: /^To do: Add a series/ })).toHaveAttribute(
		'href',
		'/admin/sources/browse',
	);
	// Dismissed stays dismissed.
	await card.getByRole('button', { name: 'Dismiss' }).click();
	await expect(card).toHaveCount(0);
	await page.reload();
	await expect(page.getByRole('heading', { name: 'Catalog' })).toBeVisible();
	await expect(card).toHaveCount(0);
});

test('a server without a domain is made public at its own address', async ({ page }) => {
	const state = await installAdminSetupFixtures(page);

	await page.goto('/admin/setup');
	await page.getByRole('button', { name: 'Get started' }).click();
	await page.getByLabel(/^Username/).fill('owner');
	await page.getByLabel(/^Email/).fill('owner@example.invalid');
	await page.getByLabel(/^Password/).fill('made-up-password');
	await page.getByLabel(/^Confirm/).fill('made-up-password');
	await page.getByLabel(/^Setup code/).fill(GOOD_SETUP_CODE);
	await page.getByRole('button', { name: 'Create account' }).click();

	// Access: the third way adds its own step, and nothing is saved until it is filled.
	await page.getByRole('radio', { name: /Public without a domain/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Public without a domain' })).toBeVisible();
	// Encryption is on unless the owner switches it off; the warning shows only then.
	const encrypt = page.getByRole('switch', { name: 'Encrypt the connection' });
	await expect(encrypt).toHaveAttribute('aria-checked', 'true');
	await expect(
		page.getByText(/free certificate for this address from Let’s Encrypt/),
	).toBeVisible();
	await expect(page.getByText('This connection is not encrypted.')).toHaveCount(0);
	await encrypt.click();
	await expect(page.getByText('This connection is not encrypted.')).toBeVisible();
	await encrypt.click();
	expect(state.accessMode).toBeNull();
	await page.getByLabel('Server address').fill('203.0.113.5');
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect.poll(() => state.accessMode).toBe('direct');
	expect(state.accessEncrypted).toBe(true);

	// The summary shows the public address, not the one this console is open on.
	await expect(page.getByRole('heading', { name: 'Who can sign in?' })).toBeVisible();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Sources' })).toBeVisible();
	await page.getByRole('button', { name: 'Skip' }).click();
	await expect(page.getByRole('heading', { name: 'You are all set' })).toBeVisible();
	await expect(page.getByText('https://203.0.113.5', { exact: true })).toBeVisible();
	// This console is still open at the plain address: the owner is sent to the new one.
	await expect(page.getByRole('link', { name: 'https://203.0.113.5/admin' })).toBeVisible();
	await expect(page.getByRole('img', { name: 'QR code for https://203.0.113.5' })).toBeVisible();
});

test('plain HTTP from the internet sends the owner to a terminal, with no form', async ({
	page,
}) => {
	const state = await installAdminSetupFixtures(page, { plainFromInternet: true });
	await page.goto('/admin/setup');
	await page.getByRole('button', { name: 'Get started' }).click();

	await expect(page.getByRole('heading', { name: 'Create the owner account' })).toBeVisible();
	await expect(page.getByText('docker compose exec server komika-server setup')).toBeVisible();
	await expect(page.getByLabel(/^Password/)).toHaveCount(0);
	await expect(page.getByRole('button', { name: 'Create account' })).toHaveCount(0);
	await expect(page.getByRole('button', { name: 'I ran it' })).toBeVisible();
	expect(state.calls.some((c) => c.op === 'CreateOwner')).toBe(false);
});

test('a server that already has an owner never shows setup', async ({ page }) => {
	await installAdminSetupFixtures(page, {
		owner: { id: 'owner-1', username: 'owner', email: 'owner@example.invalid' },
	});
	await page.goto('/admin/setup');
	await expect(page).not.toHaveURL(/\/setup$/);
	await expect(page.getByRole('heading', { name: 'Set up your server' })).toHaveCount(0);
});

test('the internet step says who does what, links the token page and reads the server', async ({
	page,
}) => {
	const state = await installAdminSetupFixtures(page, {
		preflight: {
			...PLAIN_PREFLIGHT,
			cloudflared: 'download',
			httpsPortState: 'in_use',
			inContainer: true,
			publicIp: '203.0.113.9',
		},
		tokenMissing: { TUNNEL: ['Account › Cloudflare Tunnel', 'Zone › DNS'], PUBLIC_IP: [] },
	});
	await reachAccessStep(page);
	await page.getByRole('radio', { name: /Public on the internet/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Put your server online' })).toBeVisible();

	// What the owner does by hand, with a plain link to Cloudflare. One line names the
	// three things; it unfolds to say how.
	const once = page.getByRole('heading', { name: 'You do this once' });
	await expect(once).toBeVisible();
	await expect(page.getByText('account, domain, token')).toBeVisible();
	await expect(page.getByText('A Cloudflare account. It is free.')).toBeHidden();
	await once.click();
	await expect(page.getByText('A Cloudflare account. It is free.')).toBeVisible();
	await expect(page.getByText(/Your domain, added to Cloudflare\./)).toBeVisible();
	await expect(page.getByText(/An API token, so komiq may set things up for you\./)).toBeVisible();
	const dashboard = page.getByRole('link', { name: 'Open Cloudflare' });
	await expect(dashboard).toHaveAttribute('href', 'https://dash.cloudflare.com/');
	await expect(dashboard).toHaveAttribute('target', '_blank');
	await expect(dashboard).toHaveAttribute('rel', 'noreferrer');

	// What komiq does: folded away until asked for, and it follows the connection.
	const rest = page.getByRole('list', { name: 'What komiq does' });
	await expect(rest).toBeHidden();
	await page.getByText('komiq does the rest').click();
	await expect(rest.getByRole('listitem')).toHaveCount(7);
	await expect(rest).toContainText('Downloads the connector (cloudflared).');
	await expect(rest).toContainText('Creates the tunnel.');

	// Tunnel: the server has no connector, so setup downloads it.
	const server = page.getByRole('list', { name: 'This server' });
	await expect(server.getByRole('listitem')).toHaveText([
		'komiq downloads the connector (cloudflared) during setup.',
	]);

	// The token page link carries only the permission keys Cloudflare documents.
	expect(await tokenPageLink(page)).toEqual({
		page: 'https://dash.cloudflare.com/profile/api-tokens',
		keys: [
			{ key: 'zone', type: 'read' },
			{ key: 'dns', type: 'edit' },
			{ key: 'zone_settings', type: 'edit' },
		],
		accountId: '*',
		zoneId: 'all',
		name: 'komiq',
	});
	// The help lists which rows the link fills in and which the owner adds.
	await page.getByText('How to create the token').click();
	const filled = page.getByRole('list', { name: 'Filled in by the link' });
	const byHand = page.getByRole('list', { name: 'Add by hand' });
	await expect(filled.getByRole('listitem')).toHaveText([
		/Zone\s*Zone\s*Read/,
		/Zone\s*DNS\s*Edit/,
		/Zone\s*Zone Settings\s*Edit/,
	]);
	await expect(byHand.getByRole('listitem')).toHaveText([
		/Account\s*Cloudflare Tunnel\s*Edit/,
		/Zone\s*Cache Rules\s*Edit/,
		/Account\s*Account Rulesets\s*Edit/,
		/Account\s*Account Filter Lists\s*Edit/,
	]);

	// Check token: the check is for the chosen connection, and a token that seems to
	// lack a permission gets a warning that does not block setup.
	await page.getByLabel(/^Domain/).fill('read.example.invalid');
	await page.getByLabel(/^Cloudflare API token/).fill('made-up-token');
	await page.getByRole('button', { name: 'Check token' }).click();
	await expect(page.getByText('Found the zone example.invalid in Test Account.')).toBeVisible();
	await expect(
		page.getByText('This token may lack: Account › Cloudflare Tunnel, Zone › DNS.'),
	).toBeVisible();
	expect(state.tokenChecks).toEqual([{ connection: 'TUNNEL', domain: 'read.example.invalid' }]);
	await expect(page.getByRole('button', { name: 'Set up', exact: true })).toBeEnabled();

	// Public IP: another check, other server lines, another link.
	await page.getByRole('radio', { name: /Public IP/ }).click();
	await expect(page.getByText(/This token may lack/)).toHaveCount(0);
	await expect(server.getByRole('listitem')).toHaveText([
		'Port 443 is used by another program on this server. Stop it, or use a tunnel.',
		'This server runs in a container. Port 443 must be published too. Run this on the server, then make the choice here: bash deploy/selfhost/install.sh --access public-ip --no-setup',
	]);
	await expect(page.getByLabel(/^Public IP address/)).toHaveAttribute('placeholder', '203.0.113.9');
	await expect(rest.getByRole('listitem')).toHaveCount(7);
	await expect(rest).toContainText(
		'Gets a certificate from Cloudflare and serves HTTPS on port 443.',
	);
	expect((await tokenPageLink(page)).keys).toEqual([
		{ key: 'zone', type: 'read' },
		{ key: 'dns', type: 'edit' },
		{ key: 'zone_settings', type: 'edit' },
		{ key: 'ssl_and_certificates', type: 'edit' },
	]);
	await expect(filled.getByRole('listitem')).toHaveCount(4);
	await expect(filled).toContainText('SSL and Certificates');
	await expect(byHand.getByRole('listitem')).toHaveCount(3);
	await expect(byHand).not.toContainText('Cloudflare Tunnel');
	await page.getByRole('button', { name: 'Check token' }).click();
	await expect(page.getByText('Found the zone example.invalid in Test Account.')).toBeVisible();
	await expect(page.getByText(/This token may lack/)).toHaveCount(0);
	expect(state.tokenChecks.at(-1)).toEqual({
		connection: 'PUBLIC_IP',
		domain: 'read.example.invalid',
	});
});

test('the internet step on an older server: no server lines, and the token is still checked', async ({
	page,
}) => {
	const state = await installAdminSetupFixtures(page, { oldServer: true });
	await reachAccessStep(page);
	await page.getByRole('radio', { name: /Public on the internet/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Put your server online' })).toBeVisible();
	await expect.poll(() => state.calls.some((c) => c.op === 'AccessPreflight')).toBe(true);

	await expect(page.getByRole('heading', { name: 'You do this once' })).toBeVisible();
	await expect(page.getByRole('list', { name: 'This server' })).toHaveCount(0);
	// Such a server does not download the connector: it uses the one on its machine.
	await page.getByText('komiq does the rest').click();
	await expect(page.getByRole('list', { name: 'What komiq does' })).toContainText(
		'Uses the connector (cloudflared) on this server.',
	);
	await page.getByRole('radio', { name: /Public IP/ }).click();
	await expect(page.getByLabel(/^Public IP address/)).toHaveAttribute(
		'placeholder',
		'Detected automatically',
	);
	await expect(page.getByRole('list', { name: 'This server' })).toHaveCount(0);

	// The new document is refused, so the console asks again the old way.
	await page.getByLabel(/^Domain/).fill('read.example.invalid');
	await page.getByLabel(/^Cloudflare API token/).fill('made-up-token');
	await page.getByRole('button', { name: 'Check token' }).click();
	await expect(page.getByText('Found the zone example.invalid in Test Account.')).toBeVisible();
	await expect(page.getByText(/This token may lack/)).toHaveCount(0);
	expect(
		state.calls.filter((c) => c.op.startsWith('CheckCloudflareToken')).map((c) => c.op),
	).toEqual(['CheckCloudflareTokenFor', 'CheckCloudflareToken']);
	expect(state.tokenChecks).toEqual([{ connection: null, domain: 'read.example.invalid' }]);
});

test('the address step reads the server: its ports, a container and its public address', async ({
	page,
}) => {
	await installAdminSetupFixtures(page, {
		preflight: { ...PLAIN_PREFLIGHT, inContainer: true, publicIp: '203.0.113.9' },
	});
	await reachAccessStep(page);
	await page.getByRole('radio', { name: /Public without a domain/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Public without a domain' })).toBeVisible();

	// This console is open on the server's own computer, so there is no address to
	// start from: the public address the server found is suggested.
	const address = page.getByLabel('Server address');
	await expect(address).toHaveValue('203.0.113.9');
	await expect(
		page.getByText(
			'This server’s public IP address. Open ports 80 and 443 in your cloud firewall.',
		),
	).toBeVisible();
	const server = page.getByRole('list', { name: 'This server' });
	await expect(server.getByRole('listitem')).toHaveText([
		'Ports 80 and 443 are free.',
		'This server runs in a container. Ports 80 and 443 must be published too. Run this on the server, then make the choice here: bash deploy/selfhost/install.sh --access direct --no-setup',
	]);
	// Without a certificate those ports are not needed, and readers use the HTTP port:
	// the suggestion gets it, and a container must let other machines reach it.
	const encrypt = page.getByRole('switch', { name: 'Encrypt the connection' });
	await encrypt.click();
	await expect(address).toHaveValue('203.0.113.9:8080');
	await expect(server.getByRole('listitem')).toHaveText([
		'This server runs in a container. With Docker, the HTTP port must also answer other machines: KOMIQ_BIND=0.0.0.0 in deploy/selfhost/.env. The installer sets it. Run this on the server, then save the address here: bash deploy/selfhost/install.sh --access direct --encrypt no --no-setup',
	]);
	await encrypt.click();
	await expect(address).toHaveValue('203.0.113.9');
});

test('the address step says where public port 80 must go on a server with other ports', async ({
	page,
}) => {
	await installAdminSetupFixtures(page, { preflight: { ...PLAIN_PREFLIGHT, acmePort: 8081 } });
	await reachAccessStep(page);
	await page.getByRole('radio', { name: /Public without a domain/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(
		page.getByText(
			'This server’s public IP address. Forward public port 80 to port 8081 of this server: Let’s Encrypt always connects to port 80. Open port 443 for readers.',
		),
	).toBeVisible();
	// Outside a container, plain HTTP has no line about the server.
	await page.getByRole('switch', { name: 'Encrypt the connection' }).click();
	await expect(page.getByRole('list', { name: 'This server' })).toHaveCount(0);
});

test('the address step names a port another program holds', async ({ page }) => {
	await installAdminSetupFixtures(page, {
		preflight: { ...PLAIN_PREFLIGHT, httpsPortState: 'denied', acmePortState: 'in_use' },
	});
	await reachAccessStep(page);
	await page.getByRole('radio', { name: /Public without a domain/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('list', { name: 'This server' }).getByRole('listitem')).toHaveText([
		'This server does not have permission to open port 443. Start the server with permission for ports below 1024.',
		'Port 80 is used by another program on this server. Stop it first.',
	]);
	// No public address found: the field stays empty for the owner to fill.
	await expect(page.getByLabel('Server address')).toHaveValue('');
});

test('the internet step says when this server cannot get the connector', async ({ page }) => {
	await installAdminSetupFixtures(page, {
		preflight: {
			...PLAIN_PREFLIGHT,
			cloudflared: 'unsupported',
			cloudflaredDetail:
				'There is no pinned cloudflared build for this system. Install cloudflared and set CLOUDFLARED_BIN to its path.',
			httpsPortState: 'unknown',
		},
	});
	await reachAccessStep(page);
	await page.getByRole('radio', { name: /Public on the internet/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	const server = page.getByRole('list', { name: 'This server' });
	await expect(server.getByRole('listitem')).toHaveText([
		'There is no pinned cloudflared build for this system. Install cloudflared and set CLOUDFLARED_BIN to its path. Or choose Public IP.',
	]);
	await page.getByText('komiq does the rest').click();
	await expect(page.getByRole('list', { name: 'What komiq does' })).toContainText(
		'Uses the connector (cloudflared) once you install it.',
	);
	// A port check that failed for another reason is said as such, not as a refusal.
	await page.getByRole('radio', { name: /Public IP/ }).click();
	await expect(server.getByRole('listitem')).toHaveText(['komiq could not check port 443.']);
});

/** From the Access step: choose the internet and open its step. */
async function reachInternetStep(page: Page): Promise<void> {
	await page.getByRole('radio', { name: /Public on the internet/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Put your server online' })).toBeVisible();
}

/** From the Accounts step to the summary, changing nothing on the way. */
async function reachDone(page: Page): Promise<void> {
	await expect(page.getByRole('heading', { name: 'Who can sign in?' })).toBeVisible();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Sources' })).toBeVisible();
	await page.getByRole('button', { name: 'Skip' }).click();
	await expect(page.getByRole('heading', { name: 'You are all set' })).toBeVisible();
}

/** One row of the summary, by its name. */
const summaryRow = (page: Page, name: string) =>
	page.locator('dl > div').filter({ has: page.getByRole('term').filter({ hasText: name }) });

test('a failed safety check hides the owner form until the server answers', async ({ page }) => {
	const state = await installAdminSetupFixtures(page, { setupCheckFails: true });
	await page.goto('/admin/setup');
	await page.getByRole('button', { name: 'Get started' }).click();

	// No answer is not "safe": no password field, and nothing to submit.
	await expect(page.getByText('komiq could not check this connection.')).toBeVisible();
	await expect(page.getByLabel(/^Password/)).toHaveCount(0);
	await expect(page.getByRole('button', { name: 'Create account' })).toHaveCount(0);

	state.setupCheckFails = false;
	await page.getByRole('button', { name: 'Try again' }).click();
	await expect(page.getByLabel(/^Password/)).toBeVisible();
	await expect(page.getByText('komiq could not check this connection.')).toHaveCount(0);
	expect(state.calls.filter((c) => c.op === 'SetupNeedsTerminal')).toHaveLength(2);
});

test('progress saved for another server does not leak into a fresh one', async ({ page }) => {
	await installAdminSetupFixtures(page);
	// Left by an earlier install on this address: a step this build does not have, and
	// choices made there.
	await page.addInitScript(() => {
		if (sessionStorage.getItem('seeded')) return;
		sessionStorage.setItem('seeded', '1');
		localStorage.setItem(
			'komiq-setup',
			JSON.stringify({
				current: 'lan',
				signup: 'open',
				added: ['old-reader'],
				extra: { access: 'internet', domain: 'old.example.invalid' },
			}),
		);
	});
	await page.goto('/admin/setup');
	await expect(page.getByRole('heading', { name: 'Set up your server' })).toBeVisible();
	await expect
		.poll(() => page.evaluate(() => JSON.parse(localStorage.getItem('komiq-setup') ?? 'null')))
		.toEqual({ current: 'welcome', signup: 'closed', added: [], extra: {} });
});

test('a finished Cloudflare setup is still done after the server restarts', async ({ page }) => {
	const state = await installAdminSetupFixtures(page);
	await reachAccessStep(page);
	await reachInternetStep(page);
	await page.getByLabel(/^Domain/).fill('read.example.invalid');
	await page.getByLabel(/^Cloudflare API token/).fill('made-up-token');
	await page.getByRole('button', { name: 'Set up', exact: true }).click();
	await expect(page.getByRole('heading', { name: 'Your server is online' })).toBeVisible();
	await expect(page.getByRole('list', { name: 'Cloudflare setup' })).toBeVisible();
	expect(state.accessMode).toBe('internet');

	// The server restarts: it keeps what setup made and forgets the run. The step does
	// not ask for a token again.
	state.job = null;
	await page.reload();
	await expect(page.getByRole('heading', { name: 'Your server is online' })).toBeVisible();
	await expect(page.getByText('Readers can now open https://read.example.invalid.')).toBeVisible();
	await expect(page.getByRole('list', { name: 'Cloudflare setup' })).toHaveCount(0);
	await expect(page.getByLabel(/^Cloudflare API token/)).toHaveCount(0);
	await page.getByRole('button', { name: 'Continue' }).click();

	// The summary shows the address the server has, with its QR code.
	await reachDone(page);
	await expect(summaryRow(page, 'Reader')).toContainText('https://read.example.invalid');
	await expect(
		page.getByRole('img', { name: 'QR code for https://read.example.invalid' }),
	).toBeVisible();
	await expect(page.getByText(/Internet access is not set up yet/)).toHaveCount(0);
});

test('the internet step can wait: the summary shows the address that works', async ({ page }) => {
	const state = await installAdminSetupFixtures(page);
	await reachAccessStep(page);
	await reachInternetStep(page);
	// Typed, never set up: it must not reach the summary as the reader address.
	await page.getByLabel(/^Domain/).fill('https://read.example.invalid/');
	await page.getByRole('button', { name: 'Set up later' }).click();

	await reachDone(page);
	expect(state.accessMode).toBeNull();
	expect(state.calls.some((c) => c.op === 'StartCloudflareSetup')).toBe(false);
	await expect(summaryRow(page, 'Reader')).toContainText(new URL(page.url()).origin);
	await expect(page.getByText(/read\.example\.invalid/)).toHaveCount(0);
	await expect(
		page.getByText('Internet access is not set up yet. You can finish it in Settings › Access.'),
	).toBeVisible();
});

test('a failed Cloudflare setup that is skipped is not shown as the reader address', async ({
	page,
}) => {
	const state = await installAdminSetupFixtures(page, { cloudflareFails: true });
	await reachAccessStep(page);
	await reachInternetStep(page);
	await page.getByLabel(/^Domain/).fill('read.example.invalid');
	await page.getByLabel(/^Cloudflare API token/).fill('made-up-token');
	await page.getByRole('button', { name: 'Set up', exact: true }).click();
	await expect(page.getByText('Cloudflare refused the DNS record.')).toBeVisible();

	// After a reload the token is gone (it is never saved): the retry asks for it, and
	// the field stays while it is typed into.
	await page.reload();
	await expect(page.getByText('Cloudflare refused the DNS record.')).toBeVisible();
	const token = page.getByLabel(/^Cloudflare API token/);
	await expect(token).toBeVisible();
	await token.pressSequentially('made-up');
	await expect(token).toHaveValue('made-up');

	await page.getByRole('button', { name: 'Skip for now' }).click();
	await reachDone(page);
	expect(state.accessMode).toBeNull();
	await expect(summaryRow(page, 'Reader')).toContainText(new URL(page.url()).origin);
	await expect(page.getByText(/https:\/\/read\.example\.invalid/)).toHaveCount(0);
	await expect(page.getByText(/Internet access is not set up yet/)).toBeVisible();
});

test('the summary shows the sign-up setting the server has, not the choice it refused', async ({
	page,
}) => {
	// Sign-up is pinned open by the server's environment.
	const state = await installAdminSetupFixtures(page, { registrationPinned: true });
	await reachAccessStep(page);
	await page.getByRole('radio', { name: /Only my devices/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('radio', { name: /Only people I add/ })).toHaveAttribute(
		'aria-checked',
		'true',
	);
	await reachDone(page);
	expect(state.registrationOpen).toBe(true);
	await expect(summaryRow(page, 'Sign-up')).toContainText('Anyone can sign up');
	await expect(page.getByText(/Sign-up is fixed by this server’s configuration/)).toBeVisible();
});

test('the address step can wait: nothing is saved and the summary says so', async ({ page }) => {
	const state = await installAdminSetupFixtures(page);
	await reachAccessStep(page);
	await page.getByRole('radio', { name: /Public without a domain/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Public without a domain' })).toBeVisible();
	// Typed, never saved (port 80 is still closed, say): it is not the reader address.
	await page.getByLabel('Server address').fill('203.0.113.5');
	await page.getByRole('button', { name: 'Set up later' }).click();

	await reachDone(page);
	expect(state.accessMode).toBeNull();
	expect(state.calls.some((c) => c.op.startsWith('SetAccessMode'))).toBe(false);
	await expect(summaryRow(page, 'Reader')).toContainText(new URL(page.url()).origin);
	await expect(page.getByText(/203\.0\.113\.5/)).toHaveCount(0);
	await expect(
		page.getByText('Internet access is not set up yet. You can finish it in Settings › Access.'),
	).toBeVisible();
});

test('the internet step shows the address the server stored, not what was typed', async ({
	page,
}) => {
	const state = await installAdminSetupFixtures(page);
	await reachAccessStep(page);
	await reachInternetStep(page);
	await page.getByLabel(/^Domain/).fill('https://Read.Example.invalid/');
	await page.getByLabel(/^Cloudflare API token/).fill('made-up-token');
	await page.getByRole('button', { name: 'Set up', exact: true }).click();
	await expect(page.getByRole('heading', { name: 'Your server is online' })).toBeVisible();
	expect(state.accessDomain).toBe('read.example.invalid');
	await expect(page.getByText('Readers can now open https://read.example.invalid.')).toBeVisible();
	await expect(page.getByText(/https:\/\/https:/)).toHaveCount(0);
});

test('the internet step shows a Cloudflare setup that was turned off, and turns it on', async ({
	page,
}) => {
	const state = await installAdminSetupFixtures(page);
	await reachAccessStep(page);
	await reachInternetStep(page);
	await page.getByLabel(/^Domain/).fill('read.example.invalid');
	await page.getByLabel(/^Cloudflare API token/).fill('made-up-token');
	await page.getByRole('button', { name: 'Set up', exact: true }).click();
	await expect(page.getByRole('heading', { name: 'Your server is online' })).toBeVisible();

	// Back to the access choice: "Only my devices" turns the setup off on the server,
	// and keeps it.
	await page.getByRole('button', { name: 'Back' }).click();
	await page.getByRole('radio', { name: /Only my devices/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Who can sign in?' })).toBeVisible();
	expect(state.accessMode).toBe('local');
	expect(state.cloudflare).toMatchObject({ domain: 'read.example.invalid', complete: true });

	// The internet step again: not "online", and no token is asked for to turn it on.
	await page.getByRole('button', { name: 'Back' }).click();
	await reachInternetStep(page);
	await expect(
		page.getByText('Cloudflare is set up for read.example.invalid and turned off.'),
	).toBeVisible();
	await expect(page.getByText(/Readers can now open/)).toHaveCount(0);
	await expect(page.getByLabel(/^Cloudflare API token/)).toHaveCount(0);
	await expect(page.getByRole('button', { name: 'Set it up again' })).toBeVisible();
	await page.getByRole('button', { name: 'Turn on' }).click();
	await expect(page.getByRole('heading', { name: 'Your server is online' })).toBeVisible();
	await expect(page.getByText('Readers can now open https://read.example.invalid.')).toBeVisible();
	expect(state.accessMode).toBe('internet');
	expect(state.calls.findLast((c) => c.op === 'SetAccessMode')?.variables).toEqual({
		mode: 'internet',
		domain: 'read.example.invalid',
	});
});

/** A server that is set up, with its owner signed in: Settings › Access. */
async function openAccessSettings(page: Page, initial: Record<string, unknown> = {}) {
	const state = await installAdminSetupFixtures(page, {
		owner: { id: 'owner-1', username: 'owner', email: 'owner@example.invalid' },
		...initial,
	});
	await page.addInitScript(() => localStorage.setItem('komika-admin-token', 'fixture-owner-token'));
	await page.goto('/admin/settings');
	const access = page.getByRole('region', { name: 'Access' });
	await expect(access.getByText('Mode', { exact: true })).toBeVisible();
	/** The value next to a name in the list of facts. */
	const fact = (name: string) => access.locator(`dt:text-is("${name}") + dd`);
	return { state, access, fact };
}

/** What Cloudflare setup made, as the server reports it. */
const CLOUDFLARE_SETUP = {
	domain: 'read.example.invalid',
	connection: 'TUNNEL',
	zoneName: 'example.invalid',
	complete: true,
	removable: true,
};

test('Settings: the server address follows the encryption switch', async ({ page }) => {
	const { state, access } = await openAccessSettings(page, {
		accessMode: 'direct',
		accessEncrypted: true,
		accessAddress: 'https://203.0.113.5',
	});
	await access.getByRole('button', { name: 'Change the address' }).click();
	const address = access.getByLabel('Server address');
	const encrypt = access.getByRole('switch', { name: 'Encrypt the connection' });
	await expect(address).toHaveValue('203.0.113.5');
	await expect(encrypt).toHaveAttribute('aria-checked', 'true');

	// Off: readers use the server's HTTP port. The bare address would be port 80.
	await encrypt.click();
	await expect(address).toHaveValue('203.0.113.5:8080');
	await encrypt.click();
	await expect(address).toHaveValue('203.0.113.5');

	// What the owner typed keeps its host: only the port is added or dropped, and a
	// port they typed comes back with the switch.
	await address.fill('198.51.100.7');
	await encrypt.click();
	await expect(address).toHaveValue('198.51.100.7:8080');
	await address.fill('198.51.100.7:9000');
	await encrypt.click();
	await expect(address).toHaveValue('198.51.100.7');
	await encrypt.click();
	await expect(address).toHaveValue('198.51.100.7:9000');

	await access.getByRole('button', { name: 'Save' }).click();
	await expect.poll(() => state.accessAddress).toBe('http://198.51.100.7:9000');
	expect(state.accessEncrypted).toBe(false);
});

test('Settings: "Only my devices" turns a Cloudflare setup off, and "Turn on" brings it back', async ({
	page,
}) => {
	const { state, access, fact } = await openAccessSettings(page, {
		accessMode: 'internet',
		accessDomain: 'read.example.invalid',
		cloudflare: CLOUDFLARE_SETUP,
	});
	await expect(fact('Mode')).toHaveText('Public on the internet');
	await expect(fact('Connector')).toBeVisible();
	await expect(access.getByRole('button', { name: 'Turn on' })).toHaveCount(0);

	// One confirm step says what stops. Cancel changes nothing.
	await access.getByRole('button', { name: 'Only my devices' }).click();
	const dialog = page.getByRole('dialog', { name: 'Switch to "Only my devices"?' });
	await expect(dialog).toContainText(
		'The tunnel is turned off. Readers on the internet can no longer open https://read.example.invalid. The DNS record and the other things at Cloudflare stay until you choose "Remove".',
	);
	await dialog.getByRole('button', { name: 'Cancel' }).click();
	await expect(dialog).toHaveCount(0);
	expect(state.accessMode).toBe('internet');

	await access.getByRole('button', { name: 'Only my devices' }).click();
	await dialog.getByRole('button', { name: 'Switch' }).click();
	await expect.poll(() => state.accessMode).toBe('local');
	await expect(fact('Mode')).toHaveText('Only my devices');

	// The setup is kept and shown as turned off: no token is needed to turn it on.
	await expect(fact('Cloudflare')).toHaveText(
		/Cloudflare Tunnel for read\.example\.invalid \(zone example\.invalid\)\s*Turned off/,
	);
	await expect(fact('Connector')).toHaveCount(0);
	await expect(access.getByRole('button', { name: 'Only my devices' })).toHaveCount(0);
	await expect(access.getByRole('button', { name: 'Run setup again' })).toHaveCount(0);
	await expect(access.getByRole('button', { name: 'Remove' })).toBeVisible();
	await access.getByRole('button', { name: 'Turn on' }).click();
	await expect.poll(() => state.accessMode).toBe('internet');
	expect(state.calls.findLast((c) => c.op === 'SetAccessMode')?.variables).toEqual({
		mode: 'internet',
		domain: 'read.example.invalid',
	});
	await expect(fact('Mode')).toHaveText('Public on the internet');
	await expect(fact('Cloudflare')).not.toContainText('Turned off');
	await expect(fact('Connector')).toBeVisible();
	await expect(access.getByRole('button', { name: 'Run setup again' })).toBeVisible();
	expect(state.calls.some((c) => c.op === 'RetryCloudflareSetup')).toBe(false);
});

test('Settings: "Only my devices" is the step between direct HTTPS and a Cloudflare public IP', async ({
	page,
}) => {
	// The server serves HTTPS at its own address; a Cloudflare public IP setup made
	// earlier is kept, turned off. It would use the same port.
	const refusal =
		'This server serves HTTPS at its own address, on the port Cloudflare would use. Choose "Only my devices" first, or use a Tunnel.';
	const { state, access, fact } = await openAccessSettings(page, {
		accessMode: 'direct',
		accessEncrypted: true,
		accessAddress: 'https://203.0.113.5',
		cloudflare: { ...CLOUDFLARE_SETUP, connection: 'PUBLIC_IP' },
		internetRefused: refusal,
	});
	await expect(fact('Mode')).toHaveText('Public without a domain');
	await expect(fact('Cloudflare')).toHaveText(/Public IP for read\.example\.invalid.*Turned off/);
	// The HTTPS that runs is the direct mode's, not the setup's.
	await expect(fact('HTTPS')).toHaveCount(0);
	await expect(access.getByRole('button', { name: 'Change the address' })).toBeVisible();

	// From the direct mode, "Turn on" takes the place of the address readers use: it asks.
	await access.getByRole('button', { name: 'Turn on' }).click();
	const turnOn = page.getByRole('dialog', { name: 'Turn Cloudflare on?' });
	await expect(turnOn).toContainText(
		'Readers then use https://read.example.invalid. https://203.0.113.5 stops being the reader address, and its HTTPS is turned off.',
	);
	await turnOn.getByRole('button', { name: 'Turn on' }).click();
	await expect(access.getByRole('alert')).toHaveText(refusal);
	expect(state.accessMode).toBe('direct');

	await access.getByRole('button', { name: 'Only my devices' }).click();
	const dialog = page.getByRole('dialog', { name: 'Switch to "Only my devices"?' });
	await expect(dialog).toContainText(
		'HTTPS on this server is turned off. Readers on the internet can no longer open https://203.0.113.5.',
	);
	await expect(dialog).not.toContainText('Cloudflare');
	await dialog.getByRole('button', { name: 'Switch' }).click();
	await expect.poll(() => state.accessMode).toBe('local');
	await expect(fact('Mode')).toHaveText('Only my devices');
	await expect(fact('Encryption')).toHaveCount(0);
	await expect(access.getByRole('alert')).toHaveCount(0);
	await expect(access.getByRole('button', { name: 'Use without a domain' })).toBeVisible();

	state.internetRefused = null;
	await access.getByRole('button', { name: 'Turn on' }).click();
	await expect(fact('Mode')).toHaveText('Public on the internet');
	await expect(fact('HTTPS')).toBeVisible();
});

test('Settings: Cloudflare set up by hand is saved, with the domain as the server stores it', async ({
	page,
}) => {
	const { state, access, fact } = await openAccessSettings(page, { accessMode: 'local' });
	await expect(access.getByRole('button', { name: 'Only my devices' })).toHaveCount(0);
	await access.getByRole('button', { name: 'Set up Cloudflare' }).click();
	await access.getByRole('button', { name: 'I will do it by hand' }).click();

	await access.getByRole('button', { name: 'Save' }).click();
	await expect(access.getByText('Enter the domain readers will use.')).toBeVisible();
	expect(state.accessMode).toBe('local');

	// The steps name the domain as it will be stored, not as it was typed.
	await access.getByLabel(/^Domain/).fill('https://Read.Example.invalid/');
	await expect(
		access.getByText('Open https://read.example.invalid/health. It should say ok.'),
	).toBeVisible();
	await access.getByRole('button', { name: 'Save' }).click();
	await expect.poll(() => state.accessMode).toBe('internet');
	expect(state.accessDomain).toBe('read.example.invalid');

	// The panel closes on the new status.
	await expect(access.getByLabel(/^Domain/)).toHaveCount(0);
	await expect(fact('Mode')).toHaveText('Public on the internet');
	await expect(access.getByRole('link', { name: 'https://read.example.invalid' })).toBeVisible();
	await expect(fact('Cloudflare')).toHaveText('Not set up');
	await expect(access.getByRole('button', { name: 'Only my devices' })).toBeVisible();
});
