import { defineConfig } from '@playwright/test';

/**
 * The desktop app's console as a remote admin, against a mocked server (no real server).
 * There is no Tauri shell either: in a plain browser the bridge is absent
 * (`desktop.available` is false), which is all a remote server needs.
 *
 * Needs the DESKTOP admin build, which reads its server from `komiq-server`. A build
 * always lands in apps/admin/build, and the setup suite expects the embedded build
 * there (admin-setup.config.ts). So copy the desktop build away and put that one back:
 *
 *   ADMIN_TARGET=desktop PUBLIC_KOMIKA_API= pnpm -C apps/admin build
 *   rm -rf /tmp/komiq-admin-desktop && cp -R apps/admin/build /tmp/komiq-admin-desktop
 *   ADMIN_TARGET=embedded ADMIN_BASE_PATH=/admin PUBLIC_KOMIKA_API= pnpm -C apps/admin build
 *   node e2e/fixtures/static-spa.mjs /tmp/komiq-admin-desktop 5276 ""
 *   cd e2e && npx playwright test -c admin-desktop.config.ts
 *
 * PW_CHROMIUM_PATH points Playwright at an already installed Chromium.
 */
export default defineConfig({
	testDir: './tests',
	testMatch: 'admin-desktop.spec.ts',
	timeout: 30_000,
	workers: 1,
	reporter: 'list',
	use: {
		baseURL: process.env.PW_ADMIN_DESKTOP_BASE_URL || 'http://127.0.0.1:5276',
		screenshot: 'only-on-failure',
		launchOptions: process.env.PW_CHROMIUM_PATH
			? { executablePath: process.env.PW_CHROMIUM_PATH }
			: undefined,
	},
});
