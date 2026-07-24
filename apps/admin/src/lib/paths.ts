import { resolve } from '$app/paths';

/**
 * App-internal paths are written root-relative ('/users', '/settings#repos') and
 * prefixed here with the build's base path: '' for the hosted console at
 * admin.komiq.cc, '/admin' for the self-host build the server serves from
 * `KOMIKA_WEB_DIR` (`ADMIN_BASE_PATH=/admin`, see vite.config.ts).
 */
export function href(path: string): string {
	return (resolve as (p: string) => string)(path);
}

/** The base path itself, without a trailing slash ('' when unset). */
export const basePath = href('/').replace(/\/$/, '');

/** A browser pathname with the base stripped, for comparing against app paths. */
export function routePath(pathname: string): string {
	if (!basePath || !pathname.startsWith(basePath)) return pathname;
	return pathname.slice(basePath.length) || '/';
}
