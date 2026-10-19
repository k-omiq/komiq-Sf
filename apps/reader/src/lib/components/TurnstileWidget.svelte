<script lang="ts">
	// Cloudflare Turnstile on a form, for a server that asks for it (`turnstile.svelte.ts`).
	// Renders nothing for a server that does not. In the web build the check runs as soon
	// as the form shows, mostly invisibly; a native app shows why it cannot sign in or
	// post here instead. The form calls `token()` when it sends its mutation.
	import { isTauri, TurnstileCheck, TURNSTILE_NATIVE_MESSAGE } from '@komika/api';
	import { tick } from 'svelte';
	import { loadTurnstileKey, turnstile } from '$lib/turnstile.svelte';

	const native = isTauri();
	let container = $state<HTMLDivElement>();
	let check: TurnstileCheck | null = null;

	$effect(() => {
		void loadTurnstileKey();
	});

	$effect(() => {
		const siteKey = turnstile.siteKey;
		if (!container || !siteKey || native) return;
		const current = new TurnstileCheck(container, siteKey);
		check = current;
		return () => {
			current.remove();
			if (check === current) check = null;
		};
	});

	/**
	 * The token for the next mutation: `undefined` when the server asks for no check.
	 * Throws a message the form shows when the check cannot run here or did not pass.
	 */
	export async function token(): Promise<string | undefined> {
		if (!(await loadTurnstileKey())) return undefined;
		if (native) throw new Error(TURNSTILE_NATIVE_MESSAGE);
		await tick();
		if (!check) throw new Error('The security check is still starting. Try again.');
		return check.token();
	}
</script>

{#if turnstile.siteKey}
	{#if native}
		<p class="note" role="note">{TURNSTILE_NATIVE_MESSAGE}</p>
	{:else}
		<div class="turnstile" bind:this={container}></div>
	{/if}
{/if}

<style>
	/* The widget is invisible unless Cloudflare asks for a click: no space of its own. */
	.turnstile {
		display: contents;
	}
	.note {
		margin: 0;
		color: var(--k-text-dim);
		font-size: 13px;
		line-height: 1.45;
	}
</style>
