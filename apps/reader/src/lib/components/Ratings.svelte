<script lang="ts">
	// The two ratings of a series, shown apart so nobody has to guess whose a number is:
	//
	//  - ours: the yellow star. The average of the readers on this server.
	//  - MangaDex's: the orange star. Its number is one step quieter, because it is
	//    somebody else's, and so that colour is not the only thing telling them apart.
	//
	// Either can be missing. `own` takes the card's formatted string as well as a number,
	// and anything that is not above zero is "no rating": a series nobody rated arrives
	// as 0 (or "0.0"), and that must never print as a zero star.
	//
	// THE owner of the "hide MangaDex ratings" rule: the reader's own switch sets
	// `data-md-ratings="off"` on <html> (see $lib/theme.svelte) and the CSS below hides the
	// MangaDex part everywhere this component is used. It is CSS and not a prop so that a
	// server-rendered card is right on its first paint, before the app has started.
	import Icon from './Icon.svelte';

	let {
		own = null,
		md = null,
		chip = false,
		stack = false,
		unrated = false,
		starSize = 10,
	}: {
		own?: number | string | null;
		md?: number | null;
		/** The dark chip that sits on a cover. Off: plain text in the row's own colour. */
		chip?: boolean;
		/** One rating per line, numbers in a column. Off: side by side. */
		stack?: boolean;
		/** Say "Unrated" when there is no rating to show, instead of rendering nothing. */
		unrated?: boolean;
		starSize?: number;
	} = $props();

	/** "8.4", "10" (never "10.0": the fourth character is what overflows a narrow
	 *  cover), or '' for no rating. */
	function format(value: number | string | null | undefined): string {
		const n = Number(value);
		if (!Number.isFinite(n) || n <= 0) return '';
		return n >= 9.95 ? '10' : n.toFixed(1);
	}

	const ownText = $derived(format(own));
	const mdText = $derived(format(md));
</script>

{#if ownText || mdText || unrated}
	<span class="ratings" class:chip class:stack class:md-only={!ownText && !unrated}>
		{#if ownText}
			<span class="r" title="Rated by readers here">
				<span class="mark"><Icon name="star" size={starSize} fill="var(--k-star)" /></span>
				<span class="n"
					><span class="vh">Readers here: </span>{ownText}<span class="vh"> out of 10.</span></span
				>
			</span>
		{:else if unrated}
			<!-- With a MangaDex rating to show, that rating is the answer and this stays
			     hidden; it takes over only when the reader turned MangaDex ratings off. -->
			<span class="r none" class:md-off-only={!!mdText}>
				<span class="mark"><Icon name="star" size={starSize} fill="var(--k-star)" /></span>
				<span class="n">Unrated</span>
			</span>
		{/if}
		{#if mdText}
			<span class="r md" title="Rated on MangaDex">
				<span class="mark"><Icon name="star" size={starSize} fill="var(--k-star-mangadex)" /></span>
				<span class="n"
					><span class="vh">MangaDex: </span>{mdText}<span class="vh"> out of 10.</span></span
				>
			</span>
		{/if}
	</span>
{/if}

<style>
	.ratings {
		display: inline-flex;
		align-items: center;
		gap: 10px;
		font-weight: 700;
		font-variant-numeric: tabular-nums;
		line-height: 1.25;
		white-space: nowrap;
	}
	.r {
		display: inline-flex;
		align-items: center;
		gap: 3px;
	}
	.mark {
		display: inline-flex;
		justify-content: center;
		line-height: 0;
	}
	/* One step quieter than ours, and no further: at 0.8 the number still clears 4.5:1
	   on the light page. The star keeps its full colour, which is what names the source. */
	.md .n {
		opacity: 0.8;
	}

	/* One rating per line: the stars share a column and so do the numbers. */
	.stack {
		display: inline-grid;
		grid-template-columns: auto auto;
		align-items: center;
		gap: 1px 4px;
	}
	.stack .r {
		display: contents;
	}
	.stack .n {
		justify-self: end;
	}

	/* On a cover. Always the same dark chip with light text, in both themes: it sits on
	   artwork, not on the page. */
	.chip {
		color: #f7f6f3;
		font-size: 11.5px;
		background: rgba(12, 12, 13, 0.88);
		border-radius: 6px;
		padding: 3px 7px;
	}

	.r.md-off-only {
		display: none;
	}
	:global(:root[data-md-ratings='off']) .r.md,
	:global(:root[data-md-ratings='off']) .ratings.md-only {
		display: none;
	}
	:global(:root[data-md-ratings='off']) .r.md-off-only {
		display: inline-flex;
	}
	:global(:root[data-md-ratings='off']) .stack .r.md-off-only {
		display: contents;
	}

	/* Read out, not drawn: who the number is from. */
	.vh {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}
</style>
