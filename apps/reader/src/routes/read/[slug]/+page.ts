import type { PageLoad } from './$types';
import { redirect } from '@sveltejs/kit';
import { getReaderChapter } from '$lib/data/source';

export const load: PageLoad = async ({ params, url }) => {
	const view = await getReaderChapter(
		params.slug,
		url.searchParams.get('ch'),
		url.searchParams.get('src'),
	);
	if (view.movedTo && view.movedTo.workId !== params.slug) {
		throw redirect(
			307,
			'/read/' +
				encodeURIComponent(view.movedTo.workId) +
				'?ch=' +
				encodeURIComponent(view.movedTo.chapterId),
		);
	}
	return view;
};
