<script lang="ts" module>
	/**
	 * Below this the panel is a bottom SHEET (a modal dialog) rather than a rail. Wider
	 * than the app's 640px phone breakpoint on purpose: the v2 panel is ~300px of tags and
	 * fields, and beside a results grid on a tablet it would leave room for two cards. The
	 * CSS `@media (max-width: 959px)` blocks below must match it.
	 */
	export const SHEET_QUERY = '(max-width: 959px)';
</script>

<script lang="ts">
	import type { TagInfo } from '@komika/types';
	import Icon from '$lib/components/Icon.svelte';
	import { mobileModalFocus } from '$lib/modal-focus';
	import {
		CONTENT_RATINGS,
		CONTENT_RATING_LABEL,
		DEMOGRAPHICS,
		DEMOGRAPHIC_LABEL,
		LANGUAGES,
		MAX_MIN_CHAPTERS,
		MAX_MIN_FOLLOWS,
		SFW_CONTENT_RATINGS,
		STATUSES,
		STATUS_LABEL,
		TYPES,
		TYPE_LABEL,
		YEAR_MAX,
		YEAR_MIN,
		languageLabel,
		normalize,
		parseNumberInput,
		toggleIn,
		withField,
		type BrowseState,
	} from '$lib/data/browse-filters';
	import ChipGroup from './ChipGroup.svelte';
	import TagPicker from './TagPicker.svelte';

	/**
	 * The filter panel: an inline rail on desktop, a full-height bottom sheet below
	 * {@link SHEET_QUERY}. ONE element serves both (CSS decides), so there is a single
	 * set of controls to keep in sync.
	 *
	 * It edits a DRAFT, never the URL: nothing is fetched until Apply, which is one
	 * history entry for the whole batch of changes (so Back undoes "the filters I just
	 * applied", not one checkbox at a time) and one request instead of one per tap. The
	 * page owns the draft and re-bases it whenever the APPLIED filters change underneath
	 * (a chip removed, Back) — see `filtersOnlySig`.
	 */
	let {
		open,
		draft,
		onchange,
		tags,
		tagSource,
		tagsLoading,
		nsfw,
		signedIn,
		changes,
		canReset,
		onapply,
		onreset,
		onclose,
	}: {
		/** The sheet is showing (ignored on desktop, where the rail always is). */
		open: boolean;
		draft: BrowseState;
		onchange: (next: BrowseState) => void;
		tags: TagInfo[];
		tagSource: 'tags' | 'genres';
		tagsLoading: boolean;
		/** The viewer opted into NSFW — gates the Erotica/Pornographic chips. */
		nsfw: boolean;
		signedIn: boolean;
		/** How many clauses Apply would change. */
		changes: number;
		canReset: boolean;
		onapply: () => void;
		onreset: () => void;
		/** Close the sheet WITHOUT applying (X, scrim, Escape). */
		onclose: () => void;
	} = $props();

	const uid = $props.id();

	const statusOpts = STATUSES.map((v) => ({ value: v, label: STATUS_LABEL[v] }));
	const typeOpts = TYPES.map((v) => ({
		value: v,
		label: TYPE_LABEL[v],
	}));
	const demoOpts = DEMOGRAPHICS.map((v) => ({ value: v, label: DEMOGRAPHIC_LABEL[v] }));
	const ratingOpts = $derived(
		(nsfw ? CONTENT_RATINGS : SFW_CONTENT_RATINGS).map((v) => ({
			value: v,
			label: CONTENT_RATING_LABEL[v],
		})),
	);
	// The curated list, plus any other code a shared link carried, so a selected language
	// is always visible (and removable) where it was set.
	const langOpts = $derived([
		...LANGUAGES.map((l) => ({ value: l.code, label: l.label })),
		...draft.languages
			.filter((c) => !LANGUAGES.some((l) => l.code === c))
			.map((c) => ({ value: c, label: languageLabel(c) })),
	]);

	type NumKey = 'yearFrom' | 'yearTo' | 'minChapters' | 'minRating' | 'minFollows';

	/**
	 * Commit a number field on `change` (blur / Enter), not per keystroke: half-typed
	 * years ("20") would otherwise clamp to 1900 under the viewer's fingers. The field is
	 * then rewritten with the normalised value, so a clamp (3000 → 2100) is visible even
	 * when it leaves the stored value unchanged.
	 */
	function commitNumber(e: Event, key: NumKey) {
		const input = e.currentTarget as HTMLInputElement;
		const next = normalize(withField(draft, key, parseNumberInput(input.value)));
		onchange(next);
		input.value = next[key] == null ? '' : String(next[key]);
	}

	function onKeydown(e: KeyboardEvent) {
		if (e.key === 'Escape' && open) {
			e.preventDefault();
			onclose();
		}
	}
</script>

{#snippet numberField(
	key: NumKey,
	label: string,
	opts: { min: number; max: number; step?: number; placeholder: string; decimal?: boolean },
)}
	<label class="num">
		<span class="num-label">{label}</span>
		<input
			type="number"
			inputmode={opts.decimal ? 'decimal' : 'numeric'}
			min={opts.min}
			max={opts.max}
			step={opts.step ?? 1}
			placeholder={opts.placeholder}
			value={draft[key] ?? ''}
			onchange={(e) => commitNumber(e, key)}
		/>
	</label>
{/snippet}

{#if open}
	<!-- The scrim is a sibling, marked so the modal focus helper leaves it interactive. -->
	<button
		type="button"
		class="scrim"
		data-modal-backdrop
		tabindex="-1"
		aria-label="Close filters"
		onclick={onclose}
	></button>
{/if}
<!-- Below SHEET_QUERY this element IS the dialog (role set by `mobileModalFocus`), and
     Escape-to-close belongs on the dialog itself. -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<aside
	class="panel"
	class:open
	aria-labelledby="{uid}-title"
	use:mobileModalFocus={{ open, query: SHEET_QUERY, label: 'Filters' }}
	onkeydown={onKeydown}
>
	<header class="panel-head">
		<h2 class="panel-title" id="{uid}-title">Filters</h2>
		<div class="head-actions">
			<button type="button" class="reset" disabled={!canReset} onclick={onreset}>Reset</button>
			<button type="button" class="close" aria-label="Close filters" onclick={onclose}>
				<Icon name="x" size={20} />
			</button>
		</div>
	</header>

	<div class="panel-body">
		<section class="section" aria-labelledby="{uid}-tags">
			<h3 class="glabel" id="{uid}-tags">Tags</h3>
			<TagPicker {tags} source={tagSource} loading={tagsLoading} {draft} {onchange} />
		</section>

		<ChipGroup
			legend="Status"
			initialOpen
			options={statusOpts}
			selected={draft.status}
			ontoggle={(v) => onchange(toggleIn(draft, 'status', v))}
		/>
		<ChipGroup
			legend="Format"
			initialOpen
			segmented
			options={typeOpts}
			selected={draft.types}
			ontoggle={(v) => onchange(toggleIn(draft, 'types', v))}
		/>
		<ChipGroup
			legend="Demographic"
			options={demoOpts}
			selected={draft.demographics}
			ontoggle={(v) => onchange(toggleIn(draft, 'demographics', v))}
		/>
		<ChipGroup
			legend="Content rating"
			options={ratingOpts}
			selected={draft.contentRatings}
			ontoggle={(v) => onchange(toggleIn(draft, 'contentRatings', v))}
		>
			{#snippet hint()}
				{#if !nsfw}
					<!-- The server never widens past the viewer's NSFW gate, so offering these
					     two would be a guaranteed-empty grid, not a way in. Say where the gate
					     actually lives instead. -->
					{#if signedIn}
						Erotica and Pornographic are hidden by your content settings. Turn on
						<a href="/profile#profile-preferences">Show NSFW content</a> to filter by them.
					{:else}
						Erotica and Pornographic are hidden. <a href="/login">Sign in</a> and turn on Show NSFW content
						in your profile to filter by them.
					{/if}
				{:else}
					Pick none to see every rating your settings allow.
				{/if}
			{/snippet}
		</ChipGroup>
		<ChipGroup
			legend="Original language"
			options={langOpts}
			selected={draft.languages}
			ontoggle={(v) => onchange(toggleIn(draft, 'languages', v))}
		/>

		<details class="facet" open={draft.yearFrom != null || draft.yearTo != null}>
			<summary>Year<Icon name="chevron-down" size={14} /></summary>
			<fieldset class="section nums">
				<legend class="sr-only">Year</legend>
				<div class="num-row">
					{@render numberField('yearFrom', 'From', {
						min: YEAR_MIN,
						max: YEAR_MAX,
						placeholder: String(YEAR_MIN),
					})}
					<span class="dash" aria-hidden="true">-</span>
					{@render numberField('yearTo', 'To', {
						min: YEAR_MIN,
						max: YEAR_MAX,
						placeholder: 'Now',
					})}
				</div>
			</fieldset>
		</details>

		<details
			class="facet"
			open={draft.minChapters != null || draft.minRating != null || draft.minFollows != null}
		>
			<summary>Minimums<Icon name="chevron-down" size={14} /></summary>
			<fieldset class="section nums">
				<legend class="sr-only">Minimums</legend>
				<div class="num-grid">
					{@render numberField('minChapters', 'Chapters', {
						min: 0,
						max: MAX_MIN_CHAPTERS,
						placeholder: 'Any',
					})}
					{@render numberField('minRating', 'Rating (0-10)', {
						min: 0,
						max: 10,
						step: 0.5,
						placeholder: 'Any',
						decimal: true,
					})}
					{@render numberField('minFollows', 'Follows', {
						min: 0,
						max: MAX_MIN_FOLLOWS,
						step: 100,
						placeholder: 'Any',
					})}
				</div>
				<p class="ghint">Rating and follows are MangaDex's figures.</p>
			</fieldset>
		</details>

		<!-- A switch, not a two-state chip pair: the wire flag has three values and only
		     two are worth offering. Off is the whole catalogue — including the many
		     popular series whose chapters were pulled upstream after licensing — and on is
		     what can be opened right now. `false` ("only series with NO chapters") is a
		     debugging view, not a reader's request, so it has no control. -->
		<div class="section switch-row">
			<span class="switch-text">
				<span class="switch-label" id="{uid}-hc">Has readable chapters</span>
				<span class="sr-only" id="{uid}-hc-d">Only series you can start reading right now.</span>
			</span>
			<button
				type="button"
				class="switch"
				role="switch"
				aria-checked={draft.hasChapters}
				aria-labelledby="{uid}-hc"
				aria-describedby="{uid}-hc-d"
				onclick={() => onchange(withField(draft, 'hasChapters', !draft.hasChapters))}
			>
				<span class="knob"></span>
			</button>
		</div>
	</div>

	<footer class="panel-foot">
		{#if changes > 0}
			<p class="pending" id="{uid}-pending" role="status">
				{changes}
				{changes === 1 ? 'change' : 'changes'} ready to apply
			</p>
		{/if}
		<button
			type="button"
			class="apply"
			aria-describedby={changes > 0 ? `${uid}-pending` : undefined}
			onclick={onapply}>Apply filters</button
		>
	</footer>
</aside>

<style>
	/* One open rail on desktop, one solid modal sheet on smaller screens. */
	.panel {
		display: flex;
		flex-direction: column;
		min-width: 0;
		position: sticky;
		top: calc(var(--k-header-h) + 20px);
		/* Reserve the browse heading/search row and gutter so Apply is visible on
		   the first screen as well as after this rail becomes sticky. */
		max-height: calc(100dvh - var(--k-header-h) - 140px);
		padding-right: 24px;
		border-right: 1px solid var(--k-border-1);
		background: transparent;
	}
	.panel-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 0 0 20px;
	}
	.panel-title {
		margin: 0;
		font-family: var(--k-font-display);
		font-size: 18px;
		font-weight: 700;
		color: var(--k-text);
	}
	.head-actions {
		display: flex;
		align-items: center;
		gap: 12px;
	}
	.reset,
	.close {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-height: 32px;
		padding: 0;
		border: none;
		background: transparent;
		color: var(--k-text-dim);
		font-size: 12px;
		cursor: pointer;
	}
	.reset:hover:not(:disabled),
	.close:hover {
		color: var(--k-text);
	}
	.reset:disabled {
		color: var(--k-text-disabled);
		cursor: default;
	}
	.close {
		display: none;
		width: 40px;
		height: 40px;
	}
	.panel-body {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		overscroll-behavior: contain;
		scrollbar-width: thin;
		display: flex;
		flex-direction: column;
		padding: 0 3px 0 0;
	}
	.section {
		min-width: 0;
		margin: 0;
		padding: 0 0 20px;
		border: none;
	}
	.glabel {
		margin: 0 0 12px;
		font-size: 13px;
		font-weight: 650;
		letter-spacing: 0;
		color: var(--k-text-1);
	}
	.facet {
		border-top: 1px solid var(--k-border-1);
	}
	.facet summary {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		min-height: 48px;
		list-style: none;
		font-size: 13px;
		font-weight: 650;
		color: var(--k-text-1);
		cursor: pointer;
	}
	.facet summary::-webkit-details-marker {
		display: none;
	}
	.facet summary :global(svg) {
		color: var(--k-text-dim);
	}
	.facet[open] summary :global(svg) {
		transform: rotate(180deg);
	}
	.facet summary:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
		border-radius: var(--k-radius-sm);
	}
	.ghint {
		margin: 10px 0 0;
		font-size: 11.5px;
		line-height: 1.55;
		color: var(--k-text-dim);
	}
	.num-row {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr);
		align-items: end;
		gap: 8px;
	}
	.dash {
		padding-bottom: 10px;
		color: var(--k-text-dim);
	}
	.num-grid {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 12px;
	}
	.num {
		display: flex;
		flex-direction: column;
		gap: 6px;
		min-width: 0;
	}
	.num-label {
		font-size: 12px;
		color: var(--k-text-dim);
	}
	.num input {
		width: 100%;
		min-width: 0;
		height: 40px;
		padding: 0 10px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		color: var(--k-text);
		font-size: 13px;
	}
	.num input::placeholder {
		color: var(--k-text-dim);
		opacity: 1;
	}
	.num input:focus {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
		border-color: var(--k-border-strong);
	}
	.switch-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		min-height: 60px;
		padding: 12px 0;
		border-top: 1px solid var(--k-border-1);
	}
	.switch-label {
		font-size: 12.5px;
		font-weight: 600;
		color: var(--k-text-2);
	}
	.switch {
		position: relative;
		flex-shrink: 0;
		width: 36px;
		height: 22px;
		border-radius: 11px;
		border: 1px solid var(--k-border-3);
		background: var(--k-surface-5);
		cursor: pointer;
		transition: background-color 0.15s;
	}
	.knob {
		position: absolute;
		top: 2px;
		left: 2px;
		width: 16px;
		height: 16px;
		border-radius: 50%;
		background: var(--k-text-dim);
		transition: transform 0.15s;
	}
	.switch[aria-checked='true'] {
		background: var(--k-primary);
		border-color: var(--k-primary);
	}
	.switch[aria-checked='true'] .knob {
		transform: translateX(14px);
		background: var(--k-on-primary);
	}
	.panel-foot {
		flex-shrink: 0;
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding-top: 16px;
		border-top: 1px solid var(--k-border-1);
	}
	.pending {
		margin: 0;
		font-size: 11.5px;
		color: var(--k-text-dim);
	}
	.apply {
		width: 100%;
		min-height: 44px;
		padding: 10px 16px;
		border-radius: var(--k-radius);
		border: none;
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-size: 13px;
		font-weight: 700;
		white-space: nowrap;
		cursor: pointer;
	}
	.apply:hover {
		background: var(--k-primary-hover);
	}
	.apply:active {
		transform: scale(0.98);
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		padding: 0;
		margin: -1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}
	.scrim {
		display: none;
	}
	@media (max-width: 959px) {
		/* Existing overlay layer scale: backdrop 59, dialog 60. */
		.panel {
			position: fixed;
			top: max(12px, env(safe-area-inset-top, 0px));
			left: 0;
			right: 0;
			bottom: 0;
			z-index: 60;
			max-width: 640px;
			max-height: none;
			margin-inline: auto;
			padding: 0 20px;
			border: 1px solid var(--k-border-2);
			border-bottom: none;
			border-radius: var(--k-radius-lg) var(--k-radius-lg) 0 0;
			background: var(--k-surface-2);
			transform: translateY(102%);
			visibility: hidden;
			transition:
				transform 0.2s ease,
				visibility 0s linear 0.2s;
		}
		.panel.open {
			transform: translateY(0);
			visibility: visible;
			transition:
				transform 0.2s ease,
				visibility 0s;
		}
		.panel-head {
			padding: 16px 0 12px;
		}
		.reset {
			min-height: 40px;
		}
		.close {
			display: inline-flex;
		}
		.panel-foot {
			padding-bottom: calc(16px + env(safe-area-inset-bottom, 0px));
		}
		.num input {
			font-size: 16px;
		}
		.scrim {
			display: block;
			position: fixed;
			inset: 0;
			z-index: 59;
			border: none;
			background: rgba(8, 8, 9, 0.55);
		}
	}
</style>
