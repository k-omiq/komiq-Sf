import { expect, test } from '@playwright/test';

/** Run against the hosted production preview, whose policy is emitted by Kit. */
test('reader restricts connections to configured origins and blocks unsigned scripts', async ({
	page,
}) => {
	const response = await page.goto('/');
	expect(response?.status()).toBe(200);
	const policy = response?.headers()['content-security-policy'] ?? '';
	const connect = policy
		.split(';')
		.find((directive) => directive.trim().startsWith('connect-src '));
	expect(connect).toBeDefined();
	for (const broadScheme of ['http:', 'https:', 'ws:', 'wss:']) {
		expect(connect?.trim().split(/\s+/)).not.toContain(broadScheme);
	}
	expect(
		policy.split(';').find((directive) => directive.trim().startsWith('script-src ')),
	).not.toContain("'unsafe-inline'");
	const violations = await page.evaluate(async () => {
		const events: string[] = [];
		const capture = (event: SecurityPolicyViolationEvent) => events.push(event.effectiveDirective);
		document.addEventListener('securitypolicyviolation', capture);
		const script = document.createElement('script');
		script.textContent = 'window.__unsignedCspProbe = true';
		document.body.append(script);
		await fetch('https://csp-probe.invalid/blocked').catch(() => undefined);
		await new Promise((resolve) => setTimeout(resolve, 200));
		script.remove();
		document.removeEventListener('securitypolicyviolation', capture);
		return { events, executed: '__unsignedCspProbe' in window };
	});
	expect(violations.events).toContain('connect-src');
	expect(violations.events).toContain('script-src-elem');
	expect(violations.executed).toBe(false);
	// The configured API is still usable for a real client-side navigation.
	await page.goto('/browse?sort=added');
	await expect(page.locator('.grid a.card').first()).toBeVisible({ timeout: 30_000 });
});
