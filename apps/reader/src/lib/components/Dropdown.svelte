<script lang="ts" module>
	export interface DropdownOption<V extends string> {
		value: V;
		label: string;
		/** Optional secondary text, e.g. a count, shown dimmed at the row's end. */
		hint?: string | number;
	}
</script>

<script lang="ts" generics="T extends string">
	import { tick } from 'svelte';
	import Icon, { type IconName } from './Icon.svelte';

	// A single-select dropdown: a button showing the current choice that opens a
	// listbox of options. Follows the ARIA "select-only combobox" shape closely
	// enough for screen readers while staying a plain button + popup:
	//   - trigger: aria-haspopup="listbox", aria-expanded, aria-controls
	//   - popup: role="listbox" that takes focus and tracks the highlighted option
	//     with aria-activedescendant; options are role="option" + aria-selected
	//   - keys: ArrowUp/ArrowDown/Enter/Space open from the trigger; inside the list
	//     ArrowUp/ArrowDown/Home/End move, Enter/Space pick, Escape closes and
	//     returns focus, Tab closes; typing a letter jumps to the next match
	//   - an outside press closes without stealing focus
	// Styled from the shared tokens so it works in both themes.

	let {
		value,
		options,
		onchange,
		label,
		icon,
		prefix,
		align = 'left',
		disabled = false,
		class: className = '',
	}: {
		value: T;
		options: readonly DropdownOption<T>[];
		onchange: (value: T) => void;
		/** Accessible name of the control, e.g. "Sort by". */
		label: string;
		/** Leading icon inside the trigger. */
		icon?: IconName;
		/** Visible lead-in before the current value, e.g. "Sort:"; hidden on phones. */
		prefix?: string;
		/** Which edge of the trigger the popup lines up with. */
		align?: 'left' | 'right';
		disabled?: boolean;
		class?: string;
	} = $props();

	const uid = $props.id();
	const listId = `dd-list-${uid}`;
	const optId = (i: number) => `dd-opt-${uid}-${i}`;

	let open = $state(false);
	let active = $state(0);
	let root = $state<HTMLElement | null>(null);
	let trigger = $state<HTMLButtonElement | null>(null);
	let list = $state<HTMLElement | null>(null);

	const selectedIndex = $derived(
		Math.max(
			0,
			options.findIndex((o) => o.value === value),
		),
	);
	const current = $derived(options[selectedIndex]);

	async function show(at: 'selected' | 'first' | 'last' = 'selected'): Promise<void> {
		if (disabled || !options.length) return;
		active = at === 'first' ? 0 : at === 'last' ? options.length - 1 : selectedIndex;
		open = true;
		await tick();
		list?.focus();
		scrollActive();
	}
	function hide(returnFocus: boolean): void {
		open = false;
		if (returnFocus) trigger?.focus();
	}
	function pick(i: number): void {
		const o = options[i];
		hide(true);
		if (o && o.value !== value) onchange(o.value);
	}
	function scrollActive(): void {
		list?.querySelector<HTMLElement>(`#${CSS.escape(optId(active))}`)?.scrollIntoView({
			block: 'nearest',
		});
	}
	function move(next: number): void {
		active = (next + options.length) % options.length;
		scrollActive();
	}

	function onTriggerKey(e: KeyboardEvent): void {
		if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
			e.preventDefault();
			void show(e.key === 'ArrowDown' ? 'selected' : 'last');
		}
	}
	function onListKey(e: KeyboardEvent): void {
		switch (e.key) {
			case 'ArrowDown':
				move(active + 1);
				break;
			case 'ArrowUp':
				move(active - 1);
				break;
			case 'Home':
				move(0);
				break;
			case 'End':
				move(options.length - 1);
				break;
			case 'Enter':
			case ' ':
				pick(active);
				break;
			case 'Escape':
				e.stopPropagation();
				hide(true);
				break;
			case 'Tab':
				hide(false);
				return;
			default: {
				// Type-ahead: jump to the next option starting with the typed letter.
				if (e.key.length !== 1 || e.metaKey || e.ctrlKey || e.altKey) return;
				const ch = e.key.toLowerCase();
				for (let step = 1; step <= options.length; step++) {
					const i = (active + step) % options.length;
					if (options[i].label.toLowerCase().startsWith(ch)) {
						move(i);
						break;
					}
				}
			}
		}
		e.preventDefault();
	}

	$effect(() => {
		if (!open) return;
		const onDown = (e: PointerEvent) => {
			if (root && !root.contains(e.target as Node)) hide(false);
		};
		window.addEventListener('pointerdown', onDown, true);
		return () => window.removeEventListener('pointerdown', onDown, true);
	});
</script>

<div class="dd {className}" class:open bind:this={root}>
	<button
		type="button"
		class="dd-trigger"
		bind:this={trigger}
		{disabled}
		aria-haspopup="listbox"
		aria-expanded={open}
		aria-controls={listId}
		aria-label={`${label}: ${current?.label ?? ''}`}
		onclick={() => (open ? hide(false) : show())}
		onkeydown={onTriggerKey}
	>
		{#if icon}<span class="dd-icon"><Icon name={icon} size={15} /></span>{/if}
		{#if prefix}<span class="dd-prefix">{prefix}</span>{/if}
		<span class="dd-value">{current?.label ?? ''}</span>
		<span class="dd-chev"><Icon name="chevron-down" size={15} /></span>
	</button>
	<div
		class="dd-pop"
		class:right={align === 'right'}
		id={listId}
		role="listbox"
		tabindex="-1"
		aria-label={label}
		aria-activedescendant={open ? optId(active) : undefined}
		hidden={!open}
		bind:this={list}
		onkeydown={onListKey}
	>
		{#each options as o, i (o.value)}
			<!-- Options are driven by the listbox's keyboard handler (focus stays on
			     the listbox, aria-activedescendant tracks the highlight), so the
			     per-option click handler needs no key handler of its own. -->
			<!-- svelte-ignore a11y_click_events_have_key_events -->
			<div
				class="dd-opt"
				class:active={open && i === active}
				id={optId(i)}
				role="option"
				tabindex="-1"
				aria-selected={o.value === value}
				onpointermove={() => (active = i)}
				onclick={() => pick(i)}
			>
				<span class="dd-check"
					>{#if o.value === value}<Icon name="check" size={14} strokeWidth={2.6} />{/if}</span
				>
				<span class="dd-opt-label">{o.label}</span>
				{#if o.hint !== undefined}<span class="dd-hint">{o.hint}</span>{/if}
			</div>
		{/each}
	</div>
</div>

<style>
	.dd {
		position: relative;
		display: inline-flex;
		min-width: 0;
	}
	.dd-trigger {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		width: 100%;
		min-width: 0;
		height: 40px;
		padding: 0 10px 0 13px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		border: 1px solid var(--k-border-2);
		color: var(--k-text-2);
		font: inherit;
		font-size: 13px;
		font-weight: 700;
		white-space: nowrap;
		cursor: pointer;
		transition:
			border-color 0.15s,
			background-color 0.15s,
			color 0.15s;
	}
	.dd-trigger:hover:not(:disabled),
	.dd.open .dd-trigger {
		border-color: var(--k-border-strong);
		color: var(--k-text);
	}
	.dd-trigger:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
	}
	.dd-trigger:disabled {
		opacity: 0.55;
		cursor: default;
	}
	.dd-icon {
		display: inline-flex;
		color: var(--k-text-faint);
	}
	.dd-prefix {
		color: var(--k-text-faint);
		font-weight: 600;
	}
	.dd-value {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		text-align: left;
	}
	.dd-chev {
		display: inline-flex;
		color: var(--k-text-faint);
		transition: transform 0.18s;
	}
	.dd.open .dd-chev {
		transform: rotate(180deg);
	}
	.dd-pop {
		position: absolute;
		top: calc(100% + 6px);
		left: 0;
		z-index: 40;
		min-width: max(100%, 196px);
		width: max-content;
		max-width: calc(100vw - 2 * var(--k-gutter));
		max-height: min(360px, 60vh);
		overflow-y: auto;
		padding: 5px;
		border-radius: var(--k-radius-lg);
		background: var(--k-surface-3);
		border: 1px solid var(--k-border-3);
		box-shadow: 0 22px 60px rgba(0, 0, 0, 0.5);
		display: flex;
		flex-direction: column;
		gap: 2px;
		outline: none;
	}
	/* The highlighted option carries the focus ring; the popup itself must not also
	   pick up the global `[tabindex]:focus-visible` ring (or its smaller radius). */
	.dd-pop:focus-visible {
		outline: none;
		border-radius: var(--k-radius-lg);
	}
	.dd-pop[hidden] {
		display: none;
	}
	.dd-pop.right {
		left: auto;
		right: 0;
	}
	.dd-opt {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 40px;
		padding: 0 12px 0 8px;
		border-radius: var(--k-radius);
		color: var(--k-text-2);
		font-size: 13.5px;
		font-weight: 600;
		white-space: nowrap;
		cursor: pointer;
		user-select: none;
	}
	.dd-opt.active {
		background: var(--k-hover-fill);
		color: var(--k-text);
	}
	.dd-pop:focus-visible .dd-opt.active {
		outline: 2px solid var(--k-accent);
		outline-offset: -2px;
	}
	.dd-opt[aria-selected='true'] {
		color: var(--k-text-bright);
		font-weight: 700;
	}
	.dd-check {
		width: 16px;
		flex: 0 0 auto;
		display: inline-flex;
		justify-content: center;
		color: var(--k-accent);
	}
	.dd-opt-label {
		flex: 1;
	}
	.dd-hint {
		margin-left: 12px;
		font-size: 12px;
		font-weight: 600;
		color: var(--k-text-faint);
		font-variant-numeric: tabular-nums;
	}
	@media (max-width: 640px) {
		.dd-prefix {
			display: none;
		}
	}
</style>
