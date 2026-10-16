<script lang="ts">
	import { href } from '$lib/paths';
	import type { SourceHealthStat, SourceScanHealth } from '@komika/types';
	import type { EngineGeneration as Generation } from '@komika/api';
	import { auth } from '$lib/auth.svelte';
	import { loadEngineGenerations, loadSourceScanHealth, startNewEngineGeneration } from '$lib/data';
	import { engineGenerationView } from '$lib/engine-generation';
	import Button from '$lib/components/Button.svelte';
	import EngineGeneration from '$lib/components/EngineGeneration.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';

	// Auth redirect is centralized in +layout.svelte.

	let engineRows = $state<Generation[] | null>([]);
	let engineLoading = $state(false);
	let engineLoaded = $state(false);
	let engineError = $state<string | null>(null);
	const engineView = $derived(engineGenerationView(engineRows));

	async function refreshEngine(): Promise<void> {
		if (engineLoading) return;
		engineLoading = true;
		engineError = null;
		try {
			engineRows = await loadEngineGenerations();
		} catch (err) {
			engineError = err instanceof Error ? err.message : 'Failed to load source engine state.';
		} finally {
			engineLoaded = true;
			engineLoading = false;
		}
	}

	async function startGeneration(): Promise<void> {
		// Keep the mutation's result even if the follow-up query fails.
		engineRows = await startNewEngineGeneration();
	}

	$effect(() => {
		if (!auth.user || engineLoaded || engineLoading) return;
		void refreshEngine();
	});

	// ---- Scan health ------------------------------------------------------------
	// "Is this source actually working?" used to have no honest answer: every Suwayomi
	// fetch path falls back to reading Suwayomi's own DB when the upstream fetch fails,
	// so a dead source's scans were recorded as successes — 209 series on one source sat
	// at 0 chapters reporting zero failures. `cachedFallback` is that case, now counted.
	let health = $state<SourceScanHealth[]>([]);
	let healthLoading = $state(false);
	let healthLoaded = $state(false);
	let healthError = $state<string | null>(null);

	async function refreshHealth(): Promise<void> {
		healthLoading = true;
		healthError = null;
		try {
			health = await loadSourceScanHealth();
			healthLoaded = true;
		} catch (err) {
			healthError = err instanceof Error ? err.message : 'Failed to load scan health.';
		} finally {
			healthLoaded = true; // A failed attempt waits for explicit retry.
			healthLoading = false;
		}
	}

	$effect(() => {
		if (!auth.user || healthLoaded || healthLoading) return;
		void refreshHealth();
	});

	// ---- Request health (why requests fail) -----------------------------------
	// The scan columns say THAT a source is failing. `activity` says why, and it counts
	// the requests readers make too — a chapter that will not open, a page image the
	// site refuses — which the scanner never sees.
	const SURFACE_LABEL: Record<string, string> = {
		pages: 'Chapter opens',
		image: 'Page images',
		chapters: 'Chapter lists',
		browse: 'Listings',
	};
	/** Reader-facing surfaces first: they are the ones a visitor notices. */
	const SURFACE_ORDER = ['pages', 'image', 'chapters', 'browse'];
	type ReasonTone = 'danger' | 'warning' | 'info' | undefined;
	/** Tone: danger when the site refuses us or the extension must change, warning when
	 * it may clear up on its own, info when the source is not at fault. */
	const REASON: Record<string, { label: string; hint: string; tone: ReasonTone }> = {
		blocked: {
			label: 'Blocked',
			hint: 'The site refuses our requests (403 or a Cloudflare check).',
			tone: 'danger',
		},
		rate_limited: {
			label: 'Rate limited',
			hint: 'The site says we ask too often (429).',
			tone: 'warning',
		},
		solver: {
			label: 'Solver down',
			hint: 'FlareSolverr failed. The site needs it to pass Cloudflare.',
			tone: 'warning',
		},
		not_found: {
			label: 'Not found',
			hint: 'The site answers 404. It moved, or the extension is out of date.',
			tone: 'danger',
		},
		timeout: { label: 'Timeout', hint: 'The site did not answer in time.', tone: 'warning' },
		network: {
			label: 'Unreachable',
			hint: 'The site could not be reached (DNS, connection or TLS).',
			tone: 'danger',
		},
		server_error: {
			label: 'Site error',
			hint: 'The site answers with a 5xx error.',
			tone: 'warning',
		},
		extension: {
			label: 'Extension broken',
			hint: 'The extension could not read the site. It needs an update.',
			tone: 'danger',
		},
		engine: { label: 'Engine', hint: 'Suwayomi itself failed, not the source.', tone: 'info' },
		other: { label: 'Other', hint: 'Not sorted. Read the message.', tone: undefined },
	};

	interface SurfaceSummary {
		surface: string;
		label: string;
		total: number;
		failed: number;
		/** Failure reasons, most frequent first. */
		reasons: SourceHealthStat[];
	}

	function summarize(activity: SourceHealthStat[]): SurfaceSummary[] {
		const bySurface: Record<string, SurfaceSummary> = {};
		for (const a of activity) {
			const s = (bySurface[a.surface] ??= {
				surface: a.surface,
				label: SURFACE_LABEL[a.surface] ?? a.surface,
				total: 0,
				failed: 0,
				reasons: [],
			});
			s.total += a.count;
			if (a.reason !== 'ok') {
				s.failed += a.count;
				s.reasons.push(a);
			}
		}
		const rank = (surface: string): number => {
			const i = SURFACE_ORDER.indexOf(surface);
			return i < 0 ? SURFACE_ORDER.length : i;
		};
		return Object.values(bySurface)
			.map((s) => ({ ...s, reasons: s.reasons.sort((a, b) => b.count - a.count) }))
			.sort((a, b) => rank(a.surface) - rank(b.surface));
	}

	/** Per source: its surfaces, and its totals across them. */
	const requests = $derived(
		new Map(
			health.map((h) => {
				// An older server answers without `activity`; treat that as no requests seen.
				const surfaces = summarize(h.activity ?? []);
				return [
					h.id,
					{
						surfaces,
						failing: surfaces.filter((s) => s.failed > 0),
						total: surfaces.reduce((n, s) => n + s.total, 0),
						failed: surfaces.reduce((n, s) => n + s.failed, 0),
					},
				] as const;
			}),
		),
	);
	/**
	 * The table order. The server sorts by scan failures, which puts a source whose scans
	 * look fine but whose readers get refused near the bottom — so within "no confirmed
	 * scan failure", the most failed requests come first. The sort is stable, so the
	 * server's order still decides ties.
	 */
	const healthRows = $derived(
		[...health].sort(
			(a, b) =>
				Number(b.confirmedFailing > 0) - Number(a.confirmedFailing > 0) ||
				(requests.get(b.id)?.failed ?? 0) - (requests.get(a.id)?.failed ?? 0),
		),
	);
	/** Sources with at least one failed request in the window. */
	const sourcesFailing = $derived(
		health.filter((h) => (requests.get(h.id)?.failed ?? 0) > 0).length,
	);

	function percent(part: number, whole: number): string {
		if (whole <= 0) return 'None';
		const p = (part / whole) * 100;
		return p > 0 && p < 1 ? '<1%' : `${Math.round(p)}%`;
	}

	/** Sources with an open whole-source outage — the loud ones, surfaced above the table. */
	const outages = $derived(health.filter((h) => h.outageDetectedAt));
	/** Works that go dark while those outages last. */
	const outageWorks = $derived(outages.reduce((n, h) => n + h.exclusiveWorks, 0));
	const outageSentence = $derived.by(() => {
		const one = outages.length === 1;
		const head = `${outages.length} source${one ? ' is' : 's are'} down`;
		const those = one ? 'that source' : 'those sources';
		if (outageWorks === 0) return `${head}, and no work exists only on ${those}.`;
		return (
			`${head}, and ${outageWorks} work${outageWorks === 1 ? ' that exists' : 's that exist'} ` +
			`only on ${those} cannot be read until ${one ? 'it recovers' : 'they recover'}.`
		);
	});

	/** UTC, to the minute. `missing` is what a null date means for that column. */
	function shortDate(iso: string | null, missing = 'Never'): string {
		if (!iso) return missing;
		const d = new Date(iso);
		return Number.isNaN(d.getTime()) ? 'Unknown' : d.toISOString().slice(0, 16).replace('T', ' ');
	}

	/** One word for the source as a whole: an open outage beats any failure beats none. */
	function statusOf(
		h: SourceScanHealth,
		failedRequests: number,
	): { label: string; tone: 'danger' | 'warning' | 'success' } {
		if (h.outageDetectedAt) return { label: 'Down', tone: 'danger' };
		if (h.failing > 0 || failedRequests > 0) return { label: 'Failing', tone: 'warning' };
		return { label: 'OK', tone: 'success' };
	}
</script>

{#snippet engineSection()}
	<EngineGeneration
		view={engineView}
		loading={engineLoading || !engineLoaded}
		error={engineError}
		onrefresh={refreshEngine}
		onstart={startGeneration}
	/>
{/snippet}

<div class="k-page">
	<PageHeader
		title="Health"
		description="Which sources fail when the server checks them for new chapters, and why."
	>
		{#snippet actions()}
			<Button icon="refresh" busy={healthLoading} onclick={() => refreshHealth()}>Refresh</Button>
		{/snippet}
		A failing source is often one extension release behind.
		<a class="k-link" href={href('/settings#updates')}>Check extension updates in Settings</a>
	</PageHeader>

	{#if engineView.kind === 'quarantined'}
		{@render engineSection()}
	{/if}

	<LoadState
		loading={(healthLoading || !healthLoaded) && health.length === 0}
		error={health.length === 0 ? healthError : null}
		empty={health.length === 0}
		loadingLabel="Loading scan health…"
		emptyTitle="No source health yet"
		emptyHint="No source has tracked series or recorded requests yet, so there is nothing to report."
		emptyIcon="activity"
		onretry={() => refreshHealth()}
	>
		{#if healthError}
			<!-- A failed refresh keeps the last good numbers on screen. -->
			<div class="k-notice" data-tone="danger" role="alert">
				<Icon name="alert" />
				<div class="k-notice__body">
					{healthError} The numbers below are from the last successful load.
				</div>
			</div>
		{/if}
		{#if outages.length > 0}
			<div class="k-notice" data-tone="danger" role="alert">
				<Icon name="alert" />
				<p class="k-notice__body">{outageSentence}</p>
			</div>
		{/if}

		<div class="k-stack">
			<p class="summary k-muted">
				<span class="k-num">{health.length}</span>
				source{health.length === 1 ? '' : 's'}, worst first.
				{#if sourcesFailing > 0}
					<span class="k-num">{sourcesFailing}</span> had failed requests in the last 24 hours.
				{:else}
					No failed requests in the last 24 hours.
				{/if}
				Times are UTC.
			</p>

			<details class="explain">
				<summary>What the columns mean</summary>
				<dl class="legend">
					<div>
						<dt>Failing</dt>
						<dd>
							Series whose last check failed. Confirmed means enough checks failed in a row to blame
							the source.
						</dd>
					</div>
					<div>
						<dt>Stale data</dt>
						<dd>
							Failing series where the site did not answer and the check got an old stored copy. A
							stored copy never has new chapters, so it counts as a failure.
						</dd>
					</div>
					<div>
						<dt>Errors</dt>
						<dd>Failing series where the request to the site failed outright.</dd>
					</div>
					<div>
						<dt>No chapters</dt>
						<dd>
							Series that have never returned a chapter. When that is most of a source, it has
							probably never worked.
						</dd>
					</div>
					<div>
						<dt>Exclusive works</dt>
						<dd>Works that exist only on this source. Nobody can read them while it is down.</dd>
					</div>
					<div>
						<dt>Updates off</dt>
						<dd>The server does not look for new series on this source.</dd>
					</div>
					<div>
						<dt>Failed requests</dt>
						<dd>
							The lines under a source count requests that failed in the last 24 hours and say why.
							They include what readers ask for (opening a chapter, loading a page image), not only
							the server's own checks.
						</dd>
					</div>
				</dl>
			</details>

			<div
				class="k-table"
				class:is-dim={healthLoading}
				role="table"
				aria-label="Per-source scan health"
				aria-busy={healthLoading}
			>
				<div class="k-tr k-tr--head" role="row">
					<span role="columnheader">Source</span>
					<span role="columnheader">Status</span>
					<span role="columnheader" class="n">Series</span>
					<span role="columnheader" class="n">Failing</span>
					<span role="columnheader" class="n">Stale data</span>
					<span role="columnheader" class="n">Errors</span>
					<span role="columnheader" class="n">No chapters</span>
					<span role="columnheader" class="n">Exclusive works</span>
					<span role="columnheader">Last check</span>
					<span role="columnheader">Last failure</span>
				</div>
				{#each healthRows as h (h.id)}
					{@const req = requests.get(h.id)}
					{@const status = statusOf(h, req?.failed ?? 0)}
					{@const failingSurfaces = req?.failing ?? []}
					{@const hasDetail =
						!!h.outageDetectedAt ||
						!h.installed ||
						!!h.subscriptionDisabledAt ||
						failingSurfaces.length > 0}
					<div class="k-tr" class:has-detail={hasDetail} role="row">
						<span role="cell" class="name wide" title={h.name ?? h.pkgName ?? h.id}
							>{h.name ?? h.pkgName ?? h.id}</span
						>
						<span role="cell" class="wide" data-label="Status">
							<span class="badges">
								<span class="k-badge" data-tone={status.tone}>{status.label}</span>
								{#if !h.installed}
									<span class="k-badge" data-tone="warning">Not installed</span>
								{/if}
								{#if h.subscriptionDisabledAt}
									<span class="k-badge" data-tone="warning">Updates paused</span>
								{:else if !h.subscribed}
									<span class="k-badge">Updates off</span>
								{/if}
							</span>
						</span>
						<span role="cell" class="n k-num" class:zero={h.series === 0} data-label="Series">
							{h.series}
						</span>
						<span
							role="cell"
							class="n k-num"
							class:bad={h.confirmedFailing > 0}
							class:zero={h.failing === 0}
							data-label="Failing"
						>
							<span class="failing">
								{h.failing}
								{#if h.confirmedFailing > 0}
									<span class="confirmed">{h.confirmedFailing} confirmed</span>
								{/if}
							</span>
						</span>
						<span
							role="cell"
							class="n k-num"
							class:bad={h.cachedFallback > 0}
							class:zero={h.cachedFallback === 0}
							data-label="Stale data"
						>
							{h.cachedFallback}
						</span>
						<span
							role="cell"
							class="n k-num"
							class:bad={h.fetchError > 0}
							class:zero={h.fetchError === 0}
							data-label="Errors"
						>
							{h.fetchError}
						</span>
						<span
							role="cell"
							class="n k-num"
							class:zero={h.zeroChapterSeries === 0}
							data-label="No chapters"
						>
							{h.zeroChapterSeries}
						</span>
						<span
							role="cell"
							class="n k-num"
							class:zero={h.exclusiveWorks === 0}
							data-label="Exclusive works"
						>
							{h.exclusiveWorks}
						</span>
						<span role="cell" class="wide k-num k-muted" data-label="Last check">
							{shortDate(h.lastScannedAt)}
						</span>
						<span role="cell" class="wide k-num k-muted" data-label="Last failure">
							{shortDate(h.lastFailureAt)}
						</span>
					</div>
					{#if hasDetail}
						<!-- Why this source is in trouble: the open outage, then its failed requests
						     in the last 24 h. Only failing surfaces are listed; a healthy source
						     gets no second line. -->
						<div class="detail" role="row">
							<div role="cell">
								{#if h.outageDetectedAt}
									<p>
										Down since <span class="k-num">{shortDate(h.outageDetectedAt)}</span>, with
										<span class="k-num">{h.confirmedFailing}</span> of
										<span class="k-num">{h.series}</span> series failing.
										{#if h.outageKind === 'cached_fallback'}
											The site does not answer, so checks only get old stored data.
										{:else if h.outageKind === 'fetch_error'}
											Requests to the site fail.
										{/if}
										{#if h.outageParkedUntil}
											Its series are not checked again until
											<span class="k-num">{shortDate(h.outageParkedUntil)}</span>.
										{/if}
									</p>
								{/if}
								{#if !h.installed}
									<p>The extension for this source is no longer installed.</p>
								{/if}
								{#if h.subscriptionDisabledAt}
									<p>
										The server stopped looking for new series here on
										<span class="k-num">{shortDate(h.subscriptionDisabledAt)}</span> after repeated failures.
										It tries again later on its own.
									</p>
								{/if}
								{#if failingSurfaces.length > 0}
									<div class="surfaces">
										{#each failingSurfaces as s (s.surface)}
											<div class="surface">
												<p class="surface-head">
													<strong>{s.label}</strong>
													<span class="bad k-num">
														{s.failed} of {s.total} failed ({percent(s.failed, s.total)})
													</span>
												</p>
												<ul class="reasons">
													{#each s.reasons as r (r.reason)}
														<li>
															<span class="k-badge" data-tone={REASON[r.reason]?.tone}>
																{REASON[r.reason]?.label ?? r.reason}
																<span class="k-num">{r.count}</span>
															</span>
															{#if REASON[r.reason]}<span>{REASON[r.reason].hint}</span>{/if}
														</li>
													{/each}
												</ul>
												<p class="latest">
													Latest at <span class="k-num">{shortDate(s.reasons[0].lastAt)}</span
													>{#if s.reasons[0].lastMessage}:
														<span class="k-mono">{s.reasons[0].lastMessage}</span>
													{/if}
												</p>
											</div>
										{/each}
									</div>
								{/if}
							</div>
						</div>
					{/if}
				{/each}
			</div>
		</div>
	</LoadState>

	{#if engineView.kind !== 'quarantined'}
		{@render engineSection()}
	{/if}
</div>

<style>
	.k-table {
		--cols: minmax(0, 2.2fr) minmax(0, 1.3fr) minmax(0, 0.7fr) minmax(0, 1fr) minmax(0, 0.8fr)
			minmax(0, 0.7fr) minmax(0, 0.9fr) minmax(0, 1fr) minmax(0, 1.4fr) minmax(0, 1.4fr);
	}
	/* Two-word headers wrap to a second line in the narrow number columns. */
	.k-tr--head {
		align-items: end;
		padding-top: var(--k-space-2);
		padding-bottom: var(--k-space-2);
		line-height: 1.3;
	}
	.k-tr.has-detail {
		border-bottom: 0;
	}
	.n {
		text-align: right;
	}
	/* Two lines at most: one pathological name must not stretch its row. The
	   full name stays available as the cell's title. */
	.name {
		display: -webkit-box;
		-webkit-box-orient: vertical;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		overflow: hidden;
		color: var(--k-text-bright);
		font-weight: 700;
		overflow-wrap: anywhere;
	}
	.badges {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-1);
	}
	.bad {
		color: var(--k-danger);
	}
	.zero {
		color: var(--k-text-dim);
	}
	.failing {
		display: grid;
	}
	.confirmed {
		font-size: var(--k-text-xs);
	}

	.summary {
		font-size: var(--k-text-sm);
	}
	.explain summary {
		width: fit-content;
		color: var(--k-text-muted);
		font-size: var(--k-text-sm);
		font-weight: 600;
		cursor: pointer;
	}
	.explain summary:hover {
		color: var(--k-text-bright);
	}
	.legend {
		margin-top: var(--k-space-3);
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 240px), 1fr));
		gap: var(--k-space-3) var(--k-space-6);
		font-size: var(--k-text-xs);
		line-height: 1.55;
	}
	.legend dt {
		color: var(--k-text-1);
		font-weight: 700;
	}
	.legend dd {
		color: var(--k-text-dim);
	}

	/* The second line of a source: same row visually, so no rule between the two. */
	.detail {
		display: grid;
		padding: 0 var(--k-space-4) var(--k-space-4);
		border-bottom: 1px solid var(--k-border);
		color: var(--k-text-2);
		font-size: var(--k-text-sm);
		line-height: 1.55;
		overflow-wrap: anywhere;
	}
	.detail:last-child {
		border-bottom: 0;
	}
	.detail > [role='cell'] {
		display: grid;
		gap: var(--k-space-2);
		min-width: 0;
	}
	.surfaces {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 340px), 1fr));
		gap: var(--k-space-3) var(--k-space-6);
		margin-top: var(--k-space-1);
	}
	.surface {
		display: grid;
		align-content: start;
		gap: 6px;
		min-width: 0;
	}
	.surface-head {
		display: flex;
		flex-wrap: wrap;
		gap: 0 var(--k-space-2);
	}
	.surface-head strong {
		color: var(--k-text-1);
	}
	.reasons {
		display: grid;
		gap: 6px;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.reasons li {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: var(--k-space-1) var(--k-space-2);
		color: var(--k-text-dim);
	}
	/* The explanation shares the badge's line and wraps beside it, unless there is no room. */
	.reasons li > span:not(.k-badge) {
		flex: 1 1 14ch;
		min-width: 0;
	}
	.latest {
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
	}

	@media (min-width: 721px) {
		.k-tr {
			gap: var(--k-space-3);
		}
	}
	/* Phone: name, status and dates take the full width; the six counts pair up. */
	@media (max-width: 720px) {
		.k-tr {
			grid-template-columns: repeat(2, minmax(0, 1fr));
			column-gap: var(--k-space-5);
			align-items: start;
		}
		.k-tr > .wide {
			grid-column: 1 / -1;
		}
		.k-tr > [data-label] {
			gap: var(--k-space-2);
		}
		/* A long caption may wrap instead of pushing its number out of the half column. */
		.k-tr > [data-label]::before {
			flex: 0 1 auto;
		}
		.badges {
			justify-content: flex-end;
		}
	}
</style>
