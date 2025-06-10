<script lang="ts">
	import { goto } from '$app/navigation';
	import Icon from './Icon.svelte';

	// A plain search box: type a term, Enter (or the search icon) opens /browse with
	// the query. All filtration (genre / status / sort / rating / NSFW) lives on the
	// Browse page now — the search bar no longer carries an "Advanced search" panel.
	let {
		open = $bindable(false),
		placeholder = 'Search series, authors, genres…',
	}: { open?: boolean; placeholder?: string } = $props();

	let q = $state('');
	let inputEl = $state<HTMLInputElement | undefined>();

	$effect(() => {
		if (open && inputEl) {
			const t = setTimeout(() => inputEl?.focus(), 30);
			return () => clearTimeout(t);
		}
	});

	function close() {
		open = false;
	}
	function submit() {
		const term = q.trim();
		close();
		goto(term ? `/browse?q=${encodeURIComponent(term)}` : '/browse');
	}
	function onkey(e: KeyboardEvent) {
		if (e.key === 'Escape') close();
		else if (e.key === 'Enter') submit();
	}
</script>

{#if open}
	<button class="scrim" aria-label="Close search" onclick={close}></button>
	<div class="panel">
		<div class="bar">
			<button class="search-btn" aria-label="Search" onclick={submit}>
				<Icon name="search" size={21} stroke="#87857f" />
			</button>
			<input bind:this={inputEl} bind:value={q} {placeholder} onkeydown={onkey} />
			<button class="esc" onclick={close}>ESC</button>
		</div>
	</div>
{/if}

<style>
	.scrim {
		position: fixed;
		inset: 0;
		z-index: 40;
		border: none;
		background: rgba(8, 8, 9, 0.55);
		backdrop-filter: blur(4px);
		cursor: default;
	}
	.panel {
		position: fixed;
		z-index: 50;
		top: 84px;
		left: 50%;
		transform: translateX(-50%);
		width: min(700px, calc(100vw - 40px));
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.bar {
		display: flex;
		align-items: center;
		gap: 14px;
		height: 62px;
		padding: 0 8px 0 20px;
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-pill);
		box-shadow: 0 26px 70px rgba(0, 0, 0, 0.55);
	}
	.search-btn {
		flex: 0 0 auto;
		display: flex;
		align-items: center;
		justify-content: center;
		background: transparent;
		border: none;
		padding: 0;
		cursor: pointer;
	}
	input {
		flex: 1;
		min-width: 0;
		background: transparent;
		border: none;
		outline: none;
		color: var(--k-text);
		font-size: 16px;
	}
	.esc {
		font-size: 12px;
		font-weight: 700;
		color: var(--k-text-faint);
		border: 1px solid var(--k-border-4);
		border-radius: 6px;
		padding: 4px 8px;
		background: transparent;
		cursor: pointer;
	}
</style>
