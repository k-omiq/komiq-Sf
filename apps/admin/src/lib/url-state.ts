import { goto } from '$app/navigation';

/** Parse positive pagination within the GraphQL signed 32-bit Int range. */
export function queuePage(url: URL): number {
	const value = url.searchParams.get('page') ?? '1';
	const n = Number(value);
	return /^\d+$/.test(value) && Number.isSafeInteger(n) && n > 0 && n <= 2_147_483_647 ? n : 1;
}

/** Update only this control's parameters; keep unrelated context and fragment. */
export function queueNavigate(
	url: URL,
	values: Record<string, string | number | null>,
	replaceState = false,
) {
	const next = new URL(url);
	for (const [key, value] of Object.entries(values)) {
		if (value == null || (key === 'page' && value === 1)) next.searchParams.delete(key);
		else next.searchParams.set(key, String(value));
	}
	if (next.href === url.href) return Promise.resolve();
	return goto(next.pathname + next.search + next.hash, {
		keepFocus: true,
		noScroll: true,
		replaceState,
	});
}
