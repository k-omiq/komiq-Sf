/**
 * A fake self-host server for the admin setup wizard: answers `/graphql` from an
 * in-memory state that the wizard changes (owner created, sign-up switch, users
 * added). Nothing reaches a real server. Every value here is made up.
 *
 * Returns the state, so a test can assert what the wizard sent.
 */
export const GOOD_SETUP_CODE = 'TEST-CODE-0001';

/**
 * What a current server says about its own machine before access setup
 * (`accessPreflight`): the tunnel connector is there, the HTTPS ports are free, no
 * container, no public address found. A test overrides the fields it is about.
 */
export const PLAIN_PREFLIGHT = {
	cloudflared: 'found',
	cloudflaredDetail: null,
	httpPort: 8080,
	httpsPort: 443,
	httpsPortState: 'free',
	acmePort: 80,
	acmePortState: 'free',
	inContainer: false,
	publicIp: null,
};

export async function installAdminSetupFixtures(page, initial = {}) {
	const at = '2026-10-04T00:00:00Z';
	const state = {
		owner: null,
		registrationOpen: true,
		users: [],
		calls: [],
		mangadexEnabled: false,
		accessMode: null,
		accessEncrypted: false,
		// What the server stored with the access mode: the domain (internet), or the
		// address as an origin (direct).
		accessDomain: null,
		accessAddress: null,
		// What Cloudflare setup made, and its current or last run. A server that
		// restarts keeps the first and forgets the second: a test sets `job` to null.
		// "Only my devices" and the direct mode turn a setup off and keep it: the mode
		// then is not `internet`, and `cloudflare` is still there.
		cloudflare: null,
		job: null,
		// The server's own HTTPS, as `certificateStatus` reports it (`certificateExpires`,
		// `certificateError`, `httpsError`, `plainHttpClosed`). Null: nothing to report.
		certificate: null,
		// The server refuses the `internet` mode with this message (e.g. its own HTTPS
		// holds the port a Cloudflare public IP setup would use).
		internetRefused: null,
		// A Cloudflare setup run stops at a failed step.
		cloudflareFails: false,
		// Sign-up is fixed by the server's environment: it refuses the mutation.
		registrationPinned: false,
		// The owner step's safety check gets no answer (a failed request).
		setupCheckFails: false,
		repos: [],
		extensions: [],
		// Access screens. `oldServer`: a server that predates `setupNeedsTerminal`,
		// `accessPreflight`, the `connection` argument of `checkCloudflareToken` and
		// `lanClientSeenAt`; it refuses those documents the way a real one does, at
		// validation.
		oldServer: false,
		preflight: PLAIN_PREFLIGHT,
		// Per connection type: the permission rows the checked token seems to lack.
		tokenMissing: { TUNNEL: [], PUBLIC_IP: [] },
		// Every token check the console sent: { connection (null: the old document), domain }.
		tokenChecks: [],
		lanClientSeenAt: null,
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
	// As the server tidies a domain: `https://Read.Example.com/` is `read.example.com`.
	const tidyDomain = (typed) =>
		typed
			.trim()
			.toLowerCase()
			.replace(/^https?:\/\//, '')
			.split('/')[0]
			.replace(/\.+$/, '');
	const ok = (data) => ({ headers: { 'access-control-allow-origin': '*' }, json: { data } });
	const fail = (...messages) => ({
		headers: { 'access-control-allow-origin': '*' },
		json: { data: null, errors: messages.map((message) => ({ message })) },
	});
	// As the server builds it: a public address only in a public mode.
	const accessStatus = () => {
		const domain = state.accessDomain ?? state.cloudflare?.domain ?? null;
		return {
			mode: state.accessMode ?? '',
			domain,
			address:
				state.accessMode === 'internet'
					? domain && `https://${domain}`
					: state.accessMode === 'direct'
						? state.accessAddress
						: null,
			cloudflare: state.cloudflare,
			connector: 'off',
			connectorMessage: null,
			https: state.accessMode === 'direct' && state.accessEncrypted,
			trustedProxies: [],
			trustedProxiesPinned: false,
			job: state.job,
		};
	};
	const tokenCheck = {
		zoneName: 'example.invalid',
		zoneStatus: 'active',
		accountName: 'Test Account',
	};

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
				case 'SetupNeedsTerminal':
					if (state.setupCheckFails) return route.fulfill({ status: 502, body: 'Bad gateway' });
					if (state.oldServer) {
						return route.fulfill(
							fail('Unknown field "setupNeedsTerminal" on type "InstanceInfo".'),
						);
					}
					return route.fulfill(ok({ instance: { setupNeedsTerminal: !!state.plainFromInternet } }));
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
				case 'SetAccessMode':
				case 'SetAccessModeEncrypted':
					if (v.mode === 'internet' && state.internetRefused) {
						return route.fulfill(fail(state.internetRefused));
					}
					state.accessMode = v.mode;
					state.accessEncrypted = op === 'SetAccessModeEncrypted';
					if (v.mode === 'direct') {
						// Direct mode: the server stores the address as an origin.
						// A bare IPv6 address goes in brackets.
						const bare = !v.domain.startsWith('[') && v.domain.split(':').length > 2;
						state.accessAddress = state.accessEncrypted
							? `https://${bare ? `[${v.domain}]` : v.domain.replace(/:\d+$/, '')}`
							: `http://${v.domain}`;
					} else if (v.mode === 'internet' && v.domain) {
						state.accessDomain = tidyDomain(v.domain);
					}
					return route.fulfill(ok({ setAccessMode: accessStatus() }));
				case 'AccessStatus':
					return route.fulfill(ok({ accessStatus: accessStatus() }));
				case 'CertificateStatus':
					return route.fulfill(
						ok({
							accessStatus: {
								certificateExpires: null,
								certificateError: null,
								httpsError: null,
								plainHttpClosed: false,
								// What a test says about the server's own HTTPS.
								...state.certificate,
							},
						}),
					);
				case 'StartCloudflareSetup': {
					// The run is over at once: all done, or stopped at its second step.
					const failed = !!state.cloudflareFails;
					const { connection } = v.input;
					const domain = tidyDomain(v.input.domain);
					state.cloudflare = {
						domain,
						connection,
						zoneName: 'example.invalid',
						complete: !failed,
						removable: true,
					};
					state.job = {
						kind: 'setup',
						running: false,
						failed,
						steps: [
							{ id: 'token', label: 'Check the token', state: 'done', message: null },
							{
								id: 'dns',
								label: 'Add the DNS record',
								state: failed ? 'failed' : 'done',
								message: failed ? 'Cloudflare refused the DNS record.' : null,
							},
							{
								id: 'test',
								label: 'Test the address',
								state: failed ? 'waiting' : 'done',
								message: null,
							},
						],
					};
					if (!failed) {
						// As the server does when the last step is done.
						state.accessMode = 'internet';
						state.accessDomain = domain;
					}
					return route.fulfill(ok({ startCloudflareSetup: accessStatus() }));
				}
				case 'AccessPreflight':
					if (state.oldServer) {
						return route.fulfill(fail('Unknown field "accessPreflight" on type "Query".'));
					}
					return route.fulfill(ok({ accessPreflight: state.preflight }));
				case 'LanClientSeenAt':
					if (state.oldServer) {
						return route.fulfill(fail('Unknown field "lanClientSeenAt" on type "AccessStatus".'));
					}
					return route.fulfill(ok({ accessStatus: { lanClientSeenAt: state.lanClientSeenAt } }));
				case 'CheckCloudflareTokenFor': {
					// The connection is written into the document, not sent as a variable.
					const connection = q.match(/connection:\s*(\w+)/)?.[1] ?? null;
					if (state.oldServer) {
						return route.fulfill(
							fail(
								'Unknown argument "connection" on field "checkCloudflareToken" of type "Mutation".',
								'Unknown field "missing" on type "CloudflareTokenCheck".',
							),
						);
					}
					state.tokenChecks.push({ connection, domain: v.domain });
					return route.fulfill(
						ok({
							checkCloudflareToken: {
								...tokenCheck,
								missing: state.tokenMissing[connection] ?? [],
							},
						}),
					);
				}
				case 'CheckCloudflareToken':
					state.tokenChecks.push({ connection: null, domain: v.domain });
					return route.fulfill(ok({ checkCloudflareToken: tokenCheck }));
				case 'SetRegistrationOpen':
					if (state.registrationPinned) {
						return route.fulfill(
							fail(
								'Sign-up is set by KOMIKA_REGISTRATION_OPEN on this server and cannot be changed here.',
							),
						);
					}
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
					const people = state.owner
						? [{ ...state.owner, avatarUrl: null, isAdmin: true, isBanned: false, createdAt: at }]
						: [];
					people.push(...state.users);
					const users = { ...empty, items: people, total: people.length };
					return route.fulfill(
						ok({ search: empty, sourceScanHealth: [], users, scanStatus: null }),
					);
				}
			}
		},
	);
	return state;
}
