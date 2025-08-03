import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

// Hash the exact emitted script text: adapter-static bootstraps change per
// build, but the final HTML and nginx policy are shipped together.
export function staticPolicy(html, template) {
	const sources = new Set();
	for (const match of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script\s*>/gi)) {
		if (/(?:^|\s)src\s*=/i.test(match[1]) || !match[2].trim()) continue;
		const hash = createHash('sha256').update(match[2]).digest('base64');
		sources.add(`'sha256-${hash}'`);
	}
	if (!template.includes('${CSP_SCRIPT_HASHES}')) {
		throw new Error('nginx template must contain CSP_SCRIPT_HASHES');
	}
	return template.replaceAll('${CSP_SCRIPT_HASHES}', [...sources].join(' '));
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
	const [, , htmlPath, templatePath, outputPath] = process.argv;
	if (!htmlPath || !templatePath || !outputPath) {
		throw new Error('Usage: static-csp.mjs HTML TEMPLATE OUTPUT');
	}
	writeFileSync(
		outputPath,
		staticPolicy(readFileSync(htmlPath, 'utf8'), readFileSync(templatePath, 'utf8')),
	);
}
