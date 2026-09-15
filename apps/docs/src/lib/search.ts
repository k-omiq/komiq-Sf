/**
 * Search over the prerendered index (`/search.json`). The site is a few dozen
 * pages, so a scan per keystroke is instant and needs no index structure.
 */
export interface SearchPage {
	href: string;
	title: string;
	/** Handbook title. */
	book: string;
	description: string;
	sections: { id: string; heading: string; text: string }[];
}

export interface SearchHit {
	key: string;
	href: string;
	kind: 'page' | 'section';
	title: string;
	/** Where the hit lives: "Handbook" for a page, "Handbook › Page" for a section. */
	where: string;
	/** Text around the match, for hits found in body text. */
	snippet?: string;
	score: number;
}

function snippet(text: string, term: string): string {
	const at = text.toLowerCase().indexOf(term);
	if (at < 0) return text.slice(0, 120);
	const start = Math.max(0, at - 48);
	const end = Math.min(text.length, at + term.length + 80);
	return (start > 0 ? '…' : '') + text.slice(start, end).trim() + (end < text.length ? '…' : '');
}

export function search(pages: SearchPage[], query: string, limit = 14): SearchHit[] {
	const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
	if (!terms.length) return [];
	const phrase = terms.join(' ');
	const hits: SearchHit[] = [];
	const every = (haystack: string) => terms.every((t) => haystack.includes(t));

	for (const page of pages) {
		const title = page.title.toLowerCase();
		const head = `${title} ${page.description.toLowerCase()}`;
		if (every(head)) {
			hits.push({
				key: page.href,
				href: page.href,
				kind: 'page',
				title: page.title,
				where: page.book,
				snippet: page.description,
				score: title.startsWith(phrase) ? 100 : every(title) ? 80 : 50,
			});
		}
		for (const section of page.sections) {
			const heading = section.heading.toLowerCase();
			const body = section.text.toLowerCase();
			const inHeading = section.heading !== '' && every(heading);
			if (!inHeading && !every(`${heading} ${body}`)) continue;
			hits.push({
				key: `${page.href}#${section.id}`,
				href: section.id ? `${page.href}#${section.id}` : page.href,
				kind: 'section',
				title: section.heading || page.title,
				where: `${page.book} › ${page.title}`,
				snippet: snippet(section.text, terms[0]),
				score: inHeading ? 60 : body.includes(phrase) ? 30 : 20,
			});
		}
	}
	return hits.sort((a, b) => b.score - a.score).slice(0, limit);
}
