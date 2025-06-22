<script lang="ts">
	import '@fontsource-variable/bricolage-grotesque';
	import '@fontsource-variable/manrope';
	import '@komika/ui/reset.css';
	import '@komika/ui/tokens.css';
	import '$lib/app.css';
	import { page } from '$app/state';
	import { initAuth } from '$lib/auth.svelte';
	import { initTheme } from '$lib/theme.svelte';
	import { markHydrated } from '$lib/stream';
	import { config } from '$lib/config';

	let { children } = $props();

	// Site-wide head defaults. A route that describes ITSELF (the series page) sets
	// `ownsMeta` in its `load`, and we stand down for the overridable meta tags — <head>
	// is not last-one-wins for og:*/twitter:*, so emitting both sets would leave a
	// crawler free to unfurl the generic "komiq / Read manga, together" instead of
	// the series. Route data is loaded before render, so this resolves during SSR.
	const ownsMeta = $derived(!!(page.data as { ownsMeta?: boolean }).ownsMeta);

	// Effects flush after hydration, so anything that runs here is past the initial
	// `load`. That's the signal page loads use to tell hydration (await, keep the
	// server-rendered markup) from a client navigation (stream, show skeletons).
	// See $lib/stream.
	$effect(() => {
		markHydrated();
	});

	// Restore + validate any persisted session once, client-side.
	$effect(() => {
		void initAuth();
	});
	// Restore the theme preference + track OS changes.
	$effect(() => {
		initTheme();
	});
</script>

<svelte:head>
	<!-- No default <title>: every route sets its own, so history and tabs name the
	     page (a chapter, a series, a search) instead of a wall of "komiq". -->
	{#if !ownsMeta}
		<meta
			name="description"
			content="Read manga, together. Track your library, follow updates, and discuss chapters."
		/>
	{/if}

	<!-- Page images and covers come from the image Worker on its own host, so the
	     first image fetch otherwise pays DNS + TCP + TLS before a byte moves. Open
	     that connection while the document is still parsing. -->
	{#if !config.imgDirect && config.imgWorkerBaseUrl}
		<link rel="preconnect" href={config.imgWorkerBaseUrl} />
	{/if}
	<link rel="icon" href="/favicon.ico" sizes="any" />
	<link rel="icon" type="image/png" sizes="32x32" href="/favicon-32.png" />
	<link rel="icon" type="image/png" sizes="16x16" href="/favicon-16.png" />
	<link rel="apple-touch-icon" href="/apple-touch-icon.png" />
	<link rel="manifest" href="/manifest.webmanifest" />
	<meta name="theme-color" content="#0c0c0d" />
	<meta name="apple-mobile-web-app-title" content="komiq" />

	<meta property="og:site_name" content="komiq" />
	{#if !ownsMeta}
		<meta property="og:type" content="website" />
		<meta property="og:title" content="komiq" />
		<meta property="og:description" content="Read manga, together." />
		<meta property="og:image" content="/og-image.png" />
		<meta name="twitter:card" content="summary_large_image" />
		<meta name="twitter:title" content="komiq" />
		<meta name="twitter:description" content="Read manga, together." />
		<meta name="twitter:image" content="/og-image.png" />
	{/if}
</svelte:head>

{@render children()}
