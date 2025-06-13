<script lang="ts">
	import Icon from './Icon.svelte';
	import { SHELF_META, type Shelf } from '$lib/data/types';

	// A small shelf picker. Renders a trigger showing the current shelf and a popover
	// with the four shelves. `status` is the EFFECTIVE shelf to display; picking one
	// calls `onchange` with the chosen shelf (the caller persists it). Used on library
	// cards (`variant="chip"`, overlaid on the cover) and the series page
	// (`variant="button"`, an inline control). Designed to sit inside/around links —
	// every handler stops propagation so it never triggers card navigation.
	let {
		status = null,
		onchange,
		disabled = false,
		variant = 'chip',
		canComplete = true,
	}: {
		status?: Shelf | null;
		onchange: (s: Shelf) => void;
		disabled?: boolean;
		variant?: 'chip' | 'button';
		/**
		 * Whether "Completed" is an offerable shelf — false for a series that is still
		 * publishing. Reading everything that exists of an ongoing series is being
		 * caught up, not being finished, so the option is withheld rather than left to
		 * mean the wrong thing (the same word already means "the series ended" on the
		 * series/browse badges).
		 */
		canComplete?: boolean;
	} = $props();

	let open = $state(false);
	let root = $state<HTMLElement | null>(null);

	const ALL_OPTIONS = [
		{ key: 'reading', label: 'Reading' },
		{ key: 'completed', label: 'Completed' },
		{ key: 'onhold', label: 'On Hold' },
		{ key: 'plan', label: 'Plan to Read' },
	] as const;

	// "Completed" survives the gate when it is the CURRENT shelf even on an ongoing
	// series: rows filed before this gate existed (or filed while the series was
	// listed as ended, and it later resumed) would otherwise show a checked state the
	// menu doesn't contain, leaving the reader no visible way to move off it.
	const OPTIONS = $derived(
		ALL_OPTIONS.filter((o) => o.key !== 'completed' || canComplete || status === 'completed'),
	);

	const meta = $derived(status ? SHELF_META[status] : null);
	const label = $derived(meta ? meta.label : 'SET SHELF');

	function toggle(e: MouseEvent): void {
		e.preventDefault();
		e.stopPropagation();
		if (disabled) return;
		open = !open;
	}
	function pick(e: MouseEvent, s: Shelf): void {
		e.preventDefault();
		e.stopPropagation();
		open = false;
		if (s !== status) onchange(s);
	}
	// Dismiss on outside click (capture phase, so it beats card links) or Escape.
	$effect(() => {
		if (!open) return;
		const onDoc = (e: MouseEvent) => {
			if (root && !root.contains(e.target as Node)) open = false;
		};
		const onKey = (e: KeyboardEvent) => {
			if (e.key === 'Escape') open = false;
		};
		window.addEventListener('click', onDoc, true);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('click', onDoc, true);
			window.removeEventListener('keydown', onKey);
		};
	});
</script>

<div class="sm" class:open class:chip={variant === 'chip'} bind:this={root}>
	<button
		type="button"
		class="trigger {variant}"
		class:set={!!meta}
		{disabled}
		aria-haspopup="menu"
		aria-expanded={open}
		title="Change shelf"
		style={meta
			? `--sm-color:${meta.color};--sm-bg:${meta.bg};--sm-border:${meta.border}`
			: undefined}
		onclick={toggle}
	>
		<span class="trigger-label">{label}</span>
		<Icon name="chevron-down" size={variant === 'chip' ? 11 : 14} />
	</button>
	{#if open}
		<div class="menu" role="menu">
			{#each OPTIONS as o (o.key)}
				<button
					type="button"
					role="menuitemradio"
					aria-checked={status === o.key}
					class="item"
					class:sel={status === o.key}
					onclick={(e) => pick(e, o.key)}
				>
					<span class="dot" style="background:{SHELF_META[o.key].color}"></span>
					<span class="item-label">{o.label}</span>
					{#if status === o.key}<Icon name="check" size={14} strokeWidth={2.6} />{/if}
				</button>
			{/each}
		</div>
	{/if}
</div>

<style>
	.sm {
		position: relative;
		display: inline-flex;
	}
	.trigger {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		cursor: pointer;
		font-family: inherit;
		font-weight: 800;
		letter-spacing: 0.04em;
		border: 1px solid;
		transition: filter 0.12s;
	}
	.trigger:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.trigger:not(:disabled):hover {
		filter: brightness(1.12);
	}
	/* chip: compact overlay for the library cover */
	.trigger.chip {
		font-size: 10px;
		padding: 4px 8px;
		border-radius: 6px;
		color: var(--sm-color, var(--k-text));
		background: var(--sm-bg, rgba(12, 12, 13, 0.78));
		border-color: var(--sm-border, var(--k-border-3));
		backdrop-filter: blur(3px);
	}
	/* button: inline control for the series page */
	.trigger.button {
		font-size: 12.5px;
		padding: 0 14px;
		height: 50px;
		border-radius: 8px;
		color: var(--sm-color, var(--k-text));
		background: var(--sm-bg, transparent);
		border-color: var(--sm-border, var(--k-border-4));
	}
	.trigger.button:not(.set) {
		color: var(--k-text-3);
	}
	/* The popover's SURFACE lives here, on the base rule, so both variants get it.
	   It used to live entirely in `.sm.chip .menu` below — background, border,
	   padding, shadow and even the flex column — while the base rule carried only
	   geometry. `.chip` is applied only for `variant="chip"`, so the series page's
	   `variant="button"` menu matched none of it and rendered as bare transparent
	   text floating over the page. Keep this rule about appearance and the `.chip`
	   override about position only. */
	.menu {
		position: absolute;
		z-index: 40;
		top: calc(100% + 6px);
		left: 0;
		min-width: 176px;
		display: flex;
		flex-direction: column;
		padding: 6px;
		border-radius: 10px;
		background: var(--k-surface-3, var(--k-surface));
		border: 1px solid var(--k-border-3);
		box-shadow: 0 16px 40px rgba(0, 0, 0, 0.5);
	}
	/* The chip variant sits at the cover's bottom-right corner — open the menu
	   upward and right-aligned so it stays over the cover and doesn't spill past
	   the card edge or cover the title below. */
	.sm.chip .menu {
		top: auto;
		bottom: calc(100% + 6px);
		left: auto;
		right: 0;
	}
	.item {
		display: flex;
		align-items: center;
		gap: 10px;
		width: 100%;
		padding: 9px 10px;
		border: none;
		border-radius: 7px;
		background: transparent;
		color: var(--k-text-2);
		font-family: inherit;
		font-size: 13.5px;
		font-weight: 600;
		cursor: pointer;
		text-align: left;
		white-space: nowrap;
	}
	.item:hover {
		background: var(--k-hover-fill, rgba(255, 255, 255, 0.06));
		color: var(--k-text-bright);
	}
	.item.sel {
		color: var(--k-text-bright);
	}
	.item-label {
		flex: 1;
	}
	.dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		flex: 0 0 auto;
	}
</style>
