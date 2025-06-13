<script lang="ts">
	import { tick, untrack } from 'svelte';
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import Icon from '$lib/components/Icon.svelte';
	import { auth } from '$lib/auth.svelte';
	import { REPORT_TOPICS, topicFor } from '$lib/data/content';
	import { submitReport } from '$lib/data/source';
	import type { Report, ReportKind } from '@komika/types';

	// The topic can arrive as ?kind= from the Support page or a series page's report link;
	// anything unrecognised falls through to the catch-all rather than 404ing a reader who
	// followed a stale link.
	const KINDS: string[] = REPORT_TOPICS.map((t) => t.kind);
	const urlKind = $derived.by<ReportKind>(() => {
		const k = page.url.searchParams.get('kind')?.toUpperCase();
		return k && KINDS.includes(k) ? (k as ReportKind) : 'OTHER';
	});

	let kind = $state<ReportKind>('OTHER');

	/**
	 * URL → state. Arriving at `?kind=NEEDS_MERGE` selects it, and so does going Back or
	 * Forward onto such a URL.
	 *
	 * `untrack` is load-bearing: reading `kind` here would make this effect depend on the
	 * very thing it writes, so every click on the picker would re-run it and slam `kind`
	 * back to whatever the address bar last said. Its only dependency is `urlKind`.
	 */
	$effect(() => {
		const next = urlKind;
		untrack(() => {
			if (next === kind) return;
			kind = next;
			// A validation message about the topic we just left is noise on the new one.
			error = null;
		});
	});

	const topic = $derived(topicFor(kind));

	// A series link OR a bare id is accepted, because the reader is coming from a page
	// whose URL is the thing they have to hand. `/series/w_123?x=1#y` → `w_123`.
	function parseSeriesId(input: string): string {
		const v = input.trim();
		if (!v) return '';
		const m = v.match(/\/(?:series|read)\/([^/?#]+)/);
		return m ? decodeURIComponent(m[1]) : v;
	}

	let subject = $state('');
	let secondary = $state('');
	let title = $state('');
	let username = $state('');
	let detail = $state('');
	let sourceUrl = $state('');

	/**
	 * The exact comment being reported, from `?comment=` on the Report link under a
	 * comment. Never typed: a reader has no way to see a comment's id, so if this is
	 * empty the report falls back to a username they type themselves.
	 *
	 * It matters because the server treats the two very differently — given an id it
	 * looks the comment up, snapshots the body, and takes the author from the DB rather
	 * than from the client, so the report stays intact and attributable even after the
	 * comment is deleted. A typed username gives an admin a name and nothing else.
	 */
	const commentId = $derived(page.url.searchParams.get('comment') ?? '');
	/** Display-only, from `?by=` — the server re-derives the real author from the id. */
	const reportedName = $derived(page.url.searchParams.get('by') ?? '');

	/**
	 * Prefilled series id, from `?series=` on a Report link on a series page. It goes
	 * through the same `subject` field a reader can type into, so it is visible and
	 * editable rather than a hidden value they can't see or correct.
	 *
	 * `untrack` for the same reason as the topic effect above: this writes `subject`, so
	 * reading it here would make every keystroke re-run the effect and revert the edit.
	 *
	 * Keyed on the param VALUE rather than on `subject` being empty. `?series=` outlives
	 * the first render — picking a topic rewrites the query string, which re-runs this —
	 * so an "only fill when empty" test would quietly re-stuff the field a reader had
	 * just cleared because we had guessed the wrong series. Keying on the value fills
	 * once per distinct link, and still follows a genuinely new one.
	 */
	let prefilledFrom = '';
	$effect(() => {
		const fromUrl = page.url.searchParams.get('series') ?? '';
		untrack(() => {
			if (!fromUrl || fromUrl === prefilledFrom) return;
			prefilledFrom = fromUrl;
			subject = fromUrl;
		});
	});

	let busy = $state(false);
	let error = $state<string | null>(null);
	let done = $state<Report | null>(null);

	/** The radio buttons, for roving focus. Indexes match `REPORT_TOPICS`. */
	const pickEls = $state<(HTMLButtonElement | null)[]>([]);
	let sentEl = $state<HTMLDivElement | null>(null);

	// Mirrors REPORT_DETAIL_MIN / REPORT_DETAIL_MAX in the server's `submitReport`.
	const DETAIL_MAX = 4000;
	const DETAIL_MIN = 10;
	// Counted RAW, not trimmed, because `maxlength` below is what actually stops typing
	// and it counts raw too — a trimmed counter would read "12 left" while the field
	// refused the next keystroke.
	const detailLeft = $derived(DETAIL_MAX - detail.length);

	/**
	 * Rewrite this page's query string in place.
	 *
	 * `goto`, NOT `$app/navigation`'s `replaceState`. The shallow-routing helper rewrites
	 * the address bar and `page.state` but deliberately never touches `page.url` — and
	 * `?comment=`, `?by=` and `?series=` are read straight off `page.url`. With a shallow
	 * replace, "Not this one" and "Report something else" would strip the params from the
	 * address bar while the form went on attaching the same comment, and every later edit
	 * would rebuild the URL from a `page.url` frozen at page load, resurrecting exactly
	 * what the previous edit deleted.
	 *
	 * `replaceState: true` so Back still leaves the page instead of walking five topic
	 * picks; `keepFocus` so the roving focus set by the arrow keys survives the
	 * navigation; `noScroll` so the reader isn't yanked to the top mid-form.
	 */
	function editUrl(mutate: (params: URLSearchParams) => void): void {
		const url = new URL(page.url);
		mutate(url.searchParams);
		if (url.href === page.url.href) return;
		void goto(url, { replaceState: true, noScroll: true, keepFocus: true }).catch(() => {
			/* the address bar just doesn't follow; the state set by the caller stands */
		});
	}

	function pickTopic(k: ReportKind): void {
		if (k === kind) return;
		kind = k;
		error = null;
		// Keep the address bar honest: the topic is the only thing that changes what this
		// page is, so a reload or a shared link must come back to the same form.
		editUrl((p) => p.set('kind', k));
	}

	/**
	 * Detach the pinned comment. The attachment is URL-derived, so this is the only way
	 * to get rid of one — and it has to exist: if the comment was deleted between the
	 * reader clicking Report and pressing Send, the server refuses the id and the pinned
	 * notice would otherwise leave them re-sending the same doomed report forever.
	 */
	function unpinComment(): void {
		username = username || reportedName;
		error = null;
		editUrl((p) => {
			p.delete('comment');
			p.delete('by');
		});
	}

	/** Arrow keys move between radios and select as they go, per the radiogroup pattern. */
	function onPickKey(e: KeyboardEvent, i: number): void {
		const last = REPORT_TOPICS.length - 1;
		let next: number;
		if (e.key === 'ArrowRight' || e.key === 'ArrowDown') next = i === last ? 0 : i + 1;
		else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') next = i === 0 ? last : i - 1;
		else if (e.key === 'Home') next = 0;
		else if (e.key === 'End') next = last;
		else return;
		e.preventDefault();
		pickTopic(REPORT_TOPICS[next].kind);
		pickEls[next]?.focus();
	}

	async function reset(): Promise<void> {
		done = null;
		error = null;
		subject = '';
		secondary = '';
		title = '';
		username = '';
		detail = '';
		sourceUrl = '';
		// Re-arm the prefill, so a reader who comes back through a Report link on a
		// different series still gets that series filled in.
		prefilledFrom = '';
		// The one-shot params describe the report that was JUST filed, and two of the three
		// are read straight off the URL every render — so leaving them would silently staple
		// the same comment (and re-prefill the same series) to a report the reader started
		// by asking for something else. Clearing the fields is not enough; the URL has to go
		// with them. The topic is deliberately kept: it is what the reader chose, not what
		// they were handed.
		editUrl((p) => {
			p.delete('comment');
			p.delete('by');
			p.delete('series');
		});
		// The success panel that held focus has just been destroyed; without this, focus
		// falls back to <body> and a keyboard or screen-reader user is dumped at the top
		// of the document with no idea the form came back.
		await tick();
		pickEls[REPORT_TOPICS.findIndex((t) => t.kind === kind)]?.focus();
	}

	// Same problem in the other direction: the form is replaced by the panel, so move
	// focus into it. That also announces it, which a live region on a node inserted in
	// the same frame would not reliably do.
	$effect(() => {
		if (done) sentEl?.focus();
	});

	/**
	 * The server's rejections are written for readers and are shown verbatim. Transport
	 * failures are NOT: `fetch` rejects with a TypeError ("Failed to fetch") and the API
	 * client turns any non-2xx into "Backend error 502". Those two shapes — and only
	 * those — get replaced, so a reader never sees an engineer's sentence.
	 */
	function readerMessage(err: unknown): string {
		if (!(err instanceof Error)) return 'Could not send that. Please try again.';
		if (err instanceof TypeError) {
			return "Couldn't reach komiq. Check your connection and try again — nothing was sent.";
		}
		if (/^Backend error \d+$/.test(err.message) || err.message === 'Backend returned no data') {
			return 'Something went wrong at our end and the report was not saved. Please try again in a moment.';
		}
		return err.message || 'Could not send that. Please try again.';
	}

	async function submit(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (busy) return;
		error = null;
		busy = true;
		try {
			done = await submitReport({
				kind,
				detail,
				// Only send what this topic actually asks for, so a value left over from a
				// previously selected topic can't ride along. Every field the form can show
				// is listed: `sourceUrl` and `detail` are the only two every topic asks for.
				subjectId: topic.needs.subject ? parseSeriesId(subject) || null : null,
				subjectIdSecondary: topic.needs.secondary ? parseSeriesId(secondary) || null : null,
				subjectTitle: topic.needs.title ? title.trim() || null : null,
				// The id wins when we have it; the typed name is only the fallback for a
				// reader who reached this page without one.
				commentId: topic.needs.user ? commentId || null : null,
				reportedUsername: topic.needs.user && !commentId ? username.trim() || null : null,
				sourceUrl: sourceUrl.trim() || null,
			});
			window.scrollTo({ top: 0, behavior: 'smooth' });
		} catch (err) {
			error = readerMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head>
	<title>Report an issue · komiq</title>
	<meta name="robots" content="noindex" />
</svelte:head>

<div class="wrap k-gutter">
	<a class="back" href="/support#report"><Icon name="chevron-left" size={16} /> Support</a>

	{#if done}
		<!-- `tabindex="-1"` so `submit` can move focus here. The panel REPLACES the form, so
		     without it focus falls back to <body>: a screen-reader user is told nothing and a
		     keyboard user's next Tab starts again from the top of the document. -->
		<div class="card sent" bind:this={sentEl} tabindex="-1" role="status">
			<div class="sent-icon"><Icon name="check" size={26} /></div>
			<h1>Report sent</h1>
			<p class="sub">
				Thanks — this is in the moderation queue and a human reads it. We can't reply: komiq has no
				support inbox and sends no status emails. If you end up reporting the same thing again,
				quoting this reference saves us untangling the duplicate.
			</p>
			<code class="ref">{done.id}</code>
			<div class="sent-actions">
				<button class="btn-secondary" onclick={reset}>Report something else</button>
				<a class="btn-secondary" href="/">Back to reading</a>
			</div>
		</div>
	{:else}
		<h1>Report an issue</h1>
		<p class="sub">
			Pick what's wrong. Everything below goes to a moderation queue — not a public thread.
		</p>

		<!-- Roving tabindex: the GROUP is one tab stop and the arrow keys move within it,
		     which is the radiogroup pattern. Five separate tab stops would be the wrong
		     shape for five mutually-exclusive choices. -->
		<div class="picker" role="radiogroup" aria-label="What are you reporting?">
			{#each REPORT_TOPICS as t, i (t.kind)}
				<button
					bind:this={pickEls[i]}
					type="button"
					role="radio"
					aria-checked={kind === t.kind}
					tabindex={kind === t.kind ? 0 : -1}
					class="pick"
					class:on={kind === t.kind}
					onclick={() => pickTopic(t.kind)}
					onkeydown={(e) => onPickKey(e, i)}
				>
					<span class="pick-title">{t.title}</span>
					<span class="pick-desc">{t.desc}</span>
				</button>
			{/each}
		</div>

		<form class="card" onsubmit={submit}>
			<h2>{topic.heading}</h2>
			<!-- With a comment already attached there is no username field on screen, so the
			     default prompt would be asking for something the form no longer shows. -->
			<p class="prompt">
				{commentId && topic.needs.user ? (topic.promptAttached ?? topic.prompt) : topic.prompt}
			</p>

			{#if topic.needs.subject}
				<label>
					<span>
						Series link or id
						{#if topic.needs.subject === 'optional'}<em>optional</em>{/if}
					</span>
					<input
						bind:value={subject}
						placeholder="https://komiq.cc/series/…"
						autocomplete="off"
						required={topic.needs.subject === 'required'}
					/>
					<small>Paste the address bar from the series page — we'll pull the id out of it.</small>
				</label>
			{/if}

			{#if topic.needs.secondary}
				<label>
					<span>
						The other entry
						{#if topic.needs.secondary === 'optional'}<em>optional</em>{/if}
					</span>
					<input
						bind:value={secondary}
						placeholder="https://komiq.cc/series/…"
						autocomplete="off"
						required={topic.needs.secondary === 'required'}
					/>
					<small>The duplicate. If you can't find it, describe it below instead.</small>
				</label>
			{/if}

			{#if topic.needs.title}
				<label>
					<span>
						Series title
						{#if topic.needs.title === 'optional'}<em>optional</em>{/if}
					</span>
					<input
						bind:value={title}
						placeholder="Exact title, romanised or original"
						required={topic.needs.title === 'required'}
					/>
				</label>
			{/if}

			{#if topic.needs.user}
				{#if commentId}
					<!-- Arrived from a Report link on the comment itself, so the id is exact and
					     the server will look the author up. Asking the reader to retype a
					     username here could only introduce a wrong one. -->
					<div class="pinned">
						<Icon name="comment" size={16} />
						<span>
							Reporting a specific comment{reportedName ? ` by ${reportedName}` : ''}. We've
							attached it — just tell us what's wrong below.
						</span>
						<button type="button" class="unpin" onclick={unpinComment}>Not this one</button>
					</div>
				{:else}
					<label>
						<span>
							Who are you reporting?
							{#if topic.needs.user === 'optional'}<em>optional</em>{/if}
						</span>
						<input
							bind:value={username}
							placeholder="Their username"
							autocomplete="off"
							required={topic.needs.user === 'required'}
						/>
						<small>
							Next time, the <strong>Report</strong> link under the comment itself attaches it for you
							— that's the fastest way for us to act on it.
						</small>
					</label>
				{/if}
			{/if}

			<label>
				<span>What's wrong?</span>
				<textarea
					bind:value={detail}
					rows="6"
					maxlength={DETAIL_MAX}
					minlength={DETAIL_MIN}
					required
					placeholder="Be as specific as you can — which chapters, which titles, what you saw."
				></textarea>
				<small class:warn={detailLeft < 200}>{detailLeft} characters left</small>
			</label>

			<label>
				<span>A link that helps <em>optional</em></span>
				<input
					bind:value={sourceUrl}
					placeholder="A page elsewhere that shows the correct information"
					autocomplete="off"
				/>
			</label>

			{#if error}
				<div class="error" role="alert">{error}</div>
			{/if}

			<div class="actions">
				<button class="btn-primary" type="submit" disabled={busy}>
					{busy ? 'Sending…' : 'Send report'}
				</button>
				<!-- Nothing until the session has resolved: this page is client-only, so the
				     first frame always has `auth.user === null`, and flashing "Filing
				     anonymously" at a signed-in reader is a claim about their privacy that
				     is about to become false. -->
				<span class="who">
					{#if !auth.ready}
						&nbsp;
					{:else if auth.user}
						Filed as <strong>{auth.user.username}</strong>.
					{:else}
						Filing anonymously — no account needed.
					{/if}
				</span>
			</div>
		</form>
	{/if}
</div>

<style>
	.wrap {
		max-width: 720px;
		margin: 0 auto;
		padding-top: 40px;
		padding-bottom: 90px;
	}
	.back {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		font-size: 13.5px;
		font-weight: 700;
		color: var(--k-text-dimmer);
		text-decoration: none;
		margin-bottom: 28px;
	}
	.back:hover {
		color: var(--k-text);
	}
	h1 {
		font-size: 36px;
		line-height: 1.08;
		letter-spacing: -0.03em;
		margin: 0 0 12px;
		color: var(--k-text-bright);
	}
	.sub {
		margin: 0 0 30px;
		font-size: 15.5px;
		line-height: 1.6;
		color: var(--k-text-dim);
	}
	.picker {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
		gap: 10px;
		margin-bottom: 26px;
	}
	.pick {
		display: flex;
		flex-direction: column;
		gap: 6px;
		text-align: left;
		padding: 15px 17px;
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface);
		cursor: pointer;
	}
	.pick:hover {
		border-color: var(--k-border-strong);
	}
	/* The group is a single tab stop reached by keyboard alone, so the ring is the only
	   thing telling a sighted keyboard user where they are. */
	.pick:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
	}
	.pick.on {
		border-color: var(--k-accent);
		background: rgba(224, 131, 105, 0.08);
	}
	.pick-title {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 14.5px;
		color: var(--k-text-bright);
	}
	.pick-desc {
		font-size: 12.5px;
		line-height: 1.5;
		color: var(--k-text-dimmer);
	}
	.card {
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius-xl);
		background: var(--k-surface);
		padding: 30px;
		display: flex;
		flex-direction: column;
		gap: 20px;
	}
	.card h2 {
		font-size: 20px;
		margin: 0;
		color: var(--k-text-bright);
	}
	.prompt {
		margin: -12px 0 0;
		font-size: 14px;
		line-height: 1.6;
		color: var(--k-text-dim);
	}
	label {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	label > span {
		font-size: 13px;
		font-weight: 700;
		color: var(--k-text-2);
	}
	label em {
		font-style: normal;
		font-weight: 600;
		color: var(--k-text-fainter);
		margin-left: 6px;
	}
	input,
	textarea {
		width: 100%;
		padding: 13px 15px;
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		color: var(--k-text);
		font-family: inherit;
		font-size: 15px;
		line-height: 1.55;
		outline: none;
		resize: vertical;
	}
	/* `outline: none` above kills the UA ring, so put an explicit one back — a border that
	   goes one shade lighter is not a focus indicator. */
	input:focus,
	textarea:focus {
		border-color: var(--k-accent);
		box-shadow: 0 0 0 2px rgba(224, 131, 105, 0.28);
	}
	small {
		font-size: 12.5px;
		line-height: 1.5;
		color: var(--k-text-fainter);
	}
	small.warn {
		color: var(--k-hiatus);
	}
	/* "We already have the thing you're reporting" — an attached comment, not an input. */
	.pinned {
		display: flex;
		align-items: flex-start;
		/* Wraps so "Not this one" drops to its own line instead of squeezing the sentence
		   into a two-word column on a phone. */
		flex-wrap: wrap;
		gap: 10px;
		padding: 14px 16px;
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		font-size: 13.5px;
		line-height: 1.55;
		color: var(--k-text-2);
	}
	.pinned :global(svg) {
		flex: 0 0 auto;
		margin-top: 2px;
		color: var(--k-text-dimmer);
	}
	.pinned span {
		flex: 1 1 220px;
	}
	.unpin {
		flex: 0 0 auto;
		padding: 0;
		border: none;
		background: none;
		font-family: inherit;
		font-size: 13px;
		font-weight: 700;
		color: var(--k-accent);
		text-decoration: underline;
		cursor: pointer;
	}
	.error {
		padding: 13px 16px;
		border: 1px solid rgba(224, 138, 138, 0.4);
		border-radius: var(--k-radius);
		background: rgba(224, 138, 138, 0.1);
		font-size: 14px;
		line-height: 1.55;
		color: var(--k-cancelled);
	}
	.actions {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 16px;
	}
	.btn-primary {
		height: 46px;
		padding: 0 26px;
		border: none;
		border-radius: var(--k-radius);
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-weight: 700;
		font-size: 15px;
		cursor: pointer;
	}
	.btn-primary:hover:not(:disabled) {
		background: var(--k-primary-hover);
	}
	.btn-primary:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.who {
		font-size: 13px;
		color: var(--k-text-fainter);
	}
	.who strong {
		color: var(--k-text-dim);
	}
	/* --- sent --- */
	.sent {
		align-items: flex-start;
		text-align: left;
	}
	/* Focused only so it is announced (see the template); a ring on a whole panel a mouse
	   user never asked for would just look like a rendering bug. */
	.sent:focus {
		outline: none;
	}
	.sent-icon {
		width: 46px;
		height: 46px;
		border-radius: 13px;
		display: flex;
		align-items: center;
		justify-content: center;
		background: rgba(127, 211, 154, 0.14);
		color: var(--k-ongoing);
	}
	.sent h1 {
		margin: 0;
		font-size: 28px;
	}
	.sent .sub {
		margin: -8px 0 0;
	}
	.ref {
		font-family: var(--k-font-mono);
		font-size: 13px;
		padding: 10px 14px;
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		color: var(--k-text-2);
		word-break: break-all;
	}
	.sent-actions {
		display: flex;
		flex-wrap: wrap;
		gap: 12px;
	}
	.btn-secondary {
		display: inline-flex;
		align-items: center;
		height: 42px;
		padding: 0 20px;
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius);
		background: transparent;
		font-family: inherit;
		font-weight: 700;
		font-size: 14px;
		color: var(--k-text);
		text-decoration: none;
		cursor: pointer;
	}
	.btn-secondary:hover {
		border-color: var(--k-border-strong);
		color: var(--k-text-bright);
	}
	@media (max-width: 640px) {
		.wrap {
			padding-top: 26px;
		}
		h1 {
			font-size: 28px;
		}
		.picker {
			grid-template-columns: 1fr;
		}
		.card {
			padding: 22px;
		}
	}
</style>
