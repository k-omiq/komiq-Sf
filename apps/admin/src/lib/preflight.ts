/**
 * Lines the access screens show before setup runs, from what the server knows about its
 * own machine (`accessPreflight`): the tunnel connector, the HTTPS ports, a container.
 * Shown by `Preflight.svelte` under the choice they belong to. A server that predates
 * the question gives no preflight, and the screens show no lines.
 */
import type { AccessPreflight, CloudflareConnection, PortState } from '@komika/api';

export interface PreflightLine {
	/** `ok`: nothing to do. `info`: komiq handles it. `warning`: the owner must act. */
	tone: 'ok' | 'info' | 'warning';
	text: string;
	/** A command to run in a terminal, shown after the text in the code font. */
	command?: string;
}

/** The self-host installer, run again with other choices. It keeps the data. */
const INSTALLER = 'bash deploy/selfhost/install.sh';

/**
 * The tunnel connector (cloudflared): there, fetched by setup, or not available.
 * `tunnelOnly`: the tunnel is the one connection on offer, so Public IP is no way out.
 */
export function connectorLine(p: AccessPreflight, tunnelOnly = false): PreflightLine {
	if (p.cloudflared === 'found') {
		return { tone: 'ok', text: 'The connector (cloudflared) is on this server.' };
	}
	if (p.cloudflared === 'download') {
		return { tone: 'info', text: 'komiq downloads the connector (cloudflared) during setup.' };
	}
	// The server's sentence says why, and how to install the connector by hand.
	const why = (p.cloudflaredDetail ?? '').trim();
	const [or, orElse] = tunnelOnly ? ['', '.'] : [' Or choose Public IP.', ', or choose Public IP.'];
	return {
		tone: 'warning',
		text: why
			? `${/[.!?]$/.test(why) ? why : `${why}.`}${or}`
			: `komiq cannot get the connector (cloudflared) for this server. Install cloudflared on the server${orElse}`,
	};
}

/**
 * What a port is doing. `inUse` and `denied` end their lines with the way out that fits
 * the screen.
 */
export function portLine(
	port: number,
	state: PortState,
	inUse = 'Stop it first.',
	denied = 'Start the server with permission for ports below 1024.',
): PreflightLine {
	switch (state) {
		case 'free':
			return { tone: 'ok', text: `Port ${port} is free.` };
		case 'serving':
			return { tone: 'ok', text: `komiq already answers on port ${port}.` };
		case 'in_use':
			return {
				tone: 'warning',
				text: `Port ${port} is used by another program on this server. ${inUse}`,
			};
		case 'denied':
			return {
				tone: 'warning',
				text: `This server does not have permission to open port ${port}. ${denied}`,
			};
		default:
			return { tone: 'info', text: `komiq could not check port ${port}.` };
	}
}

/**
 * A container only gets the ports its host publishes. The installer publishes them for
 * `access`, its name for the choice. `--no-setup`: the choice itself is made here.
 */
export function containerLine(ports: number[], access: 'public-ip' | 'direct'): PreflightLine {
	const [first, second] = [...new Set(ports)].sort((a, b) => a - b);
	const which = second === undefined ? `Port ${first}` : `Ports ${first} and ${second}`;
	return {
		tone: 'info',
		text: `This server runs in a container. ${which} must be published too. Run this on the server, then make the choice here:`,
		command: `${INSTALLER} --access ${access} --no-setup`,
	};
}

/** Under the connection choice of the Cloudflare screens. */
export function cloudflareLines(
	p: AccessPreflight | null,
	connection: CloudflareConnection,
	tunnelOnly = false,
): PreflightLine[] {
	if (!p) return [];
	if (connection === 'TUNNEL') return [connectorLine(p, tunnelOnly)];
	const lines = [
		portLine(
			p.httpsPort,
			p.httpsPortState,
			'Stop it, or use a tunnel.',
			'Start the server with permission for ports below 1024, or use a tunnel.',
		),
	];
	if (p.inContainer) lines.push(containerLine([p.httpsPort], 'public-ip'));
	return lines;
}

/**
 * "Public without a domain" with HTTPS: readers arrive on the HTTPS port, and Let's
 * Encrypt checks the address on the other one before it gives a certificate.
 */
export function directLines(p: AccessPreflight | null): PreflightLine[] {
	if (!p) return [];
	const lines =
		p.httpsPortState === 'free' && p.acmePortState === 'free' && p.httpsPort !== p.acmePort
			? [
					{
						tone: 'ok' as const,
						text: `Ports ${Math.min(p.acmePort, p.httpsPort)} and ${Math.max(p.acmePort, p.httpsPort)} are free.`,
					},
				]
			: [portLine(p.httpsPort, p.httpsPortState), portLine(p.acmePort, p.acmePortState)];
	if (p.inContainer) lines.push(containerLine([p.httpsPort, p.acmePort], 'direct'));
	return lines;
}

/**
 * "Public without a domain" over plain HTTP: readers arrive on the server's HTTP port.
 * Docker publishes that port for this machine only, unless its settings say otherwise.
 */
export function plainLines(p: AccessPreflight | null): PreflightLine[] {
	if (!p?.inContainer) return [];
	return [
		{
			tone: 'info',
			text: 'This server runs in a container. With Docker, the HTTP port must also answer other machines: KOMIQ_BIND=0.0.0.0 in deploy/selfhost/.env. The installer sets it. Run this on the server, then save the address here:',
			command: `${INSTALLER} --access direct --encrypt no --no-setup`,
		},
	];
}

/**
 * Whether `address` (an origin, or `host[:port]`) is on a private network: this computer,
 * a home or office network, a carrier's shared range. Readers elsewhere cannot use it.
 * A name that is not an IP address counts as public, except `localhost` and `.local`.
 */
export function isPrivateAddress(address: string): boolean {
	let host = address.trim();
	try {
		host = new URL(host.includes('://') ? host : `http://${host}`).hostname;
	} catch {
		// A bare IPv6 address is not a URL host: test it as typed.
	}
	host = host.replace(/^\[|\]$/g, '').toLowerCase();
	if (host === 'localhost' || host.endsWith('.localhost') || host.endsWith('.local')) return true;
	const v4 = host.match(/^(\d{1,3})\.(\d{1,3})\.\d{1,3}\.\d{1,3}$/);
	if (v4) {
		const [a, b] = [Number(v4[1]), Number(v4[2])];
		return (
			a === 10 ||
			a === 127 ||
			a === 0 ||
			(a === 172 && b >= 16 && b <= 31) ||
			(a === 192 && b === 168) ||
			(a === 169 && b === 254) ||
			(a === 100 && b >= 64 && b <= 127)
		);
	}
	// IPv6: loopback, unique local (fc00::/7), link-local (fe80::/10).
	return host === '::1' || /^f[cd][0-9a-f]{2}:/.test(host) || /^fe[89ab][0-9a-f]:/.test(host);
}

/**
 * The detected public address as the "Server address" field wants it: alone when the
 * server gets a certificate for it, with the plain HTTP port otherwise.
 */
export function suggestedAddress(p: AccessPreflight, encrypt: boolean): string {
	const ip = p.publicIp ?? '';
	if (!ip || encrypt) return ip;
	return withPort(ip, p.httpPort);
}

/** `203.0.113.5:8080`, `[2001:db8::1]:8080`. */
function withPort(host: string, port: number): string {
	return `${host.includes(':') ? `[${host}]` : host}:${port}`;
}

/**
 * `address` (`host[:port]`, or an origin) as the "Server address" field wants it for the
 * encryption choice: the host alone when the server gets a certificate for it, the host
 * and a port for plain HTTP. Plain HTTP keeps a port the address names, and one without
 * gets `httpPort`, the server's own. The host is never replaced, only its port.
 * Text this cannot read, or a port it does not know, leaves the address as it is.
 */
export function addressFor(address: string, encrypt: boolean, httpPort?: number): string {
	const typed = address.trim();
	// A bare IPv6 address is not a URL host until it is in brackets.
	const bareV6 = /^[0-9a-f:.]+$/i.test(typed) && typed.split(':').length > 2;
	let url: URL;
	try {
		url = new URL(bareV6 ? `http://[${typed}]` : typed.includes('://') ? typed : `http://${typed}`);
	} catch {
		return address;
	}
	const host = url.hostname.replace(/^\[|\]$/g, '');
	const plain = /^https?:$/.test(url.protocol) && url.pathname === '/' && !url.search && !url.hash;
	if (!host || !plain || url.username) return address;
	if (encrypt) return host;
	// `url.port` is empty for a port the scheme implies: read it from the text.
	const namesPort = !bareV6 && /:\d+\/?$/.test(typed);
	return namesPort || httpPort === undefined ? address : withPort(host, httpPort);
}
