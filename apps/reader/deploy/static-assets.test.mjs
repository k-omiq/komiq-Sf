import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Run after the web and static builds. Inspect final adapter output rather than
// a fixture: SvelteKit's bootstrap and its whitespace change between builds.
for (const directory of ['../.svelte-kit/cloudflare/', '../build/']) {
	test(`${directory} CSP hashes cover exactly the emitted inline scripts`, () => {
		const html = readFileSync(new URL(`${directory}index.html`, import.meta.url), 'utf8');
		const headers = readFileSync(new URL(`${directory}_headers`, import.meta.url), 'utf8');
		const expected = new Set();
		for (const [, attributes, script] of html.matchAll(
			/<script\b([^>]*)>([\s\S]*?)<\/script\s*>/gi,
		)) {
			if (!/(?:^|\s)src\s*=/i.test(attributes) && script.trim()) {
				expected.add(`'sha256-${createHash('sha256').update(script).digest('base64')}'`);
			}
		}
		assert.ok(expected.size >= 2, 'recovery handler and bootstrap must both be hashed');
		const policy = headers.match(/Content-Security-Policy: (.+)/)[1];
		const scriptDirective = policy.match(/script-src ([^;]+)/)[1];
		assert.deepEqual(new Set(scriptDirective.match(/'sha256-[^']+'/g)), expected);
		assert.ok(!scriptDirective.includes('unsafe-inline'));
		const meta = html.match(/<meta http-equiv="content-security-policy" content="([^"]+)"/);
		if (meta)
			for (const hash of expected)
				assert.ok(meta[1].includes(hash), 'meta CSP must also permit each script');
		assert.match(headers, /^\/\*\n/);
		assert.match(headers, /X-Content-Type-Options: nosniff/);
		assert.match(headers, /X-Frame-Options: DENY/);
		assert.match(headers, /Referrer-Policy: strict-origin-when-cross-origin/);
		assert.match(
			headers,
			/Permissions-Policy: camera=\(\), microphone=\(\), geolocation=\(\), payment=\(\), usb=\(\)/,
		);
		assert.match(headers, /  Cache-Control: no-cache/);
		// The 7-day immutable lifetime belongs to the hashed chunk folder alone.
		assert.match(
			headers,
			/\n\/_app\/immutable\/\*\n  ! Cache-Control\n  Cache-Control: public, max-age=604800, immutable\n/,
		);
		const longLived = headers
			.trim()
			.split('\n\n')
			.filter((rule) => /Cache-Control: .*(?:immutable|max-age=[1-9])/.test(rule));
		assert.deepEqual(
			longLived.map((rule) => rule.split('\n')[0]),
			['/_app/immutable/*'],
		);
	});
}
