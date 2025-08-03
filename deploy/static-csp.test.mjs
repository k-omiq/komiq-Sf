import { createHash } from 'node:crypto';
import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { staticPolicy } from './static-csp.mjs';

test('hashes emitted bootstrap exactly, preserving runtime origin placeholders', () => {
	const script = '\nwindow.boot = "build-specific";\n';
	const template =
		"script-src 'self' ${CSP_SCRIPT_HASHES}; connect-src 'self' ${CSP_CONNECT_ORIGIN}";
	const policy = staticPolicy(
		`<script src="/theme-init.js"></script><script>${script}</script>`,
		template,
	);
	assert.ok(policy.includes(`'sha256-${createHash('sha256').update(script).digest('base64')}'`));
	assert.ok(policy.includes('${CSP_CONNECT_ORIGIN}'));
	assert.ok(!policy.includes('unsafe-inline'));
	const injected = createHash('sha256').update('window.injected=true').digest('base64');
	assert.ok(!policy.includes(injected));
});

test('external-only HTML needs no inline script allowance and malformed template fails closed', () => {
	assert.equal(
		staticPolicy('<script src="/app.js"></script>', "script-src 'self' ${CSP_SCRIPT_HASHES}"),
		"script-src 'self' ",
	);
	assert.throws(() => staticPolicy('<script>boot()</script>', "script-src 'self' 'unsafe-inline'"));
});
