<script lang="ts">
	// The komiq "k", in the reader's accent colour (`--k-mark`, see $lib/theme.css). The
	// image is only its shape: a mask over a fill, so any colour works and the edges
	// stay smooth. Sized by the surrounding font, like a letter of the wordmark.
	import mark from '$lib/assets/mark.png';
</script>

<span class="brand-mark" style:--mark="url({mark})" aria-hidden="true"></span>

<style>
	.brand-mark {
		display: inline-block;
		/* The image is 66 × 96. */
		height: 0.8em;
		aspect-ratio: 66 / 96;
		margin-right: 1.5px;
		transform: translateY(0.07em);
		background: var(--k-mark);
		-webkit-mask: var(--mark) center / contain no-repeat;
		mask: var(--mark) center / contain no-repeat;
		transition: background var(--k-dur) var(--k-ease);
	}
</style>
