import { defineConfig } from '@playwright/test';

export default defineConfig({
	testDir: './tests',
	testMatch: 'reader-scale.spec.ts',
	workers: 1,
	reporter: 'list',
	use: { channel: 'chrome', screenshot: 'only-on-failure' },
});
