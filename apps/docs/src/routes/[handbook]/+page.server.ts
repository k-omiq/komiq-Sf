import { error } from '@sveltejs/kit';
import { getHandbook, getHandbooks } from '$lib/server/content';

export function entries() {
	return getHandbooks().map((book) => ({ handbook: book.id }));
}

export function load({ params }) {
	const book = getHandbook(params.handbook);
	if (!book) error(404, 'This page does not exist.');
	return { book };
}
