import { defineConfig } from '@playwright/test';

/** Local fixtures intercept every API request; no live admin mutations. */
export default defineConfig({
	testDir: './tests',
	testMatch: 'admin-queue-state.spec.ts',
	timeout: 30_000,
	workers: 1,
	reporter: 'list',
	use: {
		baseURL: process.env.PW_ADMIN_BASE_URL || 'http://127.0.0.1:5274',
		channel: 'chrome',
		screenshot: 'only-on-failure',
	},
});
