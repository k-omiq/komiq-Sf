<script lang="ts">
	import { page } from '$app/state';
	import Icon from '$lib/components/Icon.svelte';

	// Rendered when a load in the (app) group throws beyond the mock fallback, or
	// for unmatched routes (404). Keeps the error on-brand with a way back.
	const status = $derived(page.status);
	const is404 = $derived(status === 404);
	const heading = $derived(is404 ? 'Page not found' : 'Something went wrong');
	const message = $derived(
		is404
			? 'The page you’re looking for doesn’t exist or has moved.'
			: (page.error?.message ?? 'An unexpected error occurred while loading this page.'),
	);
</script>

<div class="err k-gutter">
	<div class="err-icon"><Icon name="alert" size={28} /></div>
	<div class="err-code">{status}</div>
	<h1>{heading}</h1>
	<p>{message}</p>
	<div class="err-btns">
		<a class="err-primary" href="/">Go home</a>
		<a class="err-ghost" href="/browse">Browse series</a>
	</div>
</div>

<style>
	.err {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 12px;
		padding: 120px 20px;
		text-align: center;
	}
	.err-icon {
		width: 64px;
		height: 64px;
		border-radius: 50%;
		display: flex;
		align-items: center;
		justify-content: center;
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-2);
		color: var(--k-accent);
	}
	.err-code {
		font-family: var(--k-font-mono);
		font-size: 13px;
		letter-spacing: 0.2em;
		color: var(--k-text-faint);
		margin-top: 4px;
	}
	h1 {
		font-size: 34px;
		margin: 0;
		color: var(--k-text-bright);
	}
	p {
		margin: 0;
		max-width: 380px;
		font-size: 14.5px;
		line-height: 1.6;
		color: var(--k-text-dim);
	}
	.err-btns {
		display: flex;
		gap: 12px;
		margin-top: 12px;
		flex-wrap: wrap;
		justify-content: center;
	}
	.err-primary,
	.err-ghost {
		height: 46px;
		padding: 0 24px;
		display: inline-flex;
		align-items: center;
		border-radius: 8px;
		font-weight: 700;
		font-size: 14px;
		text-decoration: none;
	}
	.err-primary {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.err-ghost {
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text);
	}
	.err-ghost:hover {
		border-color: rgba(255, 255, 255, 0.34);
	}
</style>
