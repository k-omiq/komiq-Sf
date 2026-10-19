import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { staticPolicy } from '../../../deploy/static-csp.mjs';

/** @param {string} directory @param {string[]} connectSources */
export function writeStaticHeaders(directory, connectSources) {
	const html = readFileSync(join(directory, 'index.html'), 'utf8');
	const security = readFileSync(new URL('../_headers', import.meta.url), 'utf8').trimEnd();
	const policy = staticPolicy(
		html,
		[
			"default-src 'self'",
			// challenges.cloudflare.com: the Turnstile widget's script and frame, for a
			// server that asks for the check (src/lib/components/TurnstileWidget.svelte).
			"script-src 'self' https://challenges.cloudflare.com ${CSP_SCRIPT_HASHES}",
			'frame-src https://challenges.cloudflare.com',
			"style-src 'self' 'unsafe-inline'",
			"img-src 'self' https: http: data: blob:",
			`connect-src ${connectSources.map((source) => (source === 'self' ? "'self'" : source)).join(' ')}`,
			"font-src 'self' data:",
			"object-src 'none'",
			"base-uri 'self'",
			"form-action 'self'",
		].join('; '),
	);
	// The wildcard covers SPA fallback URLs too. Workers Assets applies _headers
	// to the requested URL; /index.html alone would miss /browse and /read/*.
	// Dynamic series responses get their own CSP from SvelteKit, not this file.
	// Chunks are content-hashed, so they are kept 7 days without revalidation. SPA
	// fallback also serves index.html at a missing chunk URL (including non-navigation
	// requests), and _headers cannot tell a real asset from that fallback, so a missing
	// chunk would be cached as HTML for up to 7 days. A release therefore never drops a
	// chunk: deploy/release.mjs copies every earlier release's chunks into the new
	// output. `! Cache-Control` drops the wildcard's `no-cache` first, so the value is
	// replaced, not appended.
	writeFileSync(
		join(directory, '_headers'),
		`${security}\n  Content-Security-Policy: ${policy}\n  Cache-Control: no-cache\n\n/_app/*\n  X-Robots-Tag: noindex\n\n/_app/immutable/*\n  ! Cache-Control\n  Cache-Control: public, max-age=604800, immutable\n`,
	);
}
