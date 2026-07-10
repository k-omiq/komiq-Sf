/** Bust the browser's image cache without rewriting signed upstream URLs. */
export function retryCoverUrl(url: string, token: string, cacheOrigins: readonly string[]): string {
	if (!url || !token || url.startsWith('blob:') || url.startsWith('data:')) return url;
	// Direct external URLs may be signed. Only our known image-serving origins
	// support extra recovery parameters; other URLs are retried by remounting.
	try {
		if (!cacheOrigins.some((origin) => new URL(origin).origin === new URL(url).origin)) return url;
	} catch {
		return url;
	}
	const [path, hash] = url.split('#', 2);
	return `${path}${path.includes('?') ? '&' : '?'}retry=1&_coverRetry=${encodeURIComponent(token)}${hash ? `#${hash}` : ''}`;
}

/** Every DPR candidate must retry, otherwise srcset masks the new src. */
export function retryCoverSrcSet(
	srcset: string,
	token: string,
	cacheOrigins: readonly string[],
): string {
	if (!srcset || !token) return srcset;
	return srcset
		.split(', ')
		.map((candidate) => {
			const space = candidate.lastIndexOf(' ');
			return `${retryCoverUrl(candidate.slice(0, space), token, cacheOrigins)}${candidate.slice(space)}`;
		})
		.join(', ');
}
