import { test, expect, type Page } from '@playwright/test';
import { GOOD_SETUP_CODE, installAdminSetupFixtures } from '../fixtures/admin-setup.mjs';

// Runs against the DESKTOP admin build; see admin-desktop.config.ts. The app is a remote
// admin here: its server is an address it saved, not a server it runs itself.

/** Where the app reached the server so far, and where the server answers with HTTPS on. */
const OLD = 'http://203.0.113.5:8080';
const NEW = 'https://203.0.113.5';
/** Another server: an address the owner did not type. */
const OTHER = 'https://198.51.100.7';
const TOKEN = 'fixture-owner-token';
const OWNER = { id: 'owner-1', username: 'owner', email: 'owner@example.invalid' };

/**
 * The app with a remote server saved at `at`, its plain HTTP address unless said. Every
 * request to a made-up address is answered here or refused: none leaves this machine.
 *
 * `health.failing`: how many of the next HTTPS `/health` requests get no answer.
 * `health.asked`: the origins the app asked so far.
 */
async function remoteServer(page: Page, initial: Record<string, unknown> = {}, at = OLD) {
	const health = { failing: Infinity, asked: [] as string[] };
	// Routes added later are asked first, so this one is the last resort.
	await page.route(/^https?:\/\/((203\.0\.113|198\.51\.100)\.\d+|\[2001:db8:)/, (route) =>
		route.abort('connectionrefused'),
	);
	const state = await installAdminSetupFixtures(page, initial);
	await page.route(/^https:\/\/[^/]+\/health$/, (route) => {
		health.asked.push(new URL(route.request().url()).origin);
		if (health.failing > 0) {
			health.failing--;
			return route.abort('connectionrefused');
		}
		return route.fulfill({ headers: { 'access-control-allow-origin': '*' }, body: 'ok' });
	});
	// Once per tab: a reload must find what the app saved, not this again.
	await page.addInitScript(
		([server, token]) => {
			if (sessionStorage.getItem('seeded')) return;
			sessionStorage.setItem('seeded', '1');
			localStorage.setItem('komiq-server', server as string);
			if (token) localStorage.setItem('komika-admin-token', token);
		},
		[at, initial.owner ? TOKEN : null],
	);
	return { state, health };
}

/** The server the app saved, and its session. */
const saved = (page: Page) =>
	page.evaluate(() => ({
		server: localStorage.getItem('komiq-server'),
		token: localStorage.getItem('komika-admin-token'),
	}));

/** The app reloaded on the new address: its first request there. */
const firstRequestAtNew = (page: Page) =>
	page.waitForRequest((request) => request.url().startsWith(`${NEW}/graphql`));

/** Settings › Access: the owner makes the server public at 203.0.113.5, with HTTPS. */
async function turnOnHttps(page: Page) {
	await page.goto('/settings');
	const access = page.getByRole('region', { name: 'Access' });
	await access.getByRole('button', { name: 'Use without a domain' }).click();
	// The form starts from the address the app is on.
	await expect(access.getByLabel('Server address')).toHaveValue('203.0.113.5:8080');
	await access.getByLabel('Server address').fill('203.0.113.5');
	await access.getByRole('button', { name: 'Save' }).click();
	return access;
}

/** Leave Settings and come back, without a reload: Access asks the server again. */
async function reopenSettings(page: Page): Promise<void> {
	const nav = page.getByRole('navigation', { name: 'Admin navigation' });
	await nav.getByRole('link', { name: 'Users' }).click();
	await expect(page).toHaveURL(/\/users$/);
	await nav.getByRole('link', { name: 'Settings' }).click();
	await expect(page).toHaveURL(/\/settings$/);
}

test('the app follows the server to HTTPS once the new address answers', async ({ page }) => {
	const { state, health } = await remoteServer(page, { owner: OWNER });
	// The HTTPS port needs a moment: the first two tries get no answer.
	health.failing = 2;
	const reloaded = firstRequestAtNew(page);
	const access = await turnOnHttps(page);
	await reloaded;

	// Same page, same session, new address.
	await expect(access.getByRole('link', { name: NEW })).toBeVisible();
	await expect(page).toHaveURL(/\/settings$/);
	expect(state.accessAddress).toBe(NEW);
	expect(health.asked).toEqual([NEW, NEW, NEW]);
	expect(await saved(page)).toEqual({ server: NEW, token: TOKEN });
	await expect(access.getByRole('button', { name: 'Try again' })).toHaveCount(0);
});

test('no answer at the new address: the app says so, and "Try again" follows', async ({ page }) => {
	const { health } = await remoteServer(page, { owner: OWNER });
	const access = await turnOnHttps(page);

	// It gives up after a few tries, stays where it works and says what to do.
	const notice = access.getByRole('alert');
	await expect(notice).toContainText(
		`This app still uses the old address, without encryption. It got no answer at ${NEW}. Check that port 443 is open in your cloud firewall, then try again. After the first visit over HTTPS, the old address stops answering from the internet.`,
		{ timeout: 15_000 },
	);
	expect(health.asked).toEqual([NEW, NEW, NEW, NEW]);
	expect(await saved(page)).toEqual({ server: OLD, token: TOKEN });
	await expect(page).toHaveURL(/\/settings$/);

	// The port opens while the app tries again: one more miss, then an answer.
	health.failing = 1;
	const reloaded = firstRequestAtNew(page);
	await notice.getByRole('button', { name: 'Try again' }).click();
	await reloaded;
	await expect(access.getByRole('link', { name: NEW })).toBeVisible();
	expect(health.asked).toHaveLength(6);
	expect(await saved(page)).toEqual({ server: NEW, token: TOKEN });
	await expect(access.getByRole('alert')).toHaveCount(0);
});

test('an answer that names another host than the owner typed is not followed', async ({ page }) => {
	const { state, health } = await remoteServer(page, { owner: OWNER });
	// That other server is up: it would answer.
	health.failing = 0;
	// Someone on the network path changes the request, so the answer names their server.
	await page.route(
		(url) => url.pathname === '/graphql',
		(route) => {
			const body = route.request().postDataJSON();
			if (!/mutation\s+SetAccessMode/.test(body?.query ?? '')) return route.fallback();
			return route.fallback({
				postData: JSON.stringify({
					...body,
					variables: { ...body.variables, domain: '198.51.100.7' },
				}),
			});
		},
	);
	const access = await turnOnHttps(page);

	// The save is over, the move was not made and not even tried.
	await expect(page.getByText('Readers now use this address.')).toBeVisible();
	expect(state.accessAddress).toBe(OTHER);
	await expect(access.getByRole('link', { name: OTHER })).toBeVisible();
	expect(health.asked).toEqual([]);
	expect(await saved(page)).toEqual({ server: OLD, token: TOKEN });
	await expect(access.getByRole('button', { name: 'Try again' })).toHaveCount(0);
});

test('"Try again" goes to the address the owner typed, not to one a later answer names', async ({
	page,
}) => {
	const { state, health } = await remoteServer(page, { owner: OWNER });
	const access = await turnOnHttps(page);
	const again = access.getByRole('button', { name: 'Try again' });
	await expect(again).toBeVisible({ timeout: 15_000 });

	// A later answer over plain HTTP names another server, which is up. Nothing offers
	// to go there.
	state.accessAddress = OTHER;
	health.failing = 0;
	await reopenSettings(page);
	await expect(access.getByRole('link', { name: OTHER })).toBeVisible();
	await expect(again).toHaveCount(0);

	// The true answer again: the notice is back, for the address of the first try.
	state.accessAddress = NEW;
	await reopenSettings(page);
	await expect(access.getByRole('alert')).toContainText(`It got no answer at ${NEW}.`);
	const reloaded = firstRequestAtNew(page);
	await again.click();
	await reloaded;
	await expect(access.getByRole('link', { name: NEW })).toBeVisible();
	expect(health.asked).not.toContain(OTHER);
	expect(await saved(page)).toEqual({ server: NEW, token: TOKEN });
});

test('setup: the summary says the app could not move, and "Try again" moves it', async ({
	page,
}) => {
	const { state, health } = await remoteServer(page);

	await page.goto('/');
	await expect(page).toHaveURL(/\/setup$/);
	await page.getByRole('button', { name: 'Get started' }).click();
	await page.getByLabel(/^Username/).fill('owner');
	await page.getByLabel(/^Email/).fill('owner@example.invalid');
	await page.getByLabel(/^Password/).fill('made-up-password');
	await page.getByLabel(/^Confirm/).fill('made-up-password');
	await page.getByLabel(/^Setup code/).fill(GOOD_SETUP_CODE);
	await page.getByRole('button', { name: 'Create account' }).click();
	await page.getByRole('radio', { name: /Public without a domain/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Public without a domain' })).toBeVisible();
	await page.getByLabel('Server address').fill('203.0.113.5');
	await page.getByRole('button', { name: 'Continue' }).click();

	// The wizard waits while the app tries the new address, then goes on.
	await expect(page.getByRole('heading', { name: 'Who can sign in?' })).toBeVisible({
		timeout: 15_000,
	});
	expect(state.accessAddress).toBe(NEW);
	await page.getByRole('button', { name: 'Continue' }).click();
	await expect(page.getByRole('heading', { name: 'Sources' })).toBeVisible();
	await page.getByRole('button', { name: 'Skip' }).click();
	await expect(page.getByRole('heading', { name: 'You are all set' })).toBeVisible();

	const notice = page.getByRole('alert');
	await expect(notice).toContainText(`It got no answer at ${NEW}.`, { timeout: 15_000 });
	expect(health.asked).toEqual([NEW, NEW, NEW, NEW]);
	expect(await saved(page)).toEqual({ server: OLD, token: TOKEN });

	// The port is open now: the app moves, and the wizard is still on its last step.
	health.failing = 0;
	const reloaded = firstRequestAtNew(page);
	await notice.getByRole('button', { name: 'Try again' }).click();
	await reloaded;
	await expect(page.getByRole('heading', { name: 'You are all set' })).toBeVisible();
	await expect(page.getByRole('alert')).toHaveCount(0);
	expect(await saved(page)).toEqual({ server: NEW, token: TOKEN });
});

test('setup: the app moves before the wizard goes on, and opens the next step', async ({
	page,
}) => {
	const { health } = await remoteServer(page);
	// The HTTPS port needs a moment: the first try gets no answer.
	health.failing = 1;

	await page.goto('/');
	await page.getByRole('button', { name: 'Get started' }).click();
	await page.getByLabel(/^Username/).fill('owner');
	await page.getByLabel(/^Email/).fill('owner@example.invalid');
	await page.getByLabel(/^Password/).fill('made-up-password');
	await page.getByLabel(/^Confirm/).fill('made-up-password');
	await page.getByLabel(/^Setup code/).fill(GOOD_SETUP_CODE);
	await page.getByRole('button', { name: 'Create account' }).click();
	await page.getByRole('radio', { name: /Public without a domain/ }).click();
	await page.getByRole('button', { name: 'Continue' }).click();
	await page.getByLabel('Server address').fill('203.0.113.5');
	const reloaded = firstRequestAtNew(page);
	await page.getByRole('button', { name: 'Continue' }).click();

	// No later step shows on the old address: a reload there would lose what it holds.
	await expect(page.getByRole('heading', { name: 'Public without a domain' })).toBeVisible();
	await reloaded;
	await expect(page.getByRole('heading', { name: 'Who can sign in?' })).toBeVisible();
	expect(await saved(page)).toEqual({ server: NEW, token: TOKEN });
});

test('"Only my devices" says that an app on a public address loses its way in', async ({
	page,
}) => {
	await remoteServer(page, { owner: OWNER, accessMode: 'direct', accessAddress: OLD });
	await page.goto('/settings');
	const access = page.getByRole('region', { name: 'Access' });
	await access.getByRole('button', { name: 'Only my devices' }).click();
	await expect(page.getByRole('dialog', { name: 'Switch to "Only my devices"?' })).toContainText(
		'The HTTP port stops answering clients on the internet. This console is open at a public address. After the switch it stops answering here. Open it on your home network, or run "komika-server setup" on the server to change the mode.',
	);
});

test('an HTTPS address changed to another one: the notice does not speak of encryption', async ({
	page,
}) => {
	// The app is on the HTTPS address already, and the owner moves the server to another.
	await remoteServer(
		page,
		{ owner: OWNER, accessMode: 'direct', accessEncrypted: true, accessAddress: NEW },
		NEW,
	);
	await page.goto('/settings');
	const access = page.getByRole('region', { name: 'Access' });
	await access.getByRole('button', { name: 'Change the address' }).click();
	await expect(access.getByLabel('Server address')).toHaveValue('203.0.113.5');
	await access.getByLabel('Server address').fill('203.0.113.9');
	await access.getByRole('button', { name: 'Save' }).click();

	const notice = access.getByRole('alert');
	await expect(notice).toContainText(
		'This app still uses the old address. It got no answer at https://203.0.113.9. Check that port 443 is open in your cloud firewall, then try again.',
		{ timeout: 15_000 },
	);
	// Nothing about plain HTTP: the old address was not plain.
	await expect(notice).not.toContainText('stops answering');
	await expect(notice.getByRole('button', { name: 'Try again' })).toBeVisible();
	expect(await saved(page)).toEqual({ server: NEW, token: TOKEN });
});

test('a bare IPv6 address: the app follows to it too', async ({ page }) => {
	// The server stores it in brackets; the owner typed it without.
	const V6 = 'https://[2001:db8::1]';
	const { health } = await remoteServer(page, { owner: OWNER });
	health.failing = 0;
	const reloaded = page.waitForRequest((request) => request.url().startsWith(`${V6}/graphql`));
	await page.goto('/settings');
	const access = page.getByRole('region', { name: 'Access' });
	await access.getByRole('button', { name: 'Use without a domain' }).click();
	await access.getByLabel('Server address').fill('2001:db8::1');
	await access.getByRole('button', { name: 'Save' }).click();
	await reloaded;

	expect(health.asked).toEqual([V6]);
	expect(await saved(page)).toEqual({ server: V6, token: TOKEN });
});

test('a try that answers late does not undo a newer change of address', async ({ page }) => {
	await remoteServer(page, { owner: OWNER });
	const access = await turnOnHttps(page);
	const notice = access.getByRole('alert');
	const again = notice.getByRole('button', { name: 'Try again' });
	await expect(again).toBeVisible({ timeout: 15_000 });

	// The first address answers this try, but late.
	let answer!: () => void;
	const late = new Promise<void>((resolve) => (answer = resolve));
	await page.route(`${NEW}/health`, async (route) => {
		await late;
		await route.fulfill({ headers: { 'access-control-allow-origin': '*' }, body: 'ok' });
	});
	const asked = page.waitForRequest(`${NEW}/health`);
	await again.click();
	await asked;

	// Meanwhile the owner moves the server to another address, which does not answer yet.
	const askedOther = page.waitForRequest(`${OTHER}/health`);
	await access.getByRole('button', { name: 'Change the address' }).click();
	await access.getByLabel('Server address').fill('198.51.100.7');
	await access.getByRole('button', { name: 'Save' }).click();
	await askedOther;
	answer();

	// The app did not go to the address that was left. It reports the newer one.
	await expect(notice).toContainText(`It got no answer at ${OTHER}.`, { timeout: 15_000 });
	expect(await saved(page)).toEqual({ server: OLD, token: TOKEN });
});
