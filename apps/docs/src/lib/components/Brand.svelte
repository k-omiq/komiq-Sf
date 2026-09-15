<script lang="ts">
	import mark from '$lib/assets/mark.png';

	let {
		size = 20,
	}: {
		/** Font size of the wordmark in px; the mark scales with it. */
		size?: number;
	} = $props();
</script>

<!-- The same lockup as the reader's header: the coral mark IS the "k", so the
     text beside it is "omiq". Keep the two in step. -->
<span class="brand" style:font-size="{size}px" translate="no">
	<img class="brand-mark" width="32" height="32" src={mark} alt="" />
	<span>omiq</span>
</span>

<style>
	.brand {
		display: inline-flex;
		align-items: baseline;
		gap: 0;
		font-family: var(--k-font-display);
		font-weight: 700;
		letter-spacing: -0.01em;
		color: var(--k-text);
	}
	.brand-mark {
		width: auto;
		height: 0.8em;
		margin-right: 1.5px;
		transform: translateY(0.07em);
	}
</style>
