<script lang="ts">
	import { auth } from '$lib/auth.svelte';
	import { apiOrigin, canChooseServer } from '$lib/config';
	import Icon from './Icon.svelte';

	/** The page's own heading level: h1 on home, h2 under the library's title. */
	let { heading = 'h1' }: { heading?: 'h1' | 'h2' } = $props();

	// What a brand-new server shows on home and in the library: the owner is pointed
	// at the admin console (sources are added there), everyone else is told plainly.
	const owner = $derived(!!auth.user?.isAdmin);
	const adminHref = `${apiOrigin}/admin/sources/extensions`;
	// The native app cannot open the console beside itself, so it names the address.
	const adminAddress = `${apiOrigin}/admin`.replace(/^https?:\/\//, '');
</script>

<section class="empty" aria-labelledby="empty-catalog-title">
	<div class="empty-icon"><Icon name={owner ? 'plus' : 'book-open'} size={22} /></div>
	{#if owner}
		<svelte:element this={heading} id="empty-catalog-title" class="empty-title">
			Add sources in admin
		</svelte:element>
		<p class="empty-desc">
			This server has no series yet. Add an extension repo and a source in the admin console, and
			they show up here.
		</p>
		{#if canChooseServer}
			<p class="empty-desc">Open <code>{adminAddress}</code> in a browser.</p>
		{:else}
			<a class="empty-btn" href={adminHref}>Open admin</a>
		{/if}
	{:else}
		<svelte:element this={heading} id="empty-catalog-title" class="empty-title">
			Nothing here yet
		</svelte:element>
		<p class="empty-desc">Series show up here once the owner of this server adds them.</p>
	{/if}
</section>

<style>
	/* Same empty state as the library's own (icon, title, one line, one action). */
	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 14px;
		padding: 96px 20px;
		text-align: center;
	}
	.empty-icon {
		width: 56px;
		height: 56px;
		border-radius: 50%;
		background: var(--k-surface-2);
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--k-text-dim);
	}
	.empty-title {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 20px;
		color: var(--k-text);
	}
	.empty-desc {
		font-size: 14px;
		color: var(--k-text-dimmer);
		max-width: 360px;
		line-height: 1.55;
	}
	.empty-desc code {
		font-family: var(--k-font-mono);
		font-size: 13px;
		color: var(--k-text);
	}
	.empty-btn {
		margin-top: 6px;
		height: 42px;
		padding: 0 22px;
		display: inline-flex;
		align-items: center;
		border-radius: 8px;
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-weight: 700;
		font-size: 13.5px;
		text-decoration: none;
	}
	.empty-btn:hover {
		background: var(--k-primary-hover);
	}
</style>
