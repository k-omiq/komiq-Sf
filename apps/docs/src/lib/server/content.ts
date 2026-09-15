/**
 * Turns the Markdown files in `src/content` into the pages of the site. Runs only
 * at build time (every route is prerendered), so the parser and the highlighter
 * never reach the browser.
 *
 * `apps/docs/CONTENT_GUIDE.md` is the contract for what a page may contain; this
 * file is what enforces it.
 */
import { Marked, Renderer, type Token, type Tokens } from 'marked';
import { bundledLanguages, createCssVariablesTheme, createHighlighter } from 'shiki';
import { handbooks, type Handbook } from '$lib/nav';

export interface TocItem {
	id: string;
	text: string;
	depth: 2 | 3;
}

/** One searchable slice of a page: the text under one heading. */
export interface Section {
	/** Heading id, or '' for the text before the first heading. */
	id: string;
	heading: string;
	text: string;
}

export interface Chapter {
	handbook: string;
	slug: string;
	href: string;
	/** The one name of the chapter: its heading, its menu entry, its search result. */
	title: string;
	description: string;
	html: string;
	toc: TocItem[];
	sections: Section[];
}

export interface HandbookInfo extends Omit<Handbook, 'chapters'> {
	href: string;
	chapters: Pick<Chapter, 'slug' | 'href' | 'title' | 'description'>[];
}

const sources = import.meta.glob<string>('/src/content/**/*.md', {
	query: '?raw',
	import: 'default',
	eager: true,
});

// ---- small text helpers -----------------------------------------------------

function escape(text: string): string {
	return text
		.replace(/&/g, '&amp;')
		.replace(/</g, '&lt;')
		.replace(/>/g, '&gt;')
		.replace(/"/g, '&quot;');
}

/** Markdown inline text with its markup characters removed. */
function plain(text: string): string {
	return text
		.replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1')
		.replace(/[`*_~]/g, '')
		.replace(/\s+/g, ' ')
		.trim();
}

function slugify(text: string): string {
	return (
		text
			.toLowerCase()
			.replace(/[^a-z0-9]+/g, '-')
			.replace(/^-+|-+$/g, '') || 'section'
	);
}

function frontmatter(raw: string, file: string): { meta: Record<string, string>; body: string } {
	const match = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?/.exec(raw);
	if (!match) throw new Error(`${file}: missing frontmatter (title and description)`);
	const meta: Record<string, string> = {};
	for (const line of match[1].split(/\r?\n/)) {
		const at = line.indexOf(':');
		if (at < 1) continue;
		meta[line.slice(0, at).trim()] = line
			.slice(at + 1)
			.trim()
			.replace(/^(['"])(.*)\1$/, '$2');
	}
	if (!meta.title || !meta.description) {
		throw new Error(`${file}: frontmatter needs both title and description`);
	}
	return { meta, body: raw.slice(match[0].length) };
}

// ---- code -------------------------------------------------------------------

const LANG_ALIASES: Record<string, string> = {
	sh: 'bash',
	shell: 'bash',
	zsh: 'bash',
	console: 'bash',
	ts: 'typescript',
	js: 'javascript',
	yml: 'yaml',
	rs: 'rust',
	kt: 'kotlin',
	md: 'markdown',
	env: 'ini',
	conf: 'ini',
};
const LANGS = [
	'bash',
	'typescript',
	'javascript',
	'svelte',
	'rust',
	'toml',
	'yaml',
	'json',
	'jsonc',
	'graphql',
	'sql',
	'kotlin',
	'swift',
	'java',
	'python',
	'html',
	'css',
	'xml',
	'dockerfile',
	'ini',
	'diff',
	'nginx',
	'markdown',
	'powershell',
].filter((lang) => lang in bundledLanguages);

// Token colours are CSS variables (set per theme in docs.css), so one rendered
// block follows the light/dark switch without being highlighted twice.
const highlighter = await createHighlighter({
	themes: [createCssVariablesTheme({ name: 'komiq', variablePrefix: '--code-', fontStyle: true })],
	langs: LANGS,
});

const COPY_ICON =
	'<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1"/></svg>';

export function codeHtml(code: string, lang: string): string {
	const name = LANG_ALIASES[lang] ?? lang;
	const body = LANGS.includes(name)
		? highlighter.codeToHtml(code, { lang: name, theme: 'komiq' })
		: `<pre class="shiki" tabindex="0"><code>${escape(code)}</code></pre>`;
	const label = lang && lang !== 'text' ? `<span class="code-lang">${escape(lang)}</span>` : '';
	return `<div class="code">${label}<button type="button" class="code-copy" data-copy aria-label="Copy code">${COPY_ICON}<span>Copy</span></button>${body}</div>`;
}

// ---- flow: a path that data takes, one hop per line ---------------------------

const ARROW =
	'<svg class="hop-arrow" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 12h14M13 6l6 6-6 6"/></svg>';

export interface Hop {
	from: string;
	to: string;
	what: string;
}

/** `From -> To: what happens`, one per line. Null when a line does not fit. */
function parseFlow(text: string): Hop[] | null {
	const hops: Hop[] = [];
	for (const line of text.split('\n')) {
		if (!line.trim()) continue;
		const match = /^(.+?)\s*->\s*(.+?)\s*:\s*(.+)$/.exec(line.trim());
		if (!match) return null;
		hops.push({ from: match[1], to: match[2], what: match[3] });
	}
	return hops.length ? hops : null;
}

export function flowHtml(hops: Hop[]): string {
	const rows = hops
		.map(
			(hop) =>
				`<li class="hop"><span class="hop-node">${escape(hop.from)}</span><span class="hop-wire">${ARROW}<span class="k-sr-only"> to </span></span><span class="hop-node">${escape(hop.to)}</span><span class="hop-what">${escape(hop.what)}</span></li>`,
		)
		.join('');
	return `<ol class="flow">${rows}</ol>`;
}

// ---- callouts -----------------------------------------------------------------

const CALLOUTS: Record<string, { tone: string; label: string; icon: string }> = {
	NOTE: {
		tone: 'note',
		label: 'Note',
		icon: '<circle cx="12" cy="12" r="9"/><path d="M12 16v-4M12 8h.01"/>',
	},
	TIP: {
		tone: 'info',
		label: 'Tip',
		icon: '<path d="M9 18h6M10 22h4"/><path d="M12 2a7 7 0 0 0-4 12.7c.6.5 1 1.2 1 2V17h6v-.3c0-.8.4-1.5 1-2A7 7 0 0 0 12 2z"/>',
	},
	WARNING: {
		tone: 'warning',
		label: 'Warning',
		icon: '<path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4M12 17h.01"/>',
	},
};

// ---- one page -------------------------------------------------------------------

interface Rendered {
	html: string;
	toc: TocItem[];
	sections: Section[];
	/** Internal links found in the page, for the dead-link check. */
	links: string[];
}

function render(body: string): Rendered {
	const toc: TocItem[] = [];
	const links: string[] = [];
	const used = new Map<string, number>();
	const headingIds = new WeakMap<Tokens.Heading, string>();

	const marked = new Marked({ gfm: true });
	marked.use({
		renderer: {
			heading(token) {
				const depth = Math.min(Math.max(token.depth, 2), 4);
				const inner = this.parser.parseInline(token.tokens);
				const id = headingIds.get(token);
				if (!id) return `<h${depth}>${inner}</h${depth}>\n`;
				return `<h${depth} id="${id}"><a class="anchor" href="#${id}">${inner}</a></h${depth}>\n`;
			},
			code(token) {
				const lang = (token.lang ?? '').trim().split(/\s+/)[0].toLowerCase();
				if (lang === 'flow') {
					const hops = parseFlow(token.text);
					if (hops) return flowHtml(hops);
				}
				return codeHtml(token.text, lang === 'flow' ? 'text' : lang);
			},
			blockquote(token) {
				const match = /^\[!(NOTE|TIP|WARNING)\][ \t]*\n?/.exec(token.text);
				if (!match) return Renderer.prototype.blockquote.call(this, token);
				const kind = CALLOUTS[match[1]];
				const inner = marked.parse(token.text.slice(match[0].length)) as string;
				return `<aside class="callout k-notice" data-tone="${kind.tone}"><svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${kind.icon}</svg><div class="k-notice__body"><strong class="callout-label">${kind.label}</strong>${inner}</div></aside>\n`;
			},
			table(token) {
				return `<div class="table-wrap" tabindex="0" role="region" aria-label="Table">${Renderer.prototype.table.call(this, token)}</div>\n`;
			},
			link(token) {
				const text = this.parser.parseInline(token.tokens);
				const href = token.href;
				if (href.startsWith('/')) {
					links.push(href);
					return `<a href="${escape(href)}">${text}</a>`;
				}
				if (href.startsWith('#')) return `<a href="${escape(href)}">${text}</a>`;
				if (/^https?:\/\//.test(href)) {
					return `<a href="${escape(href)}" rel="noopener noreferrer">${text}</a>`;
				}
				// Anything else (a repo-relative path, mailto) is not a place the site can
				// send the reader, so it stays as text.
				return text;
			},
			image() {
				return '';
			},
			// Raw HTML in a page is shown as text, never run. Comments are dropped.
			html(token) {
				return /^\s*<!--[\s\S]*-->\s*$/.test(token.text) ? '' : escape(token.text);
			},
		},
	});

	let tokens = marked.lexer(body);
	// The title comes from the frontmatter; a page that repeats it as an H1 loses it.
	const first = tokens.findIndex((t) => t.type !== 'space');
	if (tokens[first]?.type === 'heading' && (tokens[first] as Tokens.Heading).depth === 1) {
		tokens = Object.assign(tokens.slice(first + 1), { links: tokens.links });
	}

	const sections: Section[] = [{ id: '', heading: '', text: '' }];
	for (const token of tokens as Token[]) {
		if (token.type === 'heading') {
			const heading = token as Tokens.Heading;
			const text = plain(heading.text);
			if (heading.depth <= 3) {
				const base = slugify(text);
				const seen = used.get(base) ?? 0;
				used.set(base, seen + 1);
				const id = seen ? `${base}-${seen + 1}` : base;
				headingIds.set(heading, id);
				toc.push({ id, text, depth: heading.depth === 3 ? 3 : 2 });
				sections.push({ id, heading: text, text: '' });
				continue;
			}
		}
		if (token.type === 'space') continue;
		const current = sections[sections.length - 1];
		const text =
			token.type === 'code' ? (token as Tokens.Code).text.slice(0, 600) : plain(token.raw);
		current.text += (current.text ? ' ' : '') + text.replace(/\s+/g, ' ').trim();
	}

	return {
		html: marked.parser(tokens),
		toc,
		sections: sections.filter((s) => s.text || s.heading),
		links,
	};
}

// ---- the whole site ---------------------------------------------------------------

function build(): { chapters: Map<string, Chapter>; books: HandbookInfo[] } {
	const chapters = new Map<string, Chapter>();
	const pageLinks = new Map<string, string[]>();
	const known = new Set(Object.keys(sources));

	const books = handbooks.map((book): HandbookInfo => {
		const list: HandbookInfo['chapters'] = [];
		for (const slug of book.chapters) {
			const file = `/src/content/${book.id}/${slug}.md`;
			const raw = sources[file];
			known.delete(file);
			// Listed in the menu but not written yet: left out, never shown as a dead link.
			if (raw === undefined) continue;
			const { meta, body } = frontmatter(raw, file);
			const href = `/${book.id}/${slug}`;
			const page = render(body);
			pageLinks.set(href, page.links);
			chapters.set(href, {
				handbook: book.id,
				slug,
				href,
				title: meta.title,
				description: meta.description,
				html: page.html,
				toc: page.toc,
				sections: page.sections,
			});
			list.push({ slug, href, title: meta.title, description: meta.description });
		}
		return {
			id: book.id,
			title: book.title,
			description: book.description,
			icon: book.icon,
			href: `/${book.id}`,
			chapters: list,
		};
	});

	const problems: string[] = [];
	for (const file of known) problems.push(`${file} is not listed in src/lib/nav.ts`);
	const targets = new Set(['/', ...books.map((b) => b.href), ...chapters.keys()]);
	for (const [href, links] of pageLinks) {
		for (const link of links) {
			const path = link.split('#')[0].replace(/\/$/, '') || '/';
			if (!targets.has(path)) problems.push(`${href} links to ${link}, which does not exist`);
		}
	}
	if (problems.length) {
		const message = `Docs content problems:\n  ${problems.join('\n  ')}`;
		// While pages are being written the dev server keeps working and says what is
		// wrong; a build with a dead link or an unlisted page does not ship.
		if (import.meta.env.DEV) console.warn(message);
		else throw new Error(message);
	}

	return { chapters, books: books.filter((b) => b.chapters.length > 0) };
}

const site = build();

export function getHandbooks(): HandbookInfo[] {
	return site.books;
}

export function getHandbook(id: string): HandbookInfo | undefined {
	return site.books.find((b) => b.id === id);
}

export function getChapter(handbook: string, slug: string): Chapter | undefined {
	return site.chapters.get(`/${handbook}/${slug}`);
}

export function allChapters(): Chapter[] {
	return [...site.chapters.values()];
}
