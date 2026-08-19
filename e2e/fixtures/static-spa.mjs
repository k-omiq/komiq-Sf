// Serve a static SPA build under a base path, the way komika-server serves the
// embedded admin from KOMIKA_WEB_DIR: real files as-is, any other path under the
// base answers its index.html. No dependencies.
//
//   node fixtures/static-spa.mjs <build dir> <port> [base=/admin]
import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { extname, join, normalize, resolve } from 'node:path';

const [dir, port, base = '/admin'] = process.argv.slice(2);
if (!dir || !port) {
	console.error('usage: node static-spa.mjs <build dir> <port> [base]');
	process.exit(2);
}
const root = resolve(dir);
const TYPES = {
	'.html': 'text/html; charset=utf-8',
	'.js': 'text/javascript',
	'.css': 'text/css',
	'.json': 'application/json',
	'.webmanifest': 'application/manifest+json',
	'.png': 'image/png',
	'.ico': 'image/x-icon',
	'.svg': 'image/svg+xml',
	'.woff2': 'font/woff2',
};

async function file(path) {
	try {
		return (await stat(path)).isFile() ? await readFile(path) : null;
	} catch {
		return null;
	}
}

createServer(async (req, res) => {
	const { pathname } = new URL(req.url, 'http://x');
	if (pathname !== base && !pathname.startsWith(`${base}/`)) {
		res.writeHead(404).end();
		return;
	}
	const rel = normalize(decodeURIComponent(pathname.slice(base.length)) || '/');
	const target = join(root, rel);
	const body = target.startsWith(root) ? await file(target) : null;
	if (body) {
		res.writeHead(200, { 'content-type': TYPES[extname(target)] ?? 'application/octet-stream' });
		res.end(body);
		return;
	}
	res.writeHead(200, { 'content-type': TYPES['.html'] });
	res.end(await readFile(join(root, 'index.html')));
}).listen(Number(port), '127.0.0.1');
