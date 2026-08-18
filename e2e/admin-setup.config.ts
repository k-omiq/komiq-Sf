import { defineConfig } from '@playwright/test';

/**
 * The first-run setup wizard against a mocked GraphQL fixture (no real server).
 * Needs the EMBEDDED admin build, which checks setup and lives under /admin:
 *
 *   ADMIN_TARGET=embedded ADMIN_BASE_PATH=/admin PUBLIC_KOMIKA_API= pnpm -C apps/admin build
 *   node e2e/fixtures/static-spa.mjs apps/admin/build 5275 /admin
 *   cd e2e && npx playwright test -c admin-setup.config.ts
 *
 * (`vite preview` does not serve an adapter-static build under its base path.)
 * PW_CHROMIUM_PATH points Playwright at an already installed Chromium.
 */
export default defineConfig({
	testDir: './tests',
	testMatch: 'admin-setup.spec.ts',
	timeout: 30_000,
	workers: 1,
	reporter: 'list',
	use: {
		baseURL: process.env.PW_ADMIN_SETUP_BASE_URL || 'http://127.0.0.1:5275',
		screenshot: 'only-on-failure',
		launchOptions: process.env.PW_CHROMIUM_PATH
			? { executablePath: process.env.PW_CHROMIUM_PATH }
			: undefined,
	},
});
