<script lang="ts">
	interface Metadata {
		year?: number | null;
		originalLanguage?: string | null;
		demographic?: string | null;
		contentRating?: string | null;
		tags?: { id: string | null; name: string; group: string }[];
		trackers?: { provider: string; externalId: string; url: string }[];
	}
	let { metadata }: { metadata?: Metadata | null } = $props();
	const providerNames: Record<string, string> = {
		al: 'AniList',
		anilist: 'AniList',
		mal: 'MyAnimeList',
		myanimelist: 'MyAnimeList',
		mu: 'MangaUpdates',
		mangaupdates: 'MangaUpdates',
		ap: 'Anime-Planet',
		animeplanet: 'Anime-Planet',
		kt: 'Kitsu',
		kitsu: 'Kitsu',
		nu: 'Novel Updates',
		novelupdates: 'Novel Updates',
		bw: 'BookWalker',
		bookwalker: 'BookWalker',
		md: 'MangaDex',
		mangadex: 'MangaDex',
		amz: 'Amazon',
		amazon: 'Amazon',
		ebj: 'ebookjapan',
		raw: 'Official Original',
		engtl: 'Official English',
	};
	function titleCase(value: string): string {
		return value.replace(/[_-]/g, ' ').replace(/\b\w/g, (letter) => letter.toUpperCase());
	}
	function languageName(language: string): string {
		try {
			return new Intl.DisplayNames(['en'], { type: 'language' }).of(language) ?? language;
		} catch {
			return language.toUpperCase();
		}
	}
	function safeTrackerUrl(value: string): string | null {
		try {
			const url = new URL(value);
			return url.protocol === 'https:' && !url.username && !url.password ? url.href : null;
		} catch {
			return null;
		}
	}
	const facts = $derived(
		[
			metadata?.year ? { label: 'Publication Year', value: String(metadata.year) } : null,
			metadata?.originalLanguage
				? { label: 'Original Language', value: languageName(metadata.originalLanguage) }
				: null,
			metadata?.demographic
				? { label: 'Demographic', value: titleCase(metadata.demographic) }
				: null,
			metadata?.contentRating
				? { label: 'Content Rating', value: titleCase(metadata.contentRating) }
				: null,
		].filter((fact) => fact !== null),
	);
	const groups = $derived.by(() => {
		const grouped = new Map<string, { id: string | null; name: string; group: string }[]>();
		const seen = new Set<string>();
		for (const tag of metadata?.tags ?? []) {
			const key = tag.id ?? `${tag.group}:${tag.name}`;
			if (!tag.name.trim() || seen.has(key)) continue;
			seen.add(key);
			const group = tag.group || 'Other';
			grouped.set(group, [...(grouped.get(group) ?? []), tag]);
		}
		const order = ['format', 'genre', 'theme', 'content'];
		return [...grouped.entries()].sort(([a], [b]) => {
			const rank = (group: string) => {
				const index = order.indexOf(group.toLowerCase());
				return index < 0 ? order.length : index;
			};
			return rank(a) - rank(b) || a.localeCompare(b);
		});
	});
	const trackers = $derived.by(() => {
		const seen = new Set<string>();
		return (metadata?.trackers ?? []).flatMap((tracker) => {
			const url = safeTrackerUrl(tracker.url);
			if (!url || seen.has(url)) return [];
			seen.add(url);
			return [
				{
					url,
					label: providerNames[tracker.provider.toLowerCase()] ?? titleCase(tracker.provider),
				},
			];
		});
	});
</script>

{#if facts.length || groups.length || trackers.length}
	<section class="metadata k-gutter" aria-label="Series metadata">
		<h2>Series Details</h2>
		{#if facts.length}
			<dl class="facts">
				{#each facts as fact (fact.label)}
					<div>
						<dt>{fact.label}</dt>
						<dd>{fact.value}</dd>
					</div>
				{/each}
			</dl>
		{/if}
		{#if groups.length}
			<div class="tag-groups">
				{#each groups as [group, tags] (group)}
					<div class="tag-group">
						<h3>{titleCase(group)}</h3>
						<ul class="tags">
							{#each tags as tag (tag.id ?? `${group}:${tag.name}`)}<li>{tag.name}</li>{/each}
						</ul>
					</div>
				{/each}
			</div>
		{/if}
		{#if trackers.length}
			<div class="tracker-group">
				<h3>Trackers &amp; Official Links</h3>
				<p>Find this series on other services.</p>
				<ul class="trackers">
					{#each trackers as tracker (tracker.url)}
						<li>
							<a
								href={tracker.url}
								target="_blank"
								rel="noopener noreferrer external"
								aria-label={`${tracker.label} (opens in a new tab)`}
								>{tracker.label}<span aria-hidden="true">↗</span></a
							>
						</li>
					{/each}
				</ul>
			</div>
		{/if}
	</section>
{/if}

<style>
	.metadata {
		padding-top: 28px;
		padding-bottom: 32px;
		margin-top: 24px;
		border-top: 1px solid var(--k-border-1);
		display: grid;
		gap: 24px;
	}
	h2 {
		font-size: 22px;
		color: var(--k-text);
	}
	h3 {
		font-family: var(--k-font-sans);
		font-size: 12px;
		font-weight: 700;
		color: var(--k-text-dim);
		letter-spacing: 0.04em;
	}
	.facts {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 20px;
	}
	.facts > div {
		min-width: 0;
	}
	dt {
		font-size: 12px;
		color: var(--k-text-dim);
		margin-bottom: 6px;
	}
	dd {
		font-size: 14px;
		font-weight: 600;
		color: var(--k-text-1);
		overflow-wrap: anywhere;
		font-variant-numeric: tabular-nums;
	}
	.tag-groups {
		display: grid;
		gap: 16px;
	}
	.tag-group {
		display: grid;
		grid-template-columns: 120px minmax(0, 1fr);
		gap: 16px;
		align-items: baseline;
	}
	.tags,
	.trackers {
		list-style: none;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.tags li {
		padding: 5px 10px;
		border-radius: var(--k-radius-sm);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-1);
		font-size: 12px;
		color: var(--k-text-2);
		overflow-wrap: anywhere;
		max-width: 100%;
	}
	.tracker-group {
		display: grid;
		gap: 10px;
	}
	.tracker-group p {
		font-size: 12px;
		color: var(--k-text-dim);
	}
	.trackers a {
		min-height: 40px;
		padding: 9px 12px;
		display: inline-flex;
		align-items: center;
		gap: 10px;
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius);
		font-size: 13px;
		font-weight: 600;
		color: var(--k-text-2);
		transition:
			color 0.15s,
			border-color 0.15s,
			background-color 0.15s;
	}
	.trackers a:hover {
		color: var(--k-text);
		border-color: var(--k-accent);
		background: var(--k-hover-fill);
	}
	.trackers a span {
		color: var(--k-text-dim);
	}
	@media (max-width: 640px) {
		.facts {
			grid-template-columns: repeat(2, minmax(0, 1fr));
			gap: 20px 16px;
		}
		.tag-group {
			grid-template-columns: 1fr;
			gap: 8px;
		}
		.trackers a {
			min-height: 44px;
		}
	}
</style>
