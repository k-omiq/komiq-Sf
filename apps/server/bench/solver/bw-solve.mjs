// Solve one URL with BetterWright's local CAPTCHA solver and print the clearance it
// earned; replay-all.py then checks it from plain HTTP clients.
// Usage: node bw-solve.mjs URL   -> one JSON line on stdout.
//
// BetterWright hides cookies from snippets, so it attaches over CDP to a Chromium
// this script starts, and the script (trusted host code) reads the cookies back.
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { BetterWright } from 'betterwright';
import { chromium as pw } from 'playwright-core';

const url = process.argv[2];
const PORT = 9333;
// Same shape FlareSolverr uses on arm64 (an x86_64 desktop Chrome string), with the
// bundled Chromium's major version.
const UA = `Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/${
	process.env.CHROME_MAJOR || '149'
}.0.0.0 Safari/537.36`;
const started = Date.now();
const dir = mkdtempSync(join(tmpdir(), 'bw-'));
const chrome = spawn(
	pw.executablePath(),
	[
		'--headless=new',
		'--no-sandbox',
		'--no-first-run',
		'--disable-dev-shm-usage',
		// Stock headless Chromium says "HeadlessChrome" and sets navigator.webdriver;
		// BetterChromium hides both. Hide them here too so the test is about the solver.
		`--user-agent=${UA}`,
		'--disable-blink-features=AutomationControlled',
		...(process.env.PROXY ? [`--proxy-server=${process.env.PROXY}`] : []),
		`--remote-debugging-port=${PORT}`,
		`--user-data-dir=${join(dir, 'profile')}`,
		'about:blank',
	],
	{ stdio: 'ignore' },
);

const out = { solver: 'betterwright', url };
let bw;
let cdp;
try {
	const cdpUrl = await waitForCdp(`http://127.0.0.1:${PORT}`);
	bw = new BetterWright({
		home: join(dir, 'home'),
		provider: { cdpUrl },
		adBlock: false,
		downloadPolicy: 'deny',
		defaultTimeout: 90,
	});
	const snippet = `
    const target = ${JSON.stringify(url)};
    await page.goto(target, { waitUntil: "domcontentloaded", timeout: 30000 }).catch(() => {});
    await page.waitForTimeout(6000);
    const before = await captcha.detect();
    let solve = null;
    if (before.present) {
      solve = await captcha.solve({ timeoutMs: 45000, maxStages: 3 });
      await page.waitForTimeout(4000);
    }
    const after = await captcha.detect();
    return {
      before: { present: before.present, challenge: before.challenge ?? null },
      solve: solve && { status: solve.status, provider: solve.provider, stage: solve.stage,
                        cleared: solve.cleared, attempts: (solve.attempts || []).length },
      afterPresent: after.present,
      title: (await page.title()).slice(0, 80),
      finalUrl: page.url(),
      ua: await page.evaluate(() => navigator.userAgent),
    };`;
	const r = await bw.run(snippet, { timeout: 90 });
	out.ok = r.ok;
	if (!r.ok) out.error = String(r.error).slice(0, 300);
	Object.assign(out, typeof r.result === 'object' ? r.result : { result: r.result });

	// Read cookies straight from the browser while BetterWright still holds the page.
	cdp = await pw.connectOverCDP(cdpUrl);
	const host = new URL(url).hostname.replace(/^www\./, '');
	const cookies = [];
	for (const ctx of cdp.contexts()) cookies.push(...(await ctx.cookies()));
	const mine = cookies.filter((c) => c.domain.replace(/^\./, '').endsWith(host));
	out.cfClearance = mine.some((c) => c.name === 'cf_clearance');
	out.cookieNames = [...new Set(mine.map((c) => c.name))];
	// Replayed and then dropped by replay-all.py on the host.
	out.cookieHeader = mine.map((c) => `${c.name}=${c.value}`).join('; ');
} catch (e) {
	out.ok = false;
	out.error = String(e?.message || e).slice(0, 300);
} finally {
	await cdp?.close().catch(() => {});
	await bw?.close().catch(() => {});
	chrome.kill('SIGKILL');
	rmSync(dir, { recursive: true, force: true });
	out.ms = Date.now() - started;
	process.stdout.write(`${JSON.stringify(out)}\n`);
	process.exit(0);
}

async function waitForCdp(base) {
	for (let i = 0; i < 50; i++) {
		try {
			const v = await (await fetch(`${base}/json/version`)).json();
			if (v.webSocketDebuggerUrl) return v.webSocketDebuggerUrl;
		} catch {}
		await new Promise((r) => setTimeout(r, 200));
	}
	throw new Error('Chromium did not open its debug port');
}
