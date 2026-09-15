import { allChapters, getHandbooks } from '$lib/server/content';
import { SITE_URL } from '$lib/site';

export const prerender = true;

export function GET() {
	const paths = ['/', ...getHandbooks().map((b) => b.href), ...allChapters().map((c) => c.href)];
	const urls = paths.map((path) => `<url><loc>${SITE_URL}${path}</loc></url>`).join('');
	return new Response(
		`<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">${urls}</urlset>`,
		{ headers: { 'Content-Type': 'application/xml' } },
	);
}
