import { error } from '@sveltejs/kit';
import { allChapters, getChapter, getHandbook } from '$lib/server/content';

export function entries() {
	return allChapters().map((chapter) => ({ handbook: chapter.handbook, slug: chapter.slug }));
}

export function load({ params }) {
	const book = getHandbook(params.handbook);
	const chapter = getChapter(params.handbook, params.slug);
	if (!book || !chapter) error(404, 'This page does not exist.');
	const at = book.chapters.findIndex((c) => c.slug === chapter.slug);
	return {
		book: { title: book.title, href: book.href },
		chapter: {
			title: chapter.title,
			description: chapter.description,
			html: chapter.html,
			toc: chapter.toc,
		},
		number: at + 1,
		total: book.chapters.length,
		prev: book.chapters[at - 1] ?? null,
		next: book.chapters[at + 1] ?? null,
	};
}
