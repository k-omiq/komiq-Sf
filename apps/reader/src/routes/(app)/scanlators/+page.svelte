<script lang="ts">
	// Scanlators: one list of everyone who translates what this server carries, each with
	// the rating its readers gave. A row opens in place: the reader rates there, sees how
	// the votes fall, and reads or joins the thread, without leaving the list. That keeps
	// "rate the five I read" a matter of seconds, on a phone as on a desktop.
	//
	// The list, the average and the rank are the server's (`scanlatorRatings`). The page
	// keeps three things of its own: the row that is open, the text in the search box,
	// and the viewer's rating for the moment between a tap and the server's answer.
	import { afterNavigate, goto, invalidate, replaceState } from '$app/navigation';
	import { page as pageState } from '$app/state';
	import { tick, untrack } from 'svelte';
	import type { RatedScanlator, ScanlatorSort } from '@komika/api';
	import { auth } from '$lib/auth.svelte';
	import AiBadge from '$lib/components/AiBadge.svelte';
	import Avatar from '$lib/components/Avatar.svelte';
	import CommentThread from '$lib/components/CommentThread.svelte';
	import Dropdown, { type DropdownOption } from '$lib/components/Dropdown.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import Ratings from '$lib/components/Ratings.svelte';
	import Stars from '$lib/components/Stars.svelte';
	import { debounce } from '$lib/data/browse-filters';
	import { lastPage } from '$lib/data/paging';
	import {
		MIN_RATINGS_TO_RANK,
		SCANLATOR_PAGE_SIZE,
		SHOW_LABEL,
		SORT_LABEL,
		countLabel,
		kindLabel,
		scanlatorsHref,
		siteSourceId,
		withMyScore,
		type ScanlatorShow,
		type ScanlatorState,
	} from '$lib/data/scanlators';
	import { rateScanlator, type ScanlatorList } from '$lib/data/scanlators-repo';
	import { socialLive } from '$lib/data/social-repo';

	let { data } = $props();

	const applied = $derived<ScanlatorState>(data.state);

	// ---- the list ---------------------------------------------------------------------
	// `data.list` is a pending promise on every load. The page it replaces STAYS on
	// screen, dimmed, until the new one lands: the search box asks again on every pause
	// in typing, and thirty rows turning into skeletons and back each time would be a
	// flicker, not a loading state. Only the first load has nothing to keep and shows
	// skeletons. The teardown cancels a superseded request, so the answer that lands is
	// the one for the address on screen, not the slowest one.
	let list = $state<ScanlatorList | undefined>(undefined);
	let loading = $state(true);
	$effect(() => {
		const pending = data.list;
		loading = true;
		let alive = true;
		void pending.then((r) => {
			if (!alive) return;
			list = r;
			loading = false;
			// The server's rows are the truth again.
			patched = {};
			confirmed.clear();
		});
		return () => {
			alive = false;
		};
	});

	// A row the address names (`?s=`) that this page of the list may not hold.
	let linked = $state<RatedScanlator | null>(null);
	$effect(() => {
		const pending = data.linked;
		linked = null;
		if (!pending) return;
		let alive = true;
		void pending.then((r) => {
			if (alive) linked = r;
		});
		return () => {
			alive = false;
		};
	});

	// The viewer's rating between a tap and the server's answer, by scanlator id; then
	// the server's own row. `confirmed` is the last row the server sent for a scanlator,
	// which is what a failed write goes back to.
	let patched = $state<Record<string, RatedScanlator>>({});
	const confirmed = new Map<string, RatedScanlator>();

	const rows = $derived((list?.items ?? []).map((s) => patched[s.id] ?? s));
	// Shown above the list, once, when the list does not already hold it.
	const pinned = $derived(
		linked && !rows.some((s) => s.id === linked!.id) ? (patched[linked.id] ?? linked) : null,
	);

	// A shared `?page=9` of a list that has since shrunk: go to the last page that has
	// rows, in place, instead of showing an empty one.
	$effect(() => {
		if (!list || list.status !== 'ok' || list.items.length || list.page <= 1 || !list.total) return;
		const last = lastPage(list.total, SCANLATOR_PAGE_SIZE);
		if (last < applied.page)
			void goto(scanlatorsHref({ ...applied, page: last }), { replaceState: true });
	});

	// Signing in or out changes every row's `myScore`.
	let viewer = untrack(() => auth.user?.id);
	$effect(() => {
		const id = auth.user?.id;
		if (id === viewer) return;
		viewer = id;
		void invalidate('komika:scanlators');
	});

	// ---- the open row -----------------------------------------------------------------
	// One row is open at a time. It is in the address (`?s=`) so a link can point at one
	// scanlator, and it is written with a SHALLOW replace: opening a row is not a step in
	// history and must not ask the server for the list again. A real navigation (a chip,
	// the pager, Back, a shared link) sets it from the address it lands on.
	let openId = $state<string | null>(null);
	afterNavigate(({ to }) => {
		openId = to?.url.searchParams.get('s') ?? null;
	});

	const needsAuth = $derived(socialLive() && !auth.user);
	const loginHref = $derived(
		`/login?redirect=${encodeURIComponent(scanlatorsHref(applied, openId))}`,
	);

	async function toggle(id: string, row: HTMLElement): Promise<void> {
		openId = openId === id ? null : id;
		rateError = null;
		replaceState(scanlatorsHref(applied, openId), pageState.state);
		// Closing the row above this one moves it; keep it in view.
		await tick();
		row.scrollIntoView({ block: 'nearest' });
	}

	// ---- rating -----------------------------------------------------------------------
	// Ten stars in a row: tapping 7 then 9 is one gesture. Sent as two requests at once,
	// the server could finish them in either order and keep the 7. So one write runs at
	// a time for a scanlator, and a tap during it only records the score that is wanted;
	// the running write sends that next. The star moves at once either way.
	const writing = new Set<string>();
	const wanted = new Map<string, number | null>();
	let rateError = $state<string | null>(null);

	async function rate(s: RatedScanlator, score: number | null): Promise<void> {
		if (needsAuth || (score ?? 0) === (s.myScore ?? 0)) return;
		rateError = null;
		if (!confirmed.has(s.id)) confirmed.set(s.id, s);
		patched[s.id] = withMyScore(s, score);
		wanted.set(s.id, score);
		if (writing.has(s.id)) return;
		writing.add(s.id);
		try {
			while (wanted.has(s.id)) {
				const next = wanted.get(s.id) as number | null;
				wanted.delete(s.id);
				const fresh = await rateScanlator(s.id, next);
				confirmed.set(s.id, fresh);
				// A newer tap is waiting: its star stays on screen until it is sent.
				if (!wanted.has(s.id)) patched[s.id] = fresh;
			}
		} catch (err) {
			wanted.delete(s.id);
			patched[s.id] = confirmed.get(s.id) ?? s;
			rateError = err instanceof Error ? err.message : 'Your rating was not saved.';
		} finally {
			writing.delete(s.id);
		}
	}

	// ---- search, show, sort -----------------------------------------------------------
	const options = { keepFocus: true, noScroll: true };
	let qInput = $state(untrack(() => data.state.q));
	const writeQuery = debounce(
		(q: string) =>
			void goto(scanlatorsHref({ ...applied, q, page: 1 }), { ...options, replaceState: true }),
		320,
	);
	// Address to box, for a navigation that was not our own typing (Back, a link). Our
	// own write reads back equal. `qInput` is read untracked: this runs on address
	// changes only, never on the box's own edits.
	$effect(() => {
		const q = applied.q;
		if (writeQuery.pending) return;
		if (untrack(() => qInput).trim() !== q) qInput = q;
	});
	function clearSearch(): void {
		qInput = '';
		writeQuery.cancel();
		void goto(scanlatorsHref({ ...applied, q: '', page: 1 }), options);
	}

	const SHOWS: ScanlatorShow[] = ['all', 'site', 'group', 'mine'];
	const SORTS: DropdownOption<ScanlatorSort>[] = (
		['TOP', 'VOTES', 'CHAPTERS', 'NAME'] as const
	).map((value) => ({ value, label: SORT_LABEL[value] }));
	function sortBy(sort: ScanlatorSort): void {
		void goto(scanlatorsHref({ ...applied, sort, page: 1 }), options);
	}

	/** How far a full-width bar is pushed out of its track: the score most readers gave
	 *  fills the track, the others in proportion. A shift, not a width, so a new vote
	 *  moves the bar without a layout pass. */
	function barShift(n: number, distribution: number[]): string {
		const max = Math.max(1, ...distribution);
		return `${(n / max) * 100 - 100}% 0`;
	}
</script>

<svelte:head>
	<title>Scanlators · komiq</title>
</svelte:head>

{#snippet row(s: RatedScanlator)}
	{@const open = openId === s.id}
	<article class="item" class:open>
		<h2 class="row-h">
			<button class="row" aria-expanded={open} onclick={(e) => void toggle(s.id, e.currentTarget)}>
				<span class="rank">
					<span class="vh">{s.rank ? 'Rank' : 'Not ranked'}</span>{s.rank ?? '–'}
				</span>
				<span class="av" aria-hidden="true"><Avatar name={s.name} colorKey={s.id} size={40} /></span
				>
				<span class="who">
					<span class="name">
						<span class="name-t">{s.name}</span>
						<AiBadge group={s.kind === 'GROUP' ? s.name : null} sourceId={siteSourceId(s)} />
					</span>
					<span class="meta">
						{kindLabel(s)} · {countLabel(
							s.seriesCount,
							'series',
							'series',
						)}{#if s.chapterCount != null}<span class="wide"
								>{` · ${countLabel(s.chapterCount, 'chapter', 'chapters')}`}</span
							>{/if}
					</span>
				</span>
				<span class="score">
					{#if s.rank != null}
						<span class="score-n"><Ratings own={s.rating} starSize={14} /></span>
						<span class="score-v">{countLabel(s.ratingCount, 'rating', 'ratings')}</span>
					{:else}
						<span class="score-n none">
							{s.ratingCount ? countLabel(s.ratingCount, 'rating', 'ratings') : 'No ratings'}
						</span>
						<span class="score-v">Needs {MIN_RATINGS_TO_RANK} to rank</span>
					{/if}
					{#if s.myScore}<span class="you">You gave {s.myScore}</span>{/if}
				</span>
				<span class="chev" aria-hidden="true"><Icon name="chevron-down" size={16} /></span>
			</button>
		</h2>

		{#if open}
			<div class="panel">
				<section class="rate" aria-label={`Rate ${s.name}`}>
					<h3 class="label">Your rating</h3>
					{#if needsAuth}
						<p class="hint">
							<a class="text-link k-hit" href={loginHref}>Sign in</a> to rate this scanlator.
						</p>
					{:else}
						<Stars
							value={s.myScore ?? 0}
							max={10}
							size={22}
							gap={2}
							onchange={(v) => void rate(s, v)}
						/>
						<p class="hint">
							{#if s.myScore}
								You gave <b>{s.myScore} of 10</b>.
								<button class="text-link k-hit" onclick={() => void rate(s, null)}>Remove</button>
							{:else}
								Pick a star. You can change it later.
							{/if}
						</p>
						{#if rateError}<p class="error" role="alert">{rateError}</p>{/if}
					{/if}

					<h3 class="label votes">
						{s.ratingCount ? countLabel(s.ratingCount, 'rating', 'ratings') : 'No ratings yet'}
					</h3>
					<ol class="bars">
						{#each [...s.distribution.entries()].reverse() as [i, n] (i)}
							<li class="bar" class:mine={s.myScore === i + 1}>
								<span class="bar-k">{i + 1}</span>
								<span class="track"
									><span class="fill" style:translate={barShift(n, s.distribution)}></span></span
								>
								<span class="bar-n"
									>{n.toLocaleString('en-US')}{#if s.myScore === i + 1}<span class="vh"
											>, with yours</span
										>{/if}</span
								>
							</li>
						{/each}
					</ol>
					<p class="note">
						{#if s.kind === 'SITE'}
							A site translates its own series. This rating is for every chapter that comes from it.
						{:else}
							A group posts on MangaDex. A chapter by two groups counts for each group.
						{/if}
					</p>
				</section>
				<section class="talk">
					<CommentThread
						targetType="scanlator"
						targetId={s.id}
						storageKey={s.id}
						prompt="How are their translations?"
					/>
				</section>
			</div>
		{/if}
	</article>
{/snippet}

<div class="page k-gutter">
	<h1 class="page-title">Scanlators</h1>
	<p class="lede">
		Rate the people who translate what you read. A scanlator gets a rank when
		{MIN_RATINGS_TO_RANK} readers have rated it.
	</p>

	{#if list?.status === 'unsupported'}
		<div class="empty">
			<div class="empty-icon"><Icon name="users" size={22} /></div>
			<div class="empty-title">No scanlator ratings here</div>
			<div class="empty-desc">This server does not have scanlator ratings yet.</div>
		</div>
	{:else}
		<div class="controls">
			<form
				class="search"
				role="search"
				onsubmit={(e) => {
					e.preventDefault();
					writeQuery.flush();
				}}
			>
				<Icon name="search" size={18} />
				<input
					type="search"
					name="q"
					autocomplete="off"
					enterkeyhint="search"
					aria-label="Find a scanlator"
					placeholder="Find a scanlator"
					bind:value={qInput}
					oninput={() => writeQuery(qInput)}
				/>
				{#if qInput}
					<button type="button" class="clear k-hit" aria-label="Clear search" onclick={clearSearch}>
						<Icon name="x" size={16} />
					</button>
				{/if}
			</form>
			<nav class="shows" aria-label="Show">
				{#each SHOWS as show (show)}
					<a
						class="show k-hit"
						class:on={applied.show === show}
						aria-current={applied.show === show ? 'true' : undefined}
						href={scanlatorsHref({ ...applied, show, page: 1 })}
						data-sveltekit-noscroll
						data-sveltekit-keepfocus
					>
						{#if show === 'group'}
							<span class="wide">{SHOW_LABEL[show]}</span><span class="narrow">Groups</span>
						{:else}
							{SHOW_LABEL[show]}
						{/if}
					</a>
				{/each}
			</nav>
			<div class="sort">
				<Dropdown
					value={applied.sort}
					options={SORTS}
					onchange={sortBy}
					label="Sort by"
					icon="arrow-up-down"
					align="right"
				/>
			</div>
		</div>

		<div class="list" id="scanlator-list" class:stale={loading && !!list} aria-busy={loading}>
			{#if !list}
				{#each Array(8) as _, i (i)}
					<div class="k-skeleton row-sk"></div>
				{/each}
			{:else if list.status === 'error'}
				<div class="empty">
					<div class="empty-icon"><Icon name="alert" size={22} /></div>
					<div class="empty-title">The scanlators did not load</div>
					<div class="empty-desc">Check your connection, then try again.</div>
					<button class="empty-btn k-hit" onclick={() => void invalidate('komika:scanlators')}>
						Try again
					</button>
				</div>
			{:else if applied.show === 'mine' && needsAuth}
				<div class="empty">
					<div class="empty-icon"><Icon name="star" size={22} /></div>
					<div class="empty-title">Sign in to see your ratings</div>
					<div class="empty-desc">Your ratings are kept with your account.</div>
					<a class="empty-btn k-hit" href={loginHref}>Sign in</a>
				</div>
			{:else if !rows.length && !pinned}
				<div class="empty">
					<div class="empty-icon"><Icon name="users" size={22} /></div>
					{#if applied.q}
						<div class="empty-title">No scanlator has that name</div>
						<div class="empty-desc">Try a shorter name, or part of it.</div>
						<button class="empty-btn k-hit" onclick={clearSearch}>Clear the search</button>
					{:else if applied.show === 'mine'}
						<div class="empty-title">You have not rated a scanlator yet</div>
						<div class="empty-desc">Open a scanlator in the list and pick a star.</div>
						<a class="empty-btn k-hit" href={scanlatorsHref({ ...applied, show: 'all', page: 1 })}>
							Show all scanlators
						</a>
					{:else}
						<div class="empty-title">No scanlators yet</div>
						<div class="empty-desc">This server has no chapters with a scanlator yet.</div>
					{/if}
				</div>
			{:else}
				{#if pinned}{@render row(pinned)}{/if}
				{#each rows as s (s.id)}
					{@render row(s)}
				{/each}
			{/if}
		</div>

		{#if list?.status === 'ok'}
			<Pager
				page={list.page}
				hasNext={list.hasNext}
				total={list.total}
				pageSize={SCANLATOR_PAGE_SIZE}
				count={rows.length}
				{loading}
				href={(p) => scanlatorsHref({ ...applied, page: p })}
				label="Scanlator pages"
				controls="scanlator-list"
				scrollTarget="scanlator-list"
			/>
		{/if}
	{/if}
</div>

<style>
	.page {
		/* A list of names and numbers: narrower than the card grids, so a name and its
		   rating stay in one glance. */
		max-width: 1120px;
		padding-top: 56px;
		padding-bottom: 80px;
	}
	.page-title {
		margin: 0;
		font-size: 34px;
		line-height: 1.15;
		color: var(--k-text-bright);
	}
	.lede {
		margin: 10px 0 0;
		max-width: 60ch;
		font-size: 15px;
		line-height: 1.5;
		color: var(--k-text-dim);
		text-wrap: pretty;
	}

	/* ---- controls ---- */
	.controls {
		display: grid;
		grid-template-columns: minmax(200px, 300px) minmax(0, 1fr) auto;
		grid-template-areas: 'search shows sort';
		align-items: center;
		gap: 12px;
		margin-top: 32px;
	}
	.search {
		grid-area: search;
		display: flex;
		align-items: center;
		gap: 10px;
		height: 42px;
		padding: 0 6px 0 16px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		color: var(--k-text-dim);
	}
	.search:focus-within {
		outline: 2px solid var(--k-focus-ring);
		outline-offset: 2px;
	}
	.search input {
		flex: 1;
		min-width: 0;
		align-self: stretch;
		border: none;
		outline: none;
		background: transparent;
		color: var(--k-text);
		font-size: 14px;
	}
	.search input::placeholder {
		color: var(--k-text-dimmer);
	}
	/* The browser's own clear button would sit beside ours. */
	.search input::-webkit-search-cancel-button {
		display: none;
	}
	.clear {
		flex: 0 0 auto;
		width: 30px;
		height: 30px;
		display: grid;
		place-items: center;
		border: none;
		border-radius: 50%;
		background: transparent;
		color: var(--k-text-dim);
		cursor: pointer;
	}
	.clear:hover {
		color: var(--k-text);
		background: var(--k-hover-fill);
	}
	.shows {
		grid-area: shows;
		display: flex;
		gap: 4px;
		min-width: 0;
		overflow-x: auto;
		scrollbar-width: none;
		/* Room for the focus ring, which `overflow` would cut. */
		padding: 4px;
		margin: -4px;
	}
	.shows::-webkit-scrollbar {
		display: none;
	}
	.show {
		flex: 0 0 auto;
		display: inline-flex;
		align-items: center;
		height: 36px;
		padding: 0 14px;
		border-radius: var(--k-radius-pill);
		color: var(--k-text-dim);
		font-size: 13px;
		font-weight: 700;
		text-decoration: none;
		white-space: nowrap;
		transition:
			color var(--k-dur),
			background-color var(--k-dur);
	}
	.show:hover {
		color: var(--k-text);
		background: var(--k-hover-fill);
	}
	.show.on {
		color: var(--k-on-primary);
		background: var(--k-primary);
	}
	.sort {
		grid-area: sort;
	}
	/* The shared trigger is outlined and says hover and open with the outline's colour.
	   This page has no outlines, so here the fill says both. */
	.sort :global(.dd-trigger) {
		border-color: transparent;
	}
	.sort :global(.dd-trigger:hover:not(:disabled)),
	.sort :global(.dd.open .dd-trigger) {
		border-color: transparent;
		background: var(--k-surface-5);
	}

	/* ---- list ---- */
	.list {
		display: flex;
		flex-direction: column;
		gap: 2px;
		margin-top: 20px;
		scroll-margin-top: calc(var(--k-header-h) + 16px);
		transition: opacity var(--k-dur);
	}
	.list.stale {
		opacity: 0.55;
	}
	.row-sk {
		height: 66px;
		border-radius: var(--k-radius-xl);
	}
	.item {
		border-radius: var(--k-radius-xl);
		transition: background-color var(--k-dur);
	}
	.item.open {
		background: var(--k-surface-2);
	}
	/* A heading for the outline only: it must not bring the display face to the row. */
	.row-h {
		margin: 0;
		font: inherit;
		letter-spacing: normal;
	}
	.row {
		width: 100%;
		display: grid;
		grid-template-columns: 28px 40px minmax(0, 1fr) auto 16px;
		align-items: center;
		gap: 14px;
		padding: 13px 16px 13px 12px;
		border: none;
		border-radius: var(--k-radius-xl);
		background: none;
		color: inherit;
		font: inherit;
		text-align: left;
		cursor: pointer;
		scroll-margin-top: calc(var(--k-header-h) + 12px);
		transition: background-color var(--k-dur);
	}
	.row:hover {
		background: var(--k-hover-fill);
	}
	.row:focus-visible {
		outline-offset: -2px;
	}
	.rank {
		text-align: right;
		font-size: 13px;
		font-weight: 700;
		font-variant-numeric: tabular-nums;
		color: var(--k-text-dim);
	}
	.av {
		display: flex;
	}
	.who {
		min-width: 0;
	}
	.name {
		display: flex;
		align-items: center;
		gap: 8px;
		min-width: 0;
		font-size: 15px;
		font-weight: 700;
		line-height: 1.3;
		color: var(--k-text-bright);
	}
	.name-t,
	.meta {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.meta {
		display: block;
		margin-top: 2px;
		font-size: 12.5px;
		color: var(--k-text-dim);
	}
	.score {
		display: grid;
		justify-items: end;
		gap: 1px;
		white-space: nowrap;
	}
	.score-n {
		font-size: 17px;
		font-weight: 800;
		color: var(--k-text-bright);
	}
	/* The shared component sets its own weight and gap; here it is the row's headline. */
	.score-n :global(.ratings) {
		font-weight: 800;
		gap: 5px;
	}
	.score-n.none {
		font-size: 13px;
		font-weight: 700;
		color: var(--k-text-dim);
	}
	.score-v,
	.you {
		font-size: 12px;
		font-variant-numeric: tabular-nums;
		color: var(--k-text-dim);
	}
	.you {
		font-weight: 700;
		color: var(--k-text-2);
	}
	.chev {
		display: flex;
		color: var(--k-text-dim);
		transition: transform var(--k-dur) var(--k-ease);
	}
	.open .chev {
		transform: rotate(180deg);
	}

	/* ---- the open row ---- */
	.panel {
		display: grid;
		grid-template-columns: 300px minmax(0, 1fr);
		gap: 32px 56px;
		/* The left edge of the panel is the left edge of the avatar above it. */
		padding: 10px 24px 28px 54px;
		transition:
			opacity 0.2s var(--k-ease),
			translate 0.2s var(--k-ease);
	}
	@starting-style {
		.panel {
			opacity: 0;
			translate: 0 -6px;
		}
	}
	.label {
		margin: 0 0 10px;
		font-family: var(--k-font-sans);
		font-size: 13px;
		font-weight: 700;
		letter-spacing: 0;
		color: var(--k-text-2);
	}
	.label.votes {
		margin-top: 28px;
	}
	.rate :global(.stars) {
		margin-left: -3px;
	}
	.hint,
	.note,
	.error {
		margin: 10px 0 0;
		font-size: 12.5px;
		line-height: 1.5;
		color: var(--k-text-dim);
	}
	@media (pointer: coarse) {
		.rate .hint {
			margin-top: 22px;
		}
	}
	.hint b {
		font-weight: 700;
		color: var(--k-text);
	}
	.error {
		color: var(--k-danger);
	}
	.note {
		margin-top: 16px;
		max-width: 42ch;
	}
	.text-link {
		padding: 0;
		border: none;
		background: none;
		color: var(--k-text-1);
		font: inherit;
		font-weight: 700;
		text-decoration: underline;
		text-underline-offset: 3px;
		cursor: pointer;
	}
	.text-link:hover {
		color: var(--k-text-bright);
	}
	.bars {
		display: grid;
		gap: 3px;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.bar {
		display: grid;
		grid-template-columns: 16px minmax(0, 1fr) 34px;
		align-items: center;
		gap: 10px;
		font-size: 12px;
		font-variant-numeric: tabular-nums;
		color: var(--k-text-dim);
	}
	.bar-k {
		text-align: right;
	}
	/* The score the viewer gave. */
	.bar.mine {
		font-weight: 800;
		color: var(--k-text-bright);
	}
	.track {
		height: 6px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-5);
		overflow: hidden;
	}
	.fill {
		display: block;
		height: 100%;
		border-radius: var(--k-radius-pill);
		background: var(--k-star);
		transition: translate 0.3s var(--k-ease);
	}
	.talk {
		min-width: 0;
	}

	/* ---- empty and error ---- */
	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 10px;
		padding: 72px 20px;
		text-align: center;
	}
	/* Sizes as on Updates, so an empty list reads the same on both screens. */
	.empty-icon {
		width: 56px;
		height: 56px;
		display: grid;
		place-items: center;
		margin-bottom: 4px;
		border-radius: 50%;
		background: var(--k-surface-4);
		color: var(--k-text-dim);
	}
	.empty-title {
		font-family: var(--k-font-display);
		font-size: 18px;
		font-weight: 700;
		color: var(--k-text);
	}
	.empty-desc {
		max-width: 320px;
		font-size: 14px;
		line-height: 1.5;
		color: var(--k-text-dimmer);
	}
	.empty-btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		height: 42px;
		margin-top: 6px;
		padding: 0 22px;
		border: none;
		border-radius: 8px;
		background: var(--k-primary);
		color: var(--k-on-primary);
		font: inherit;
		font-size: 13.5px;
		font-weight: 700;
		text-decoration: none;
		cursor: pointer;
	}
	.empty-btn:hover {
		background: var(--k-primary-hover);
	}

	/* The short form of a label, for a phone. */
	.narrow {
		display: none;
	}

	/* Read out, not drawn. */
	.vh {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}

	@media (prefers-reduced-motion: reduce) {
		.panel,
		.fill,
		.chev,
		.list {
			transition: none;
		}
	}

	@media (max-width: 960px) {
		.panel {
			grid-template-columns: minmax(0, 1fr);
			padding-right: 20px;
		}
		.rate {
			max-width: 340px;
		}
		.controls {
			grid-template-columns: minmax(0, 1fr) auto;
			grid-template-areas:
				'search sort'
				'shows shows';
		}
	}
	@media (max-width: 720px) {
		.page {
			padding-top: 28px;
		}
		.page-title {
			font-size: 28px;
		}
		.controls {
			margin-top: 24px;
		}
		.row {
			grid-template-columns: 20px 40px minmax(0, 1fr) auto 14px;
			gap: 9px;
			padding: 12px 8px 12px 4px;
		}
		.wide {
			display: none;
		}
		.narrow {
			display: inline;
		}
		/* A long name takes a second line before it is cut. */
		.name-t {
			white-space: normal;
			display: -webkit-box;
			-webkit-line-clamp: 2;
			line-clamp: 2;
			-webkit-box-orient: vertical;
		}
		.score-n {
			font-size: 16px;
		}
		.panel {
			padding: 6px 12px 22px;
		}
	}
</style>
