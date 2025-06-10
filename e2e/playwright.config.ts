import { defineConfig, devices } from '@playwright/test';

/**
 * Playwright config for the Komika reader smoke tests.
 *
 * The reader is assumed to be served separately (dev server, static preview, or
 * the nginx image). Point the tests at it with PW_BASE_URL; defaults to the
 * reader dev server on :5173.
 *
 * To let Playwright launch the reader itself, set PW_WEB_SERVER=1 (see the
 * commented `webServer` block below). It is opt-in so it doesn't fight an
 * already-running dev server / other agents' processes.
 */
const baseURL = process.env.PW_BASE_URL ?? 'http://localhost:5173';

export default defineConfig({
	testDir: './tests',
	fullyParallel: true,
	forbidOnly: !!process.env.CI,
	retries: process.env.CI ? 2 : 0,
	workers: process.env.CI ? 1 : undefined,
	reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : 'list',

	use: {
		baseURL,
		trace: 'on-first-retry',
		screenshot: 'only-on-failure',
	},

	projects: [
		{
			name: 'chromium',
			use: { ...devices['Desktop Chrome'] },
		},
	],

	// Opt-in: launch the reader dev server for the test run. Guarded by an env
	// flag so it never collides with a dev server you already have running.
	...(process.env.PW_WEB_SERVER
		? {
				webServer: {
					command: 'pnpm --filter @komika/reader dev',
					// Run from the repo root so the pnpm filter resolves.
					cwd: '..',
					url: baseURL,
					reuseExistingServer: !process.env.CI,
					timeout: 120_000,
				},
			}
		: {}),
});
