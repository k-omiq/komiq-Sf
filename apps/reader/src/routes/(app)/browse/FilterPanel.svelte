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
	import { FLAG, type ComicType } from '$lib/data/types';
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
		prefix: FLAG[TYPE_LABEL[v] as ComicType],
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
		<button type="button" class="close" aria-label="Close filters" onclick={onclose}>
			<Icon name="x" size={20} />
		</button>
	</header>

	<div class="panel-body">
		<section class="section" aria-labelledby="{uid}-tags">
			<h3 class="glabel" id="{uid}-tags">Tags</h3>
			<TagPicker {tags} source={tagSource} loading={tagsLoading} {draft} {onchange} />
		</section>

		<ChipGroup
			legend="Status"
			options={statusOpts}
			selected={draft.status}
			ontoggle={(v) => onchange(toggleIn(draft, 'status', v))}
		/>
		<ChipGroup
			legend="Format"
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

		<fieldset class="section nums">
			<legend class="glabel">Year</legend>
			<div class="num-row">
				{@render numberField('yearFrom', 'From', {
					min: YEAR_MIN,
					max: YEAR_MAX,
					placeholder: String(YEAR_MIN),
				})}
				<span class="dash" aria-hidden="true">–</span>
				{@render numberField('yearTo', 'To', {
					min: YEAR_MIN,
					max: YEAR_MAX,
					placeholder: 'Now',
				})}
			</div>
		</fieldset>

		<fieldset class="section nums">
			<legend class="glabel">Minimums</legend>
			<div class="num-grid">
				{@render numberField('minChapters', 'Chapters', {
					min: 0,
					max: MAX_MIN_CHAPTERS,
					placeholder: 'Any',
				})}
				{@render numberField('minRating', 'Rating (0–10)', {
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

		<!-- A switch, not a two-state chip pair: the wire flag has three values and only
		     two are worth offering. Off is the whole catalogue — including the many
		     popular series whose chapters were pulled upstream after licensing — and on is
		     what can be opened right now. `false` ("only series with NO chapters") is a
		     debugging view, not a reader's request, so it has no control. -->
		<div class="section switch-row">
			<span class="switch-text">
				<span class="switch-label" id="{uid}-hc">Has readable chapters</span>
				<span class="ghint" id="{uid}-hc-d">Only series you can start reading right now.</span>
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
		<button type="button" class="reset" disabled={!canReset} onclick={onreset}>Reset</button>
		<button type="button" class="apply" class:dirty={changes > 0} onclick={onapply}>
			{changes > 0 ? `Apply ${changes} ${changes === 1 ? 'change' : 'changes'}` : 'Apply'}
		</button>
	</footer>
</aside>

<style>
	.panel {
		display: flex;
		flex-direction: column;
		min-width: 0;
		/* Desktop rail: sticky beside the grid, scrolling inside itself so the Apply bar
		   stays reachable however long the tag list is. */
		position: sticky;
		top: calc(var(--k-header-h) + 16px);
		max-height: calc(100vh - var(--k-header-h) - 32px);
		max-height: calc(100dvh - var(--k-header-h) - 32px);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius-xl);
	}
	.panel-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 16px 18px 12px;
		border-bottom: 1px solid var(--k-border);
	}
	.panel-title {
		margin: 0;
		font-family: var(--k-font-display);
		font-size: 16px;
		font-weight: 700;
		color: var(--k-text);
	}
	.close {
		display: none;
		width: 40px;
		height: 40px;
		margin: -8px -10px -8px 0;
		border: none;
		background: transparent;
		color: var(--k-text-dim);
		cursor: pointer;
		align-items: center;
		justify-content: center;
		border-radius: var(--k-radius-pill);
	}
	.panel-body {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		overscroll-behavior: contain;
		display: flex;
		flex-direction: column;
		gap: 26px;
		padding: 16px 18px 20px;
	}
	.section {
		display: flex;
		flex-direction: column;
		gap: 12px;
		min-width: 0;
		margin: 0;
		padding: 0;
		border: none;
	}
	.glabel {
		margin: 0 0 10px;
		padding: 0;
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.09em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	h3.glabel {
		margin: 0;
	}
	/* Fieldsets lay out as blocks (see ChipGroup for the floated-legend rationale). */
	.nums {
		display: block;
	}
	.nums .glabel {
		float: left;
		width: 100%;
	}
	.nums .num-row,
	.nums .num-grid {
		clear: both;
	}
	.nums .ghint {
		margin-top: 8px;
	}
	.ghint {
		margin: 0;
		font-size: 12px;
		line-height: 1.45;
		color: var(--k-text-faint);
	}
	.num-row {
		display: flex;
		align-items: flex-end;
		gap: 8px;
	}
	.num-row .num {
		flex: 1;
	}
	.dash {
		padding-bottom: 10px;
		color: var(--k-text-faint);
	}
	.num-grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(110px, 1fr));
		gap: 10px;
	}
	.num {
		display: flex;
		flex-direction: column;
		gap: 5px;
		min-width: 0;
	}
	.num-label {
		font-size: 12px;
		font-weight: 600;
		color: var(--k-text-dim);
	}
	.num input {
		width: 100%;
		min-width: 0;
		height: 38px;
		padding: 0 10px;
		border-radius: var(--k-radius-md);
		background: var(--k-surface);
		border: 1px solid var(--k-border-3);
		color: var(--k-text);
		font-size: 13.5px;
		font-family: inherit;
	}
	.num input:focus {
		outline: none;
		border-color: var(--k-border-strong);
	}
	.switch-row {
		flex-direction: row;
		align-items: center;
		justify-content: space-between;
		gap: 14px;
	}
	.switch-text {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}
	.switch-label {
		font-size: 13.5px;
		font-weight: 700;
		color: var(--k-text);
	}
	.switch {
		position: relative;
		flex: 0 0 auto;
		width: 44px;
		height: 26px;
		border-radius: 13px;
		border: 1px solid var(--k-border-3);
		background: var(--k-surface-5);
		cursor: pointer;
		transition: background-color 0.15s;
	}
	.switch .knob {
		position: absolute;
		top: 2px;
		left: 2px;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		background: var(--k-text-dim);
		transition:
			transform 0.15s,
			background-color 0.15s;
	}
	.switch[aria-checked='true'] {
		background: var(--k-primary);
		border-color: var(--k-primary);
	}
	.switch[aria-checked='true'] .knob {
		transform: translateX(18px);
		background: var(--k-on-primary);
	}
	.panel-foot {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 10px;
		padding: 12px 18px;
		border-top: 1px solid var(--k-border);
	}
	.reset {
		height: 40px;
		padding: 0 14px;
		border: none;
		border-radius: var(--k-radius-pill);
		background: transparent;
		color: var(--k-text-dim);
		font-size: 13px;
		font-weight: 700;
		cursor: pointer;
	}
	.reset:hover:not(:disabled) {
		color: var(--k-text);
	}
	.reset:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.apply {
		height: 40px;
		padding: 0 20px;
		border-radius: var(--k-radius-pill);
		border: 1px solid var(--k-border-3);
		background: var(--k-surface-4);
		color: var(--k-text);
		font-size: 13.5px;
		font-weight: 700;
		cursor: pointer;
		transition:
			background-color 0.15s,
			color 0.15s;
	}
	.apply.dirty {
		background: var(--k-primary);
		border-color: var(--k-primary);
		color: var(--k-on-primary);
	}
	.scrim {
		display: none;
	}

	@media (max-width: 959px) {
		/* Sheet. Full height on phones (with room for the status bar), centred and capped
		   on tablets. Hidden by transform + visibility rather than display so it can
		   animate; `mobileModalFocus` also marks it inert while closed. */
		.panel {
			position: fixed;
			top: max(16px, env(safe-area-inset-top, 0px));
			left: 0;
			right: 0;
			bottom: 0;
			z-index: 60;
			max-width: 720px;
			max-height: none;
			margin-inline: auto;
			border-radius: 18px 18px 0 0;
			border-bottom: none;
			background: var(--k-surface-3);
			box-shadow: 0 -20px 60px rgba(0, 0, 0, 0.45);
			transform: translateY(102%);
			visibility: hidden;
			transition:
				transform 0.28s cubic-bezier(0.4, 0, 0.2, 1),
				visibility 0s linear 0.28s;
		}
		.panel.open {
			transform: translateY(0);
			visibility: visible;
			transition:
				transform 0.28s cubic-bezier(0.4, 0, 0.2, 1),
				visibility 0s;
		}
		.close {
			display: inline-flex;
		}
		.panel-head,
		.panel-body,
		.panel-foot {
			padding-left: max(18px, env(safe-area-inset-left, 0px));
			padding-right: max(18px, env(safe-area-inset-right, 0px));
		}
		.panel-foot {
			padding-bottom: calc(12px + env(safe-area-inset-bottom, 0px));
		}
		.apply {
			flex: 1;
			max-width: 320px;
		}
		.scrim {
			display: block;
			position: fixed;
			inset: 0;
			z-index: 59;
			border: none;
			background: rgba(8, 8, 9, 0.55);
			backdrop-filter: blur(2px);
		}
	}
</style>
