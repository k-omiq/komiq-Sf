<script lang="ts">
	import { href } from '$lib/paths';
	import { serverAddress } from '$lib/config';
	import { loadExtensionRepos, loadExtensions, loadUsers } from '$lib/data';
	import Button from './Button.svelte';
	import Icon from './Icon.svelte';

	// The Catalog page's first-run card: four things to do on a new server, each
	// ticked from what the server really has. It opens the first time the catalogue
	// is seen empty and stays (so "Add a series" can tick too) until every line is
	// done or the owner dismisses it. The choice is remembered in this browser only.
	let {
		empty,
		hasSeries,
	}: {
		/** The unfiltered catalogue loaded and holds nothing. */
		empty: boolean;
		/** The unfiltered catalogue holds a series; null while not known. */
		hasSeries: boolean | null;
	} = $props();

	// Per server: the desktop app is one browser origin for every server it connects to.
	const KEY = `komiq-getting-started:${serverAddress()}`;
	type Stored = 'open' | 'closed' | null;

	function read(): Stored {
		try {
			const v = localStorage.getItem(KEY);
			return v === 'open' || v === 'closed' ? v : null;
		} catch {
			return null;
		}
	}
	function write(v: 'open' | 'closed'): void {
		try {
			localStorage.setItem(KEY, v);
		} catch {
			// Private mode: the card simply follows the empty catalogue.
		}
	}

	let stored = $state<Stored>(read());
	let closed = $state(false);
	// Each fact is null until its request answers; a failed one stays null (unticked).
	let hasRepo = $state<boolean | null>(null);
	let hasExtension = $state<boolean | null>(null);
	let hasReader = $state<boolean | null>(null);
	let asked = false;
	// The last answer about series: the prop goes null while the catalogue is loading,
	// searched or paged, and the tick must not flicker off for that.
	let seriesKnown = $state<boolean | null>(null);
	$effect(() => {
		if (hasSeries !== null) seriesKnown = hasSeries;
	});

	const open = $derived(!closed && stored !== 'closed' && (stored === 'open' || empty));

	$effect(() => {
		if (!open) return;
		if (stored !== 'open') {
			stored = 'open';
			write('open');
		}
		if (asked) return;
		asked = true;
		loadExtensionRepos().then(
			(repos) => (hasRepo = repos.length > 0),
			() => {},
		);
		loadExtensions().then(
			(list) => (hasExtension = list.some((e) => e.isInstalled)),
			() => {},
		);
		// The owner is one account; a second one is a reader.
		loadUsers().then(
			(users) => (hasReader = (users.total ?? users.items.length) > 1),
			() => {},
		);
	});

	const lines = $derived([
		{
			id: 'repo',
			done: hasRepo,
			label: 'Add an extension repo',
			hint: 'komiq ships with no sources. A repo is a list of extensions you choose to trust.',
			to: href('/settings#repos'),
		},
		{
			id: 'extension',
			done: hasExtension,
			label: 'Install an extension',
			hint: 'Each extension adds one or more sources to read from.',
			to: href('/sources/extensions'),
		},
		{
			id: 'series',
			done: seriesKnown,
			label: 'Add a series',
			hint: 'Browse a source and add what you want in the catalog.',
			to: href('/sources/browse'),
		},
		{
			id: 'reader',
			done: hasReader,
			label: 'Add a reader',
			hint: 'Give someone else an account. Each person keeps their own library.',
			to: href('/users'),
		},
	]);
	const doneCount = $derived(lines.filter((l) => l.done).length);

	// Everything ticked: the card has done its job and does not come back.
	$effect(() => {
		if (open && doneCount === lines.length) dismiss();
	});

	function dismiss(): void {
		closed = true;
		stored = 'closed';
		write('closed');
	}
</script>

{#if open}
	<section class="start" aria-labelledby="start-h">
		<header class="head">
			<div>
				<h2 id="start-h" class="title">Getting started</h2>
				<p class="k-hint">
					<span class="k-num">{doneCount}</span> of <span class="k-num">{lines.length}</span> done
				</p>
			</div>
			<Button variant="ghost" size="sm" icon="x" onclick={dismiss}>Dismiss</Button>
		</header>
		<ol class="lines">
			{#each lines as line (line.id)}
				<li>
					<a class="line" class:is-done={line.done} href={line.to}>
						<span class="mark">
							{#if line.done}
								<Icon name="check" size={15} />
							{:else}
								<span class="dot"></span>
							{/if}
							<span class="sr">{line.done ? 'Done:' : 'To do:'}</span>
						</span>
						<span class="text">
							<span class="label">{line.label}</span>
							<span class="hint">{line.hint}</span>
						</span>
						<span class="go"><Icon name="chevron-right" size={15} /></span>
					</a>
				</li>
			{/each}
		</ol>
	</section>
{/if}

<style>
	.start {
		display: grid;
		gap: var(--k-space-3);
		padding: 14px;
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius);
	}
	.head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: var(--k-space-3);
	}
	.title {
		margin: 0;
		font-size: var(--k-text-md);
		font-weight: 600;
		color: var(--k-text-1);
	}
	.lines {
		display: grid;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.line {
		display: grid;
		grid-template-columns: 20px minmax(0, 1fr) auto;
		align-items: start;
		gap: var(--k-space-3);
		padding: 10px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		color: inherit;
		text-decoration: none;
	}
	.line:hover {
		background: var(--k-surface-3);
	}
	.mark,
	.go {
		display: grid;
		place-items: center;
		height: 20px;
		color: var(--k-text-dim);
	}
	.line.is-done .mark {
		color: var(--k-success);
	}
	.dot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--k-surface-5);
	}
	.text {
		display: grid;
		gap: 2px;
		min-width: 0;
		padding-top: 1px;
	}
	.label {
		font-size: var(--k-text-sm);
		color: var(--k-text-1);
	}
	.line.is-done .label {
		color: var(--k-text-dim);
	}
	.hint {
		font-size: var(--k-text-xs);
		line-height: 1.5;
		color: var(--k-text-dim);
	}
	.sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
	}
</style>
