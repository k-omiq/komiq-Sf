<script lang="ts">
	import type { TagInfo } from '@komika/types';
	import type { TagMode } from '@komika/api';
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
	 * The tag picker: every tag on its group's shelf, each a THREE-state toggle
	 * (off → include → exclude → off), with type-to-search and the AND/OR mode of each
	 * list.
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
	const shelves = $derived(shelveTags(tags, needle, draft));
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

<div class="picker">
	<div class="tsearch">
		<Icon name="search" size={15} />
		<input
			type="search"
			bind:value={needle}
			placeholder="Search tags…"
			aria-label="Search tags"
			autocomplete="off"
			aria-controls="{uid}-shelves"
		/>
		{#if needle}
			<button
				type="button"
				class="tclear"
				aria-label="Clear tag search"
				onclick={() => (needle = '')}><Icon name="x" size={14} /></button
			>
		{/if}
	</div>

	{#if triState}
		<div class="modes">
			{@render modeToggle('inclMode', 'Include', 'AND')}
			{@render modeToggle('exclMode', 'Exclude', 'OR')}
		</div>
		<p class="thint" id="{uid}-hint">
			Tap a tag once to include it, again to exclude it, and a third time to clear it.
			<strong>Include · All</strong> needs every included tag; <strong>Exclude · Any</strong> hides a
			series with any excluded tag.
		</p>
	{:else if !loading}
		<p class="thint" id="{uid}-hint">
			Shows series with <strong>any</strong> of the selected genres. Excluding tags needs a newer server.
		</p>
	{/if}

	<div class="shelves" id="{uid}-shelves" aria-busy={loading}>
		{#if loading}
			<span class="tempty">Loading tags…</span>
		{:else if tags.length === 0}
			<span class="tempty">Tags aren't available yet.</span>
		{:else}
			{#each shelves as sh (sh.group)}
				<div class="shelf" role="group" aria-labelledby="{uid}-g-{sh.group}">
					<span class="shelf-label" id="{uid}-g-{sh.group}">{sh.label}</span>
					<div class="tags">
						{#each sh.tags as t (t.id)}
							{@const st = tagState(draft, t.id)}
							<button
								type="button"
								class="tag {st}"
								aria-label="{t.name}, {STATE_TEXT[st]}"
								aria-describedby="{uid}-hint"
								onclick={() => tap(t.id)}
							>
								{#if st !== 'off'}<span class="mark" aria-hidden="true"
										>{st === 'include' ? '+' : '−'}</span
									>{/if}
								<span class="tname">{t.name}</span>
								<span class="tcount" aria-hidden="true">{t.count.toLocaleString()}</span>
							</button>
						{/each}
					</div>
				</div>
			{:else}
				<span class="tempty">No tags match “{needle}”.</span>
			{/each}
		{/if}
	</div>
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
		height: 38px;
		padding: 0 6px 0 12px;
		border-radius: var(--k-radius-md);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		color: var(--k-text-faint);
	}
	.tsearch:focus-within {
		border-color: var(--k-border-strong);
	}
	.tsearch input {
		flex: 1;
		min-width: 0;
		background: transparent;
		border: none;
		outline: none;
		color: var(--k-text);
		font-size: 13.5px;
	}
	.tclear {
		width: 28px;
		height: 28px;
		border: none;
		background: transparent;
		color: var(--k-text-faint);
		cursor: pointer;
		display: inline-flex;
		align-items: center;
		justify-content: center;
	}
	.modes {
		display: flex;
		flex-wrap: wrap;
		gap: 10px 16px;
	}
	.mode {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.mode-label {
		font-size: 12.5px;
		font-weight: 600;
		color: var(--k-text-dim);
	}
	.seg {
		display: inline-flex;
		padding: 2px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
	}
	.seg button {
		min-width: 44px;
		height: 28px;
		padding: 0 10px;
		border: none;
		border-radius: var(--k-radius-pill);
		background: transparent;
		color: var(--k-text-dim);
		font-size: 12px;
		font-weight: 700;
		cursor: pointer;
	}
	.seg button[aria-checked='true'] {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.thint {
		margin: 0;
		font-size: 12px;
		line-height: 1.45;
		color: var(--k-text-faint);
	}
	.thint strong {
		color: var(--k-text-dim);
		font-weight: 700;
	}
	.shelves {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.shelf {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.shelf-label {
		font-size: 11.5px;
		font-weight: 700;
		color: var(--k-text-dim);
	}
	.tags {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.tag {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		max-width: 100%;
		min-height: 30px;
		padding: 4px 10px;
		border-radius: var(--k-radius);
		background: transparent;
		border: 1px solid var(--k-border-3);
		color: var(--k-text-3);
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
		text-align: left;
		transition:
			color 0.12s,
			background-color 0.12s,
			border-color 0.12s;
	}
	.tag:hover {
		border-color: var(--k-border-strong);
	}
	.mark {
		width: 10px;
		font-weight: 800;
		text-align: center;
		flex: 0 0 auto;
	}
	.tname {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.tcount {
		font-size: 10.5px;
		font-weight: 700;
		color: var(--k-text-faint);
		flex: 0 0 auto;
	}
	/* Include / exclude tints are mixed from the status tokens, so they follow the theme
	   (the light ramp darkens them) without a hard-coded colour per theme. */
	.tag.include {
		background: color-mix(in srgb, var(--k-ongoing) 16%, transparent);
		border-color: color-mix(in srgb, var(--k-ongoing) 60%, transparent);
		color: var(--k-text-bright);
	}
	.tag.include .mark {
		color: var(--k-ongoing);
	}
	.tag.exclude {
		background: color-mix(in srgb, var(--k-cancelled) 14%, transparent);
		border-color: color-mix(in srgb, var(--k-cancelled) 60%, transparent);
		color: var(--k-text-bright);
	}
	.tag.exclude .mark {
		color: var(--k-cancelled);
	}
	.tag.exclude .tname {
		text-decoration: line-through;
		text-decoration-thickness: 1.5px;
	}
	.tempty {
		font-size: 12.5px;
		color: var(--k-text-faint);
		padding: 4px 2px;
	}
</style>
