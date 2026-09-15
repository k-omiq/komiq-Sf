<script lang="ts">
	import type { TagGroup, TagInfo } from '@komika/types';
	import type { TagMode } from '@komika/api';
	import { innerHeight } from 'svelte/reactivity/window';
	import Icon from '$lib/components/Icon.svelte';
	import {
		cycleTag,
		setTagState,
		shelveTags,
		tagState,
		withField,
		type BrowseState,
		type TagState,
	} from '$lib/data/browse-filters';

	/**
	 * A compact category preview with search, expansion, and a permanent selection
	 * shelf. Each tag cycles off → include → exclude → off. Match options hold the
	 * include/exclude list modes.
	 *
	 * Tri-state is not expressible with `aria-pressed` ("mixed" means partially pressed,
	 * not "excluded"), so each tag carries its state in its accessible NAME instead —
	 * "Romance, excluded" — and the visual never relies on colour alone: included tags get
	 * a plus, excluded ones a minus and a strike-through.
	 *
	 * With `source === 'genres'` (a server that predates the tag vocabulary) the list is
	 * the legacy genre facets and the picker is include-only and match-ANY, because that
	 * is all the legacy `genres` argument can express; it says so rather than offering
	 * controls the server would ignore.
	 */
	let {
		tags,
		source,
		loading,
		draft,
		onchange,
	}: {
		tags: TagInfo[];
		source: 'tags' | 'genres';
		loading: boolean;
		draft: BrowseState;
		onchange: (next: BrowseState) => void;
	} = $props();

	const uid = $props.id();
	let needle = $state('');
	let category = $state<TagGroup>('GENRE');
	let showAll = $state(false);
	// Shorter screens start with two rows; taller screens can show four.
	const previewCount = $derived((innerHeight.current ?? 900) <= 850 ? 4 : 8);
	const allShelves = $derived(shelveTags(tags, '', draft));
	const activeCategory = $derived(
		allShelves.some((shelf) => shelf.group === category) ? category : allShelves[0]?.group,
	);
	// Selected tags have a permanent shelf, so changing categories or searching never
	// hides a draft selection. The category list only contains unselected tags.
	const selectedTags = $derived(tags.filter((tag) => tagState(draft, tag.id) !== 'off'));
	const shelves = $derived(
		shelveTags(tags, needle, draft)
			.map((shelf) => ({
				...shelf,
				tags: shelf.tags.filter((tag) => tagState(draft, tag.id) === 'off'),
			}))
			.filter((shelf) => shelf.tags.length && (needle.trim() || shelf.group === activeCategory)),
	);
	const triState = $derived(source === 'tags');

	function tap(id: string) {
		const st = tagState(draft, id);
		onchange(
			triState ? cycleTag(draft, id) : setTagState(draft, id, st === 'off' ? 'include' : 'off'),
		);
	}

	// Each toggle leads with its list's DEFAULT (include: All, exclude: Any).
	const AND_FIRST: TagMode[] = ['AND', 'OR'];
	const OR_FIRST: TagMode[] = ['OR', 'AND'];

	const STATE_TEXT: Record<TagState, string> = {
		off: 'not filtered',
		include: 'included',
		exclude: 'excluded',
	};

	/** Arrow keys move between the two options of a mode toggle (radio-group pattern). */
	function modeKey(e: KeyboardEvent, key: 'inclMode' | 'exclMode') {
		if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(e.key)) return;
		e.preventDefault();
		const next: TagMode = draft[key] === 'AND' ? 'OR' : 'AND';
		onchange(withField(draft, key, next));
		const group = (e.currentTarget as HTMLElement).closest('[role="radiogroup"]');
		queueMicrotask(() => group?.querySelector<HTMLElement>(`[data-mode="${next}"]`)?.focus());
	}
</script>

{#snippet modeToggle(key: 'inclMode' | 'exclMode', label: string, first: TagMode)}
	<div class="mode">
		<span class="mode-label" id="{uid}-{key}">{label}</span>
		<div class="seg" role="radiogroup" aria-labelledby="{uid}-{key}">
			{#each first === 'AND' ? AND_FIRST : OR_FIRST as m (m)}
				<button
					type="button"
					role="radio"
					data-mode={m}
					aria-checked={draft[key] === m}
					tabindex={draft[key] === m ? 0 : -1}
					onclick={() => onchange(withField(draft, key, m))}
					onkeydown={(e) => modeKey(e, key)}>{m === 'AND' ? 'All' : 'Any'}</button
				>
			{/each}
		</div>
	</div>
{/snippet}

{#snippet tagButton(tag: TagInfo)}
	{@const state = tagState(draft, tag.id)}
	<button
		type="button"
		class="tag k-hit {state}"
		aria-label="{tag.name}, {STATE_TEXT[state]}"
		aria-describedby="{uid}-hint"
		title="{tag.count.toLocaleString()} series"
		onclick={() => tap(tag.id)}
	>
		{#if state !== 'off'}<span class="mark" aria-hidden="true"
				>{state === 'include' ? '+' : '-'}</span
			>{/if}
		<span class="tname">{tag.name}</span>
	</button>
{/snippet}

<div class="picker">
	<div class="tsearch">
		<Icon name="search" size={15} />
		<input
			type="search"
			bind:value={needle}
			placeholder="Search tags"
			aria-label="Search tags"
			autocomplete="off"
			aria-controls="{uid}-shelves"
		/>
		{#if needle}
			<button
				type="button"
				class="tclear k-hit"
				aria-label="Clear tag search"
				onclick={() => (needle = '')}
			>
				<Icon name="x" size={14} />
			</button>
		{/if}
	</div>

	{#if selectedTags.length}
		<div class="tags selected" role="group" aria-label="Selected tags">
			{#each selectedTags as tag (tag.id)}{@render tagButton(tag)}{/each}
		</div>
	{/if}

	{#if allShelves.length > 1 && !needle.trim()}
		<div class="categories" role="group" aria-label="Tag category">
			{#each allShelves as shelf (shelf.group)}
				<button
					type="button"
					aria-pressed={activeCategory === shelf.group}
					onclick={() => {
						category = shelf.group;
						showAll = false;
					}}>{shelf.label}</button
				>
			{/each}
		</div>
	{/if}

	<div class="shelves" id="{uid}-shelves" aria-busy={loading}>
		{#if loading}
			<div class="tag-skeleton" aria-label="Loading tags">
				{#each Array(6) as _}<span></span>{/each}
			</div>
		{:else if tags.length === 0}
			<p class="tempty">Tags aren't available yet.</p>
		{:else}
			{#each shelves as shelf (shelf.group)}
				<div class="shelf" role="group" aria-label={shelf.label}>
					{#if needle.trim()}<span class="shelf-label">{shelf.label}</span>{/if}
					<div class="tags">
						{#each showAll || needle.trim() ? shelf.tags : shelf.tags.slice(0, previewCount) as tag (tag.id)}
							{@render tagButton(tag)}
						{/each}
					</div>
				</div>
			{:else}
				<p class="tempty">
					{needle.trim() ? 'No matching tags to add.' : 'All tags in this category are selected.'}
				</p>
			{/each}
		{/if}
	</div>

	<div class="tag-tools">
		{#if !needle.trim() && (shelves[0]?.tags.length ?? 0) > previewCount}
			<button
				type="button"
				class="show-all k-hit"
				aria-expanded={showAll}
				onclick={() => (showAll = !showAll)}
			>
				{showAll ? 'Show fewer' : `Show all ${shelves[0].tags.length} tags`}
				<Icon name="chevron-down" size={13} />
			</button>
		{/if}
		{#if triState}
			<details class="match-options">
				<summary class="k-hit">Tag options<Icon name="chevron-down" size={13} /></summary>
				<p class="thint" id="{uid}-hint">Click to cycle: include (+), exclude (-), clear.</p>
				<div class="modes">
					{@render modeToggle('inclMode', 'Include', 'AND')}
					{@render modeToggle('exclMode', 'Exclude', 'OR')}
				</div>
				<p class="thint">
					Include All requires every selected tag. Exclude Any hides series with any excluded tag.
				</p>
			</details>
		{/if}
	</div>
	{#if !triState && !loading}
		<p class="thint" id="{uid}-hint">
			Matches any selected genre. Tag exclusions need a newer server.
		</p>
	{/if}
</div>

<style>
	.picker {
		display: flex;
		flex-direction: column;
		gap: 12px;
		min-width: 0;
	}
	.tsearch {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 40px;
		padding: 0 6px 0 10px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		color: var(--k-text-dim);
	}
	.tsearch:focus-within {
		border-color: var(--k-border-strong);
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
	}
	.tsearch input {
		flex: 1;
		min-width: 0;
		padding: 9px 0;
		background: transparent;
		border: none;
		outline: none;
		color: var(--k-text);
		font-size: 13px;
	}
	.tsearch input::placeholder {
		color: var(--k-text-dim);
		opacity: 1;
	}
	.tclear {
		width: 30px;
		height: 30px;
		border: none;
		background: transparent;
		color: var(--k-text-dim);
		cursor: pointer;
		display: inline-flex;
		align-items: center;
		justify-content: center;
	}
	.categories {
		display: flex;
		border-bottom: 1px solid var(--k-border-1);
	}
	.categories button {
		flex: 1;
		min-width: 0;
		min-height: 36px;
		padding: 4px 3px 10px;
		border: none;
		border-bottom: 2px solid transparent;
		background: transparent;
		color: var(--k-text-dim);
		font-size: 11.5px;
		cursor: pointer;
	}
	.categories button[aria-pressed='true'] {
		border-bottom-color: var(--k-text);
		color: var(--k-text);
		font-weight: 650;
	}
	.categories button:hover {
		color: var(--k-text);
	}
	.shelves,
	.shelf {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.shelf-label {
		font-size: 12px;
		font-weight: 600;
		color: var(--k-text-dim);
	}
	.tags,
	.tag-skeleton {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 6px;
	}
	.tag {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
		min-height: 36px;
		padding: 6px 10px;
		border-radius: var(--k-radius);
		background: transparent;
		border: 1px solid var(--k-border-2);
		color: var(--k-text-3);
		font-size: 12px;
		font-weight: 500;
		cursor: pointer;
		text-align: left;
		transition:
			background-color 0.12s,
			border-color 0.12s;
	}
	.tag:hover {
		background: var(--k-hover-fill);
		border-color: var(--k-border-strong);
	}
	.tag:active {
		transform: scale(0.98);
	}
	.mark {
		flex-shrink: 0;
		font-size: 14px;
		font-weight: 600;
	}
	.tname {
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.tag.include {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.tag.exclude {
		border-color: var(--k-border-strong);
		color: var(--k-text-dim);
	}
	.tag.exclude .tname {
		text-decoration: line-through;
	}
	.tag-tools {
		position: relative;
		min-height: 28px;
	}
	.tag-tools .show-all {
		position: absolute;
		top: 0;
		left: 0;
	}
	.match-options summary {
		margin-left: auto;
		width: fit-content;
	}
	.match-options[open] > .thint:first-of-type {
		margin-top: 8px;
	}
	.show-all,
	.match-options summary {
		display: flex;
		align-items: center;
		gap: 6px;
		min-height: 28px;
		padding: 0;
		border: none;
		background: transparent;
		color: var(--k-text-dim);
		font-size: 11.5px;
		cursor: pointer;
	}
	.show-all:hover,
	.match-options summary:hover {
		color: var(--k-text);
	}
	.show-all[aria-expanded='true'] :global(svg),
	.match-options[open] summary :global(svg) {
		transform: rotate(180deg);
	}
	.match-options summary {
		list-style: none;
	}
	.match-options summary::-webkit-details-marker {
		display: none;
	}
	.match-options summary:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
	}
	.modes {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 10px;
		margin: 8px 0 10px;
	}
	.mode {
		display: flex;
		flex-direction: column;
		gap: 5px;
	}
	.mode-label {
		font-size: 11.5px;
		color: var(--k-text-dim);
	}
	.seg {
		display: inline-flex;
		gap: 6px;
	}
	.seg button {
		flex: 1;
		min-height: 32px;
		padding: 3px 8px;
		border: 1px solid transparent;
		border-radius: var(--k-radius);
		background: transparent;
		color: var(--k-text-dim);
		font-size: 12px;
		cursor: pointer;
	}
	.seg button[aria-checked='true'] {
		background: var(--k-surface-4);
		border-color: var(--k-border-2);
		color: var(--k-text);
	}
	.thint,
	.tempty {
		margin: 0;
		font-size: 11px;
		line-height: 1.55;
		color: var(--k-text-dim);
	}
	.tag-skeleton span {
		height: 36px;
		border-radius: var(--k-radius);
		background: var(--k-surface-4);
	}
	@media (max-width: 959px) {
		.tag,
		.categories button,
		.seg button {
			min-height: 40px;
		}
		.tsearch input {
			font-size: 16px;
		}
	}
	/* Touch: full 44px rows. */
	@media (pointer: coarse) {
		.categories button,
		.seg button,
		.tsearch {
			min-height: 44px;
		}
	}
</style>
