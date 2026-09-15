<script lang="ts">
	import { page } from '$app/state';
	import { SITE_NAME, SITE_URL } from '$lib/site';

	let {
		title,
		description,
	}: {
		/** Page title without the site name. Omit on the home page. */
		title?: string;
		description: string;
	} = $props();

	const full = $derived(title ? `${title} · ${SITE_NAME}` : SITE_NAME);
	const url = $derived(SITE_URL + (page.url.pathname.replace(/\/+$/, '') || '/'));
</script>

<svelte:head>
	<title>{full}</title>
	<meta name="description" content={description} />
	<link rel="canonical" href={url} />
	<meta property="og:title" content={full} />
	<meta property="og:description" content={description} />
	<meta property="og:url" content={url} />
	<meta property="og:image" content="{SITE_URL}/og-image.png" />
	<meta name="twitter:card" content="summary_large_image" />
</svelte:head>
