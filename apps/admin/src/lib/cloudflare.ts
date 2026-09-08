/**
 * Text for the Cloudflare screens (setup wizard, Settings › Access): the exact token
 * permissions, and the manual steps for owners who prefer the dashboard. Kept in one
 * place so both screens and `docs/self-host-cloudflare.md` say the same thing.
 */
import type { CloudflareConnection } from '@komika/api';

/** One row of the "Create Custom Token" form: scope, group, level. */
export interface Permission {
	scope: 'Account' | 'Zone';
	group: string;
	level: 'Read' | 'Edit';
}

export function tokenPermissions(connection: CloudflareConnection): Permission[] {
	return [
		connection === 'TUNNEL'
			? { scope: 'Account', group: 'Cloudflare Tunnel', level: 'Edit' }
			: { scope: 'Zone', group: 'SSL and Certificates', level: 'Edit' },
		{ scope: 'Zone', group: 'Zone', level: 'Read' },
		{ scope: 'Zone', group: 'DNS', level: 'Edit' },
		{ scope: 'Zone', group: 'Zone Settings', level: 'Edit' },
		{ scope: 'Zone', group: 'Cache Rules', level: 'Edit' },
		// Cloudflare's Cache Rules API guide lists these two as well.
		{ scope: 'Account', group: 'Account Rulesets', level: 'Edit' },
		{ scope: 'Account', group: 'Account Filter Lists', level: 'Edit' },
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
