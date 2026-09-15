import { json } from '@sveltejs/kit';
import { allChapters, getHandbook } from '$lib/server/content';
import type { SearchPage } from '$lib/search';

export const prerender = true;

/** The whole site as one file, fetched the first time someone opens search. */
export function GET() {
	const pages: SearchPage[] = allChapters().map((chapter) => ({
		href: chapter.href,
		title: chapter.title,
		book: getHandbook(chapter.handbook)?.title ?? '',
		description: chapter.description,
		sections: chapter.sections.map((s) => ({ id: s.id, heading: s.heading, text: s.text })),
	}));
	return json(pages);
}
