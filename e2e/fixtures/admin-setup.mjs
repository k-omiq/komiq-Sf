/**
 * A fake self-host server for the admin setup wizard: answers `/graphql` from an
 * in-memory state that the wizard changes (owner created, sign-up switch, users
 * added). Nothing reaches a real server. Every value here is made up.
 *
 * Returns the state, so a test can assert what the wizard sent.
 */
export const GOOD_SETUP_CODE = 'TEST-CODE-0001';

export async function installAdminSetupFixtures(page, initial = {}) {
	const at = '2026-10-04T00:00:00Z';
	const state = {
		owner: null,
		registrationOpen: true,
		users: [],
		calls: [],
		mangadexEnabled: false,
		repos: [],
		extensions: [],
		...initial,
	};
	// What a made-up repo lists once added (B5 Sources step).
	const repoExtensions = (indexUrl) =>
		[
			['Alpha Scans', 'en', false],
			['Beta Comics', 'en', false],
			['Gamma Manga', 'es', false],
			['Delta Toons', 'fr', true],
		].map(([name, lang, isNsfw], i) => ({
			pkgName: `test.extension.${lang}.${name.split(' ')[0].toLowerCase()}`,
			name,
			lang,
			versionName: `1.4.${i}`,
			isInstalled: false,
			hasUpdate: false,
			isNsfw,
			iconUrl: null,
			repo: indexUrl,
			subscribed: false,
		}));
	const sessionUser = (u) => ({
		id: u.id,
		username: u.username,
		displayName: null,
		bio: null,
		avatarUrl: null,
		isAdmin: true,
		showNsfw: false,
		joinedAt: at,
		mustChangePassword: false,
	});
	const ok = (data) => ({ headers: { 'access-control-allow-origin': '*' }, json: { data } });
	const fail = (message) => ({
		headers: { 'access-control-allow-origin': '*' },
		json: { data: null, errors: [{ message }] },
	});

	await page.route(
		(url) => url.pathname === '/graphql',
		async (route) => {
			const req = route.request();
			const body =
				req.method() === 'POST'
					? req.postDataJSON()
					: Object.fromEntries(new URL(req.url()).searchParams);
			const q = body.query || '';
			const v =
				typeof body.variables === 'string' ? JSON.parse(body.variables) : body.variables || {};
			const op = q.match(/(?:query|mutation)\s+(\w+)/)?.[1] ?? '';
			state.calls.push({ op, variables: v });
			const authed = !!req.headers()['authorization'] && state.owner;

			switch (op) {
				case 'Instance':
					return route.fulfill(
						ok({
							instance: {
								setupRequired: !state.owner,
								name: 'komiq',
								version: '0.0.0-test',
								registrationOpen: state.registrationOpen,
								mangadexEnabled: false,
							},
						}),
					);
				case 'Session':
					return route.fulfill(
						ok({
							session: authed
								? { token: 'fixture-owner-token', user: sessionUser(state.owner) }
								: null,
						}),
					);
				case 'CreateOwner':
					if (state.owner) return route.fulfill(fail('This server already has an owner.'));
					if (v.setupCode !== GOOD_SETUP_CODE) return route.fulfill(fail('Invalid setup code'));
					state.owner = { id: 'owner-1', username: v.username, email: v.email };
					return route.fulfill(
						ok({
							createOwner: { token: 'fixture-owner-token', user: sessionUser(state.owner) },
						}),
					);
				case 'SetRegistrationOpen':
					state.registrationOpen = v.open;
					return route.fulfill(ok({ setRegistrationOpen: v.open }));
				case 'CreateUser': {
					const u = {
						id: `user-${state.users.length + 1}`,
						username: v.input.username,
						email: v.input.email,
						avatarUrl: null,
						isAdmin: !!v.input.isAdmin,
						isBanned: false,
						createdAt: at,
					};
					state.users.push(u);
					return route.fulfill(ok({ createUser: u }));
				}
				case 'MangadexSetting':
					return route.fulfill(
						ok({
							mangadexSetting: { enabled: state.mangadexEnabled, pinned: false, pinnedBy: null },
						}),
					);
				case 'SetMangadexEnabled':
					state.mangadexEnabled = v.enabled;
					return route.fulfill(ok({ setMangadexEnabled: v.enabled }));
				case 'ExtensionRepos':
					return route.fulfill(ok({ extensionRepos: state.repos }));
				case 'AddExtensionRepo':
					state.repos.push({ indexUrl: v.indexUrl, name: 'Test Repo', badgeLabel: 'TST' });
					state.extensions.push(...repoExtensions(v.indexUrl));
					return route.fulfill(ok({ addExtensionRepo: state.extensions.length }));
				case 'Extensions':
					return route.fulfill(ok({ extensions: state.extensions }));
				case 'InstallExtension': {
					const ext = state.extensions.find((e) => e.pkgName === v.pkgName);
					ext.isInstalled = true;
					return route.fulfill(ok({ installExtension: ext }));
				}
				default: {
					// Whatever the console opens after setup: an empty catalogue.
					const empty = { items: [], page: 1, hasNextPage: false, total: 0 };
					return route.fulfill(
						ok({ search: empty, sourceScanHealth: [], users: empty, scanStatus: null }),
					);
				}
			}
		},
	);
	return state;
}
