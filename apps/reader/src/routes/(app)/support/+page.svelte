<script lang="ts">
	import Icon from '$lib/components/Icon.svelte';
	import { type ComicType } from '$lib/data/types';

	let { data } = $props();
	const supportCategories = $derived(data.supportCategories);
	const faqs = $derived(data.faqs);

	let query = $state('');
	let open = $state<Record<string, boolean>>({});

	const filtered = $derived.by(() => {
		const q = query.trim().toLowerCase();
		if (!q) return faqs;
		return faqs.filter((f) => f.q.toLowerCase().includes(q) || f.a.toLowerCase().includes(q));
	});
	const subtitle = $derived(
		query.trim()
			? `${filtered.length} result${filtered.length === 1 ? '' : 's'} for "${query.trim()}"`
			: 'Quick answers to the things readers ask most.',
	);

	const catIcon: Record<string, ComicType | 'user' | 'book' | 'card' | 'flag'> = {
		Account: 'user',
		Reading: 'book',
		Donations: 'card',
		'Report an issue': 'flag',
	};

	const contacts = [
		{
			icon: 'mail',
			title: 'Email us',
			desc: 'help@komiq.app — we answer within one business day.',
			bg: 'rgba(224,179,84,0.14)',
			color: 'var(--k-hiatus)',
			note: 'Include your account email',
		},
	] as const;
</script>

<section class="hero k-gutter">
	<div class="hero-badge"><Icon name="help" size={28} /></div>
	<h1>How can we help?</h1>
	<div class="searchbar">
		<Icon name="search" size={20} stroke="#87857f" />
		<input bind:value={query} placeholder="Search help articles…" />
	</div>
</section>

<section class="cats k-gutter">
	<div class="cats-grid">
		{#each supportCategories as c (c.title)}
			<div class="cat">
				<div class="cat-icon" style="background:{c.iconBg};color:{c.iconColor}">
					<Icon name={catIcon[c.title] as never} size={20} />
				</div>
				<span class="cat-title">{c.title}</span>
				<span class="cat-desc">{c.desc}</span>
			</div>
		{/each}
	</div>
</section>

<section class="faq-wrap k-gutter">
	<div class="faq">
		<h2>Frequently asked</h2>
		<p class="faq-sub">{subtitle}</p>
		<div class="faq-list">
			{#each filtered as f (f.q)}
				<div class="faq-item">
					<button class="faq-q" onclick={() => (open = { ...open, [f.q]: !open[f.q] })}>
						<span>{f.q}</span>
						<span class="chev" class:up={open[f.q]}><Icon name="chevron-down" size={18} /></span>
					</button>
					{#if open[f.q]}<p class="faq-a">{f.a}</p>{/if}
				</div>
			{/each}
			{#if query.trim() && filtered.length === 0}
				<div class="faq-empty">
					No articles match "{query.trim()}". Try another search or contact us below.
				</div>
			{/if}
		</div>
	</div>
</section>

<section class="contact-wrap k-gutter">
	<div class="contacts">
		{#each contacts as c (c.title)}
			<div class="contact">
				<div class="contact-icon" style="background:{c.bg};color:{c.color}">
					<Icon name={c.icon} size={20} />
				</div>
				<span class="contact-title">{c.title}</span>
				<span class="contact-desc">{c.desc}</span>
				<span class="contact-note">{c.note}</span>
			</div>
		{/each}
	</div>
</section>

<style>
	.hero {
		padding-top: 72px;
		padding-bottom: 8px;
		display: flex;
		flex-direction: column;
		align-items: center;
		text-align: center;
		gap: 22px;
	}
	.hero-badge {
		width: 60px;
		height: 60px;
		border-radius: 16px;
		background: rgba(255, 255, 255, 0.05);
		border: 1px solid var(--k-border-3);
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--k-text);
	}
	.hero h1 {
		font-size: 40px;
		line-height: 1.05;
		letter-spacing: -0.03em;
		margin: 0;
		color: var(--k-text-bright);
	}
	.searchbar {
		width: 100%;
		max-width: 600px;
		display: flex;
		align-items: center;
		gap: 14px;
		height: 56px;
		padding: 0 22px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-3);
		border-radius: 12px;
		transition: border-color 0.15s;
	}
	.searchbar:focus-within {
		border-color: var(--k-border-strong);
	}
	.searchbar input {
		flex: 1;
		min-width: 0;
		background: transparent;
		border: none;
		outline: none;
		color: var(--k-text);
		font-size: 16px;
	}
	.cats {
		padding-top: 44px;
	}
	.cats-grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(230px, 1fr));
		gap: 20px;
		max-width: 1000px;
		margin: 0 auto;
	}
	.cat {
		display: flex;
		flex-direction: column;
		gap: 12px;
		border: 1px solid var(--k-border-1);
		border-radius: 14px;
		padding: 24px;
		background: var(--k-surface);
		cursor: pointer;
		transition: all 0.15s;
	}
	.cat:hover {
		border-color: rgba(255, 255, 255, 0.22);
		background: #131313;
	}
	.cat-icon {
		width: 42px;
		height: 42px;
		border-radius: 11px;
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.cat-title {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 16px;
		color: var(--k-text);
	}
	.cat-desc {
		font-size: 13px;
		color: var(--k-text-dimmer);
		line-height: 1.5;
	}
	.faq-wrap,
	.contact-wrap {
		padding-top: 64px;
		display: flex;
		justify-content: center;
	}
	.faq {
		width: 100%;
		max-width: 760px;
	}
	.faq h2 {
		font-size: 24px;
		margin: 0 0 6px;
		color: var(--k-text-bright);
	}
	.faq-sub {
		font-size: 14px;
		color: var(--k-text-dimmer);
		margin: 0 0 24px;
	}
	.faq-list {
		border: 1px solid var(--k-border-1);
		border-radius: 14px;
		overflow: hidden;
		background: var(--k-surface);
	}
	.faq-item {
		border-bottom: 1px solid var(--k-border);
	}
	.faq-q {
		width: 100%;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 16px;
		padding: 20px 24px;
		cursor: pointer;
		background: none;
		border: none;
		text-align: left;
		font-weight: 700;
		font-size: 15px;
		color: var(--k-text-1);
	}
	.chev {
		flex: 0 0 auto;
		color: var(--k-text-dimmer);
		transition: transform 0.2s;
	}
	.chev.up {
		transform: rotate(180deg);
	}
	.faq-a {
		margin: 0;
		padding: 0 24px 22px;
		font-size: 14px;
		line-height: 1.65;
		color: #a9a7a1;
		max-width: 640px;
	}
	.faq-empty {
		padding: 40px 20px;
		text-align: center;
		color: var(--k-text-dimmer);
		font-size: 14px;
	}
	.contacts {
		width: 100%;
		max-width: 760px;
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
		gap: 18px;
	}
	.contact {
		border: 1px solid var(--k-border-1);
		border-radius: 14px;
		padding: 24px;
		background: var(--k-surface);
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.contact-icon {
		width: 40px;
		height: 40px;
		border-radius: 10px;
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.contact-title {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 16px;
		color: var(--k-text);
	}
	.contact-desc {
		font-size: 13px;
		color: var(--k-text-dimmer);
		line-height: 1.5;
	}
	.contact-note {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: 12.5px;
		color: var(--k-text-fainter);
		margin-top: 2px;
		font-weight: 600;
	}
</style>
