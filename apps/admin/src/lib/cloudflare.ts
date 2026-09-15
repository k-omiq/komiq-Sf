/**
 * Text for the Cloudflare screens (setup wizard, Settings › Access): the exact token
 * permissions, the link that opens Cloudflare's token form with some of them filled in,
 * what automatic setup does, and the manual steps for owners who prefer the dashboard.
 * Kept in one place so both screens and `docs/self-host-cloudflare.md` say the same
 * thing. The terminal wizard (`apps/server/src/setup_cli.rs`) has the same permission
 * rows, the same link and the same manual steps: change them together.
 */
import type { CloudflareConnection } from '@komika/api';

/** One row of the "Create Custom Token" form: scope, group, level. */
export interface Permission {
	scope: 'Account' | 'Zone';
	group: string;
	level: 'Read' | 'Edit';
	/**
	 * The row's key in Cloudflare's token template link ({@link tokenTemplateUrl}), which
	 * then fills the row in. A row without a key has no documented one: the owner adds it
	 * by hand.
	 */
	templateKey?: TemplateKey;
}

/**
 * The permission keys Cloudflare documents for a token template link, of the ones setup
 * needs, in the order the link lists them. Only documented keys go in the link: an
 * unknown key breaks the whole form.
 */
const TEMPLATE_KEYS = ['zone', 'dns', 'zone_settings', 'ssl_and_certificates'] as const;
type TemplateKey = (typeof TEMPLATE_KEYS)[number];

export function tokenPermissions(connection: CloudflareConnection): Permission[] {
	return [
		connection === 'TUNNEL'
			? { scope: 'Account', group: 'Cloudflare Tunnel', level: 'Edit' }
			: {
					scope: 'Zone',
					group: 'SSL and Certificates',
					level: 'Edit',
					templateKey: 'ssl_and_certificates',
				},
		{ scope: 'Zone', group: 'Zone', level: 'Read', templateKey: 'zone' },
		{ scope: 'Zone', group: 'DNS', level: 'Edit', templateKey: 'dns' },
		{ scope: 'Zone', group: 'Zone Settings', level: 'Edit', templateKey: 'zone_settings' },
		{ scope: 'Zone', group: 'Cache Rules', level: 'Edit' },
		// Cloudflare's Cache Rules API guide lists these two as well.
		{ scope: 'Account', group: 'Account Rulesets', level: 'Edit' },
		{ scope: 'Account', group: 'Account Filter Lists', level: 'Edit' },
	];
}

/** The Cloudflare dashboard: where an owner makes an account and adds a domain. */
export const CLOUDFLARE_DASHBOARD = 'https://dash.cloudflare.com/';

/**
 * Cloudflare's token template link: it opens "Create Custom Token" named `komiq`, with
 * the rows of {@link tokenPermissions} that have a `templateKey` filled in. The owner
 * adds the other rows there and creates the token.
 */
export function tokenTemplateUrl(connection: CloudflareConnection): string {
	const rows = tokenPermissions(connection);
	const keys = TEMPLATE_KEYS.flatMap((key) => {
		const row = rows.find((p) => p.templateKey === key);
		return row ? [{ key, type: row.level.toLowerCase() }] : [];
	});
	return (
		`${CLOUDFLARE_DASHBOARD}profile/api-tokens` +
		`?permissionGroupKeys=${encodeURIComponent(JSON.stringify(keys))}` +
		'&accountId=*&zoneId=all&name=komiq'
	);
}

/**
 * What automatic setup does for the owner, in the order of the server's checklist.
 * `connector` is what the server said about cloudflared (`AccessPreflight.cloudflared`),
 * or null when it did not say: such a server uses the one on its machine.
 */
export function automaticSteps(
	connection: CloudflareConnection,
	httpsPort = 443,
	connector: 'found' | 'download' | 'unsupported' | null = null,
): string[] {
	const route =
		connection === 'TUNNEL'
			? [
					connector === 'download'
						? 'Downloads the connector (cloudflared).'
						: connector === 'unsupported'
							? 'Uses the connector (cloudflared) once you install it.'
							: 'Uses the connector (cloudflared) on this server.',
					'Creates the tunnel.',
					'Starts the connector. It starts again with the server.',
					'Adds the DNS record that points your domain at the tunnel.',
				]
			: [
					`Gets a certificate from Cloudflare and serves HTTPS on port ${httpsPort}.`,
					'Sets the encryption mode (SSL) of the whole zone to Full (strict).',
					'Adds the DNS record that points your domain at this server.',
					'Learns each visitor’s real address from Cloudflare.',
				];
	return [
		...route,
		'Adds cache rules for covers, page images and static files.',
		'Tells Cloudflare to respect the server’s cache headers.',
		'Tests the address.',
	];
}

/** The local port the server listens on, as the console sees it. */
export function localPort(address: string): string {
	try {
		const u = new URL(address);
		return u.port || (u.protocol === 'https:' ? '443' : '80');
	} catch {
		return '8080';
	}
}

/**
 * A typed domain as the server will store it, for text that names it before it is
 * saved: `https://Read.Example.com/` is `read.example.com`.
 */
export function plainDomain(typed: string): string {
	return typed
		.trim()
		.toLowerCase()
		.replace(/^https?:\/\//, '')
		.replace(/\/.*$/, '')
		.replace(/\.+$/, '');
}

/** Dashboard steps that end in the same state automatic setup creates. */
export function manualSteps(
	connection: CloudflareConnection,
	domain: string,
	port: string,
): string[] {
	const host = domain || 'your domain';
	const route =
		connection === 'TUNNEL'
			? [
					'In Cloudflare, open Zero Trust › Networks › Tunnels and create a tunnel of type Cloudflared.',
					'On this server, run the install command Cloudflare shows. It installs cloudflared with the tunnel token.',
					`Add a public hostname: ${host}, service HTTP, URL localhost:${port}. Cloudflare creates the DNS record.`,
					'Set TRUSTED_PROXY_CIDRS=127.0.0.1/32,::1/128 on the server and restart it, so sign-in limits see real visitor addresses.',
				]
			: [
					`In DNS, add an A record for ${host} pointing at this server's public address, with the proxy on.`,
					'In SSL/TLS › Origin Server, create a certificate for the domain. Serve it on port 443 with a reverse proxy in front of this server.',
					'In SSL/TLS › Overview, set the encryption mode to Full (strict).',
					'Set TRUSTED_PROXY_CIDRS to the ranges listed at cloudflare.com/ips and restart the server.',
				];
	return [
		...route,
		`In Caching › Cache Rules, add a rule for ${host} where the path starts with /covers/, /api/v1/manga/, /_app/immutable/ or /admin/_app/immutable/, or equals /img. Set it to eligible for cache, with edge and browser TTL taken from the origin.`,
		'In Caching › Configuration, set Browser Cache TTL to Respect Existing Headers.',
		domain
			? `Open https://${domain}/health. It should say ok.`
			: 'Open /health on your domain. It should say ok.',
	];
}
