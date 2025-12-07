/** Run against reader-scale-server.mjs + a reader dev server configured for it. */
import { chromium } from 'playwright';
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const base = process.env.PW_READER_SCALE_URL || 'http://127.0.0.1:5175';
for (const width of [1440, 390]) {
	const results = { baseline: [], optimized: [] };
	for (let run = 0; run < 3; run++) {
		for (const mode of ['baseline', 'optimized']) {
			const page = await browser.newPage({ viewport: { width, height: 900 } });
			await page.addInitScript(() => localStorage.setItem('komika-token', 'visual-fixture'));
			const cdp = await page.context().newCDPSession(page);
			await cdp.send('Emulation.setCPUThrottlingRate', { rate: 4 });
			await page.goto(`${base}/library`, { waitUntil: 'networkidle' });
			await page.locator('.lib-card').last().waitFor({ state: 'attached' });
			if (mode === 'baseline')
				await page.addStyleTag({
					content: '.grid.large .cover { content-visibility: visible !important; }',
				});
			const result = await page.evaluate(async () => {
				const frames = [];
				let previous = performance.now();
				for (let frame = 0; frame < 80; frame++) {
					window.scrollTo(0, frame * 180);
					await new Promise(requestAnimationFrame);
					const now = performance.now();
					frames.push(now - previous);
					previous = now;
				}
				return {
					mean: frames.reduce((a, b) => a + b) / frames.length,
					p95: [...frames].sort((a, b) => a - b)[Math.floor(frames.length * 0.95)],
				};
			});
			results[mode].push(result);
			await page.close();
		}
	}
	for (const mode of ['baseline', 'optimized']) {
		const samples = results[mode];
		const median = (key) => samples.map((s) => s[key]).sort((a, b) => a - b)[1];
		console.log(
			JSON.stringify({
				width,
				mode,
				cards: 600,
				cpuThrottle: 4,
				samples,
				medianMean: median('mean'),
				medianP95: median('p95'),
			}),
		);
	}
}
await browser.close();
