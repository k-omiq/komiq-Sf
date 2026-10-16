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
			"script-src 'self' ${CSP_SCRIPT_HASHES}",
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
	// Chunks get one day, not `immutable`: SPA fallback also serves index.html at a
	// missing chunk URL (including non-navigation requests), and _headers cannot tell
	// a real asset from that fallback. A day bounds a cached fallback; `no-cache` here
	// would revalidate every chunk on every page load. `! Cache-Control` drops the
	// wildcard's `no-cache` first, so the value is replaced, not appended.
	writeFileSync(
		join(directory, '_headers'),
		`${security}\n  Content-Security-Policy: ${policy}\n  Cache-Control: no-cache\n\n/_app/*\n  X-Robots-Tag: noindex\n\n/_app/immutable/*\n  ! Cache-Control\n  Cache-Control: public, max-age=86400\n`,
	);
}
