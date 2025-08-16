<script lang="ts">
	import { tick } from 'svelte';
	import type { BrowseSortField, SortDirection } from '@komika/api';
	import Icon from '$lib/components/Icon.svelte';
	import { SORT_LABEL, dirApplies, dirLabel } from '$lib/data/browse-filters';

	/**
	 * "Sort by" — a listbox menu button plus a direction toggle.
	 *
	 * A custom listbox rather than a native <select> because the options change with the
	 * query (Relevance exists only when there is something to rank) and the direction
	 * toggle has to sit inside the same visual control. Keyboard model follows the WAI-ARIA
	 * "select-only combobox / listbox popup" pattern with ROVING focus: the open list moves
	 * real DOM focus between options (so a screen reader announces each one without
	 * aria-activedescendant bookkeeping), Arrow/Home/End move, Enter/Space choose, Escape
	 * closes and returns focus to the button, Tab closes and lets focus move on, a typed
	 * letter jumps to the next option starting with it. Outside pointer-down closes.
	 */
	let {
		options,
		value,
		dir,
		onselect,
		ontoggledir,
	}: {
		options: BrowseSortField[];
		/** The EFFECTIVE sort (never null) — what the results are actually ordered by. */
		value: BrowseSortField;
		dir: SortDirection;
		onselect: (sort: BrowseSortField) => void;
		ontoggledir: () => void;
	} = $props();

	const uid = $props.id();
	let open = $state(false);
	let active = $state(0);
	let root: HTMLDivElement | undefined = $state();
	let button: HTMLButtonElement | undefined = $state();
	let list: HTMLUListElement | undefined = $state();

	function focusActive() {
		const el = list?.querySelectorAll<HTMLElement>('[role="option"]')[active];
		el?.focus();
	}

	async function openMenu(at?: 'first' | 'last') {
		const sel = options.indexOf(value);
		active = at === 'first' ? 0 : at === 'last' ? options.length - 1 : Math.max(0, sel);
		open = true;
		await tick();
		focusActive();
	}

	function close(returnFocus: boolean) {
		open = false;
		if (returnFocus) button?.focus();
	}

	function choose(i: number) {
		const v = options[i];
		close(true);
		if (v && v !== value) onselect(v);
	}

	// Enter/Space are left to the button's native click (which toggles); only the arrows
	// need handling here. ArrowDown opens on the current choice, ArrowUp on the last one.
	function onButtonKey(e: KeyboardEvent) {
		if (e.key === 'ArrowDown') {
			e.preventDefault();
			void openMenu();
		} else if (e.key === 'ArrowUp') {
			e.preventDefault();
			void openMenu('last');
		}
	}

	function move(to: number) {
		const n = options.length;
		active = ((to % n) + n) % n;
		focusActive();
	}

	function onListKey(e: KeyboardEvent) {
		switch (e.key) {
			case 'ArrowDown':
				e.preventDefault();
				move(active + 1);
				break;
			case 'ArrowUp':
				e.preventDefault();
				move(active - 1);
				break;
			case 'Home':
				e.preventDefault();
				move(0);
				break;
			case 'End':
				e.preventDefault();
				move(options.length - 1);
				break;
			case 'Enter':
			case ' ':
				e.preventDefault();
				choose(active);
				break;
			case 'Escape':
				// Stop here so an enclosing sheet/dialog doesn't ALSO close on this Escape.
				e.preventDefault();
				e.stopPropagation();
				close(true);
				break;
			case 'Tab':
				close(false);
				break;
			default:
				// Type-ahead: next option (after the current one) starting with the letter.
				if (e.key.length === 1 && /\S/.test(e.key)) {
					const k = e.key.toLowerCase();
					for (let step = 1; step <= options.length; step++) {
						const i = (active + step) % options.length;
						if (SORT_LABEL[options[i]].toLowerCase().startsWith(k)) {
							move(i);
							break;
						}
					}
				}
		}
	}

	function onWindowPointer(e: PointerEvent) {
		if (open && root && !root.contains(e.target as Node)) close(false);
	}
</script>

<svelte:window onpointerdown={onWindowPointer} />

<div class="sort" bind:this={root}>
	<span class="sort-label" id="{uid}-label">Sort by</span>
	<div class="menu-wrap">
		<button
			bind:this={button}
			type="button"
			class="menu-btn"
			aria-haspopup="listbox"
			aria-expanded={open}
			aria-controls="{uid}-list"
			aria-labelledby="{uid}-label {uid}-value"
			onclick={() => (open ? close(true) : openMenu())}
			onkeydown={onButtonKey}
		>
			<span id="{uid}-value">{SORT_LABEL[value]}</span>
			<Icon name="chevron-down" size={15} />
		</button>
		{#if open}
			<ul
				bind:this={list}
				id="{uid}-list"
				class="menu"
				role="listbox"
				aria-labelledby="{uid}-label"
				tabindex="-1"
				onkeydown={onListKey}
			>
				{#each options as o, i (o)}
					<!-- Keyboard handling lives on the listbox (roving focus), not per option. -->
					<!-- svelte-ignore a11y_click_events_have_key_events -->
					<li
						role="option"
						aria-selected={o === value}
						tabindex={i === active ? 0 : -1}
						class:active={i === active}
						onclick={() => choose(i)}
						onpointermove={() => {
							if (active !== i) {
								active = i;
								focusActive();
							}
						}}
					>
						<span class="check" aria-hidden="true">
							{#if o === value}<Icon name="check" size={14} strokeWidth={2.6} />{/if}
						</span>
						{SORT_LABEL[o]}
					</li>
				{/each}
			</ul>
		{/if}
	</div>
	<!-- Hidden (not disabled) for Relevance: "least relevant first" is not an order, and a
	     greyed-out control next to the menu reads as broken rather than as inapplicable. -->
	{#if dirApplies(value)}
		<button
			type="button"
			class="dir-btn"
			onclick={ontoggledir}
			aria-label="Order: {dirLabel(value, dir)}. Switch to {dirLabel(
				value,
				dir === 'ASC' ? 'DESC' : 'ASC',
			)}"
			title="Switch to {dirLabel(value, dir === 'ASC' ? 'DESC' : 'ASC')}"
		>
			<span class="dir-arrow" class:asc={dir === 'ASC'} aria-hidden="true">
				<Icon name="arrow-up-down" size={15} />
			</span>
			<span class="dir-text">{dirLabel(value, dir)}</span>
		</button>
	{/if}
</div>

<style>
	.sort {
		display: flex;
		align-items: center;
		gap: 8px;
		min-width: 0;
	}
	.sort-label {
		font-size: 12.5px;
		color: var(--k-text-faint);
		white-space: nowrap;
	}
	.menu-wrap {
		position: relative;
	}
	.menu-btn,
	.dir-btn {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		height: 36px;
		padding: 0 12px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		color: var(--k-text);
		font-size: 13px;
		font-weight: 700;
		cursor: pointer;
		white-space: nowrap;
		transition: border-color 0.15s;
	}
	.menu-btn:hover,
	.dir-btn:hover,
	.menu-btn[aria-expanded='true'] {
		border-color: var(--k-border-strong);
	}
	.dir-btn {
		color: var(--k-text-2);
		font-weight: 600;
	}
	.dir-arrow {
		display: inline-flex;
		transition: transform 0.15s;
	}
	.dir-arrow.asc {
		transform: scaleY(-1);
	}
	.menu {
		position: absolute;
		top: calc(100% + 6px);
		right: 0;
		z-index: 35;
		min-width: 200px;
		/* Never wider than the viewport minus the phone gutter: on a 360px screen the
		   button sits at the right edge, so a right-anchored list grows leftwards. */
		max-width: calc(100vw - 32px);
		margin: 0;
		padding: 6px;
		list-style: none;
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-lg);
		box-shadow: 0 16px 40px rgba(0, 0, 0, 0.35);
	}
	.menu li {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 9px 10px;
		border-radius: var(--k-radius);
		font-size: 13.5px;
		font-weight: 600;
		color: var(--k-text-2);
		cursor: pointer;
		outline: none;
	}
	.menu li.active {
		background: var(--k-hover-fill);
		color: var(--k-text-bright);
		box-shadow: inset 0 0 0 1px var(--k-border-3);
	}
	.menu li[aria-selected='true'] {
		color: var(--k-text-bright);
	}
	.check {
		width: 16px;
		display: inline-flex;
		justify-content: center;
		color: var(--k-accent);
	}
	@media (max-width: 480px) {
		/* The phone toolbar is tight: the "Sort by" caption goes (the button still carries
		   it in its accessible name via aria-labelledby), and the direction toggle shows
		   just its arrow — its full wording is in its aria-label and title. */
		.sort-label {
			position: absolute;
			width: 1px;
			height: 1px;
			overflow: hidden;
			clip: rect(0 0 0 0);
			white-space: nowrap;
		}
		.dir-text {
			display: none;
		}
		.dir-btn {
			width: 36px;
			padding: 0;
			justify-content: center;
		}
	}
</style>
