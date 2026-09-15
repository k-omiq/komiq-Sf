<script lang="ts">
	import type { Snippet } from 'svelte';
	import mark from '$lib/assets/mark.png';

	// A full-page screen that stands in for the whole app (the server is not set up,
	// or the account must pick a new password first). Same card, fields and primary
	// button as the sign-in page, under the brand, with no app chrome around it:
	// nothing behind it is usable yet.
	let {
		title,
		lead,
		children,
		foot,
	}: {
		title: string;
		lead?: string;
		children: Snippet;
		/** Quiet text under the card (a secondary action). */
		foot?: Snippet;
	} = $props();
</script>

<div class="screen">
	<div class="brand" role="img" aria-label="komiq">
		<img class="brand-mark" width="32" height="32" src={mark} alt="" />
		<span>omiq</span>
	</div>
	<section class="card" aria-labelledby="account-screen-title">
		<div class="head">
			<h1 id="account-screen-title">{title}</h1>
			{#if lead}<p class="sub">{lead}</p>{/if}
		</div>
		{@render children()}
	</section>
	{#if foot}<div class="foot">{@render foot()}</div>{/if}
</div>

<style>
	.screen {
		min-height: 100vh;
		min-height: 100dvh;
		display: flex;
		flex-direction: column;
		align-items: center;
		/* No app chrome here, so the screen itself keeps clear of the status bar / notch,
		   the home indicator and a landscape notch. */
		padding: max(clamp(40px, 14vh, 128px), calc(var(--k-safe-top) + var(--k-space-6)))
			max(var(--k-space-4), var(--k-safe-right)) calc(var(--k-space-10) + var(--k-safe-bottom))
			max(var(--k-space-4), var(--k-safe-left));
		background: var(--k-bg);
	}
	.brand {
		display: inline-flex;
		align-items: baseline;
		margin-bottom: var(--k-space-6);
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 22px;
		letter-spacing: -0.01em;
		color: var(--k-text);
	}
	.brand-mark {
		width: auto;
		height: 0.8em;
		margin-right: 1.5px;
		transform: translateY(0.07em);
	}
	.card {
		width: 100%;
		max-width: 400px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius-xl);
		padding: var(--k-space-8);
	}
	.head {
		margin-bottom: var(--k-space-6);
	}
	h1 {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 26px;
		letter-spacing: -0.02em;
		line-height: 1.2;
		color: var(--k-text-bright);
	}
	.sub {
		margin-top: var(--k-space-2);
		color: var(--k-text-dim);
		font-size: 14px;
		line-height: 1.5;
	}
	.foot {
		margin-top: var(--k-space-5);
		text-align: center;
		font-size: 13px;
		color: var(--k-text-dim);
	}

	/* The form parts, shared by every screen built on this one (same as sign-in). */
	.card :global(form) {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-4);
	}
	.card :global(label) {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-2);
	}
	.card :global(label > span) {
		font-size: 12px;
		font-weight: 600;
		color: var(--k-text-dim);
		letter-spacing: 0.01em;
	}
	.card :global(input) {
		width: 100%;
		height: 44px;
		padding: 0 var(--k-space-4);
		background: var(--k-surface);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-md);
		color: var(--k-text);
		font-family: var(--k-font-sans);
		font-size: 14px;
		transition: border-color 0.15s;
	}
	.card :global(input:focus) {
		outline: none;
		border-color: var(--k-border-strong);
	}
	.card :global(.field-error) {
		color: var(--k-danger);
		font-size: 12.5px;
		line-height: 1.4;
	}
	.card :global(.error) {
		padding: var(--k-space-3) var(--k-space-4);
		background: var(--k-danger-soft);
		border-radius: var(--k-radius);
		color: var(--k-danger);
		font-size: 13px;
		line-height: 1.4;
	}
	.card :global(.submit) {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 100%;
		height: 46px;
		margin-top: var(--k-space-1);
		border: none;
		border-radius: var(--k-radius-md);
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-family: var(--k-font-sans);
		font-size: 14px;
		font-weight: 700;
		text-decoration: none;
		cursor: pointer;
		transition: background 0.15s;
	}
	.card :global(.submit:hover:not(:disabled)) {
		background: var(--k-primary-hover);
	}
	.card :global(.submit:disabled) {
		opacity: 0.6;
		cursor: default;
	}
	.foot :global(.link) {
		background: none;
		border: none;
		padding: 4px 6px;
		color: var(--k-text-dim);
		font-family: var(--k-font-sans);
		font-size: 13px;
		font-weight: 600;
		cursor: pointer;
	}
	.foot :global(.link:hover) {
		color: var(--k-text);
	}
	@media (pointer: coarse) {
		.foot :global(.link) {
			min-width: 44px;
			min-height: 44px;
		}
	}
</style>
